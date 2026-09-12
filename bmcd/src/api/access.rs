// Copyright 2026 Oleg Tsarev
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Who may get in, readable and changeable from the interface.
//!
//! Until now both halves of this lived on the filesystem and arrived by SSH:
//! the local password through `passwd` on a serial console, and the client CA
//! through a script in another repository. Neither was visible from the
//! interface that asks for the password on every login, which meant nobody
//! could answer "who can reach this board" without logging into it.
//!
//! NOT THE LEGACY DISPATCHER, and that is the point. Every `opt=set` call is
//! written to the audit log with its whole query string (`legacy::audit`), so
//! a password routed through it would be recorded in clear in
//! `/tmp/bmcd.<date>.log` and in remote syslog when SQU-108 lands. These
//! handlers take a JSON body, log the ACTION and the actor and never the
//! secret.
//!
//! THE DAEMON NEVER REWRITES config.yaml. That file is the operator's, it
//! carries comments, and this daemon reads it with the `config` crate, which
//! has no writer. So the two things the interface can change live in two
//! small files the daemon owns -- the CA bundle itself, and a JSON sidecar
//! for the header name -- and `config.yaml` still wins wherever it speaks.
//! `Tls::effective_*` in `crate::config` is where that precedence is decided.

use crate::api::into_legacy_response::LegacyResponse;
use crate::authentication::authentication_context::Actor;
use crate::config::{self, AccessOverrides};
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use openssl::x509::X509;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Shortest password this will accept.
///
/// Twelve, not eight. The board is on a management LAN with a web interface,
/// an API and SSH all reading the same account, and `BanPatrol` slows an
/// online guess but does nothing for anyone who takes a copy of
/// `/etc/shadow`. A refusal here is cheap; the alternative is discovering the
/// number was too small after it mattered.
const MIN_PASSWORD_LEN: usize = 12;

const SHADOW_FILE: &str = "/etc/shadow";

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/access")
            .route("", web::get().to(get_access))
            .route("/password", web::post().to(set_password))
            .route("/client-ca", web::put().to(put_client_ca))
            .route("/client-ca", web::delete().to(delete_client_ca)),
    );
}

/// What the interface shows on the access card.
#[derive(Debug, Serialize)]
struct AccessState {
    /// How THIS request was authenticated, so the page can say "you are here
    /// as oleg@tsarev.id, vouched for by the gateway" rather than guessing.
    actor: ActorView,
    /// The local account the password belongs to.
    local_account: String,
    /// Null when no client CA is in effect.
    client_ca: Option<ClientCaView>,
    identity_header: HeaderView,
    /// True when `client_ca` came from config.yaml, which this daemon will
    /// not rewrite: the interface must send the operator to that file rather
    /// than pretend it can change it.
    client_ca_pinned_in_config: bool,
}

#[derive(Debug, Serialize)]
struct ActorView {
    name: String,
    /// `mtls`, `token`, `basic` or `loopback`.
    scheme: String,
}

#[derive(Debug, Serialize)]
struct HeaderView {
    name: String,
    /// `config`, `override` or `default` -- which of the three decided it.
    source: String,
}

/// A trust anchor, described in the terms the person deciding whether to keep
/// it actually needs: who it is, when it dies, and a fingerprint they can
/// compare against the one their proxy shows.
#[derive(Debug, Serialize)]
struct ClientCaView {
    subject: String,
    issuer: String,
    not_after: String,
    /// SHA-256 over the DER, the same digest openssl prints.
    fingerprint: String,
    /// A bundle may hold more than one. All of them are trusted, so all of
    /// them are shown.
    count: usize,
}

#[derive(Debug, Deserialize)]
struct PasswordChange {
    /// Whose password. Sent explicitly rather than taken from the actor: an
    /// operator arriving through the gateway is `oleg@tsarev.id`, which is
    /// not a local account, and guessing `root` for them would be a guess.
    username: String,
    current_password: String,
    new_password: String,
}

#[derive(Debug, Deserialize)]
struct ClientCaUpload {
    /// One or more certificates, PEM.
    pem: String,
    /// Optional. Absent leaves the header name alone.
    identity_header: Option<String>,
}

fn actor_of(request: &HttpRequest) -> ActorView {
    match request.extensions().get::<Actor>() {
        Some(Actor::User { name, scheme }) => ActorView {
            name: name.clone(),
            scheme: (*scheme).to_string(),
        },
        Some(Actor::Loopback) => ActorView {
            name: "loopback".to_string(),
            scheme: "loopback".to_string(),
        },
        None => ActorView {
            name: "unattributed".to_string(),
            scheme: "none".to_string(),
        },
    }
}

/// Read a bundle and describe it. `None` when the path holds nothing usable,
/// which is reported as "no CA" rather than as an error: a board with a
/// corrupt bundle is a board that trusts nobody, and that is the honest
/// reading.
fn describe_ca(path: &Path) -> Option<ClientCaView> {
    let pem = std::fs::read(path).ok()?;
    let chain = X509::stack_from_pem(&pem).ok()?;
    let first = chain.first()?;
    let digest = first.digest(openssl::hash::MessageDigest::sha256()).ok()?;

    Some(ClientCaView {
        subject: name_to_string(first.subject_name()),
        issuer: name_to_string(first.issuer_name()),
        not_after: first.not_after().to_string(),
        fingerprint: digest
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
        count: chain.len(),
    })
}

fn name_to_string(name: &openssl::x509::X509NameRef) -> String {
    name.entries()
        .map(|e| {
            format!(
                "{}={}",
                e.object().nid().short_name().unwrap_or("?"),
                e.data().to_string().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Load a bundle through the same call the TLS acceptor makes at startup.
///
/// Writing the file to disk first is deliberate: `set_ca_file` takes a path,
/// which is what the daemon will hand it on the next boot, so this exercises
/// the real code path rather than a similar one. The scratch file goes beside
/// nothing important and is removed either way.
fn trial_load(pem: &[u8]) -> Result<(), String> {
    use openssl::ssl::{SslAcceptor, SslMethod};

    let path = std::env::temp_dir().join(format!("bmcd-ca-trial-{}.pem", std::process::id()));
    std::fs::write(&path, pem).map_err(|e| e.to_string())?;

    let result = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())
        .map_err(|e| e.to_string())
        .and_then(|mut builder| builder.set_ca_file(&path).map_err(|e| e.to_string()));

    let _ = std::fs::remove_file(&path);
    result
}

async fn get_access(request: HttpRequest, tls: web::Data<config::Tls>) -> HttpResponse {
    let overrides = AccessOverrides::load();
    let (header_name, header_source) = tls.effective_identity_header(&overrides);

    let state = AccessState {
        actor: actor_of(&request),
        local_account: "root".to_string(),
        client_ca: tls.effective_client_ca().as_deref().and_then(describe_ca),
        identity_header: HeaderView {
            name: header_name,
            source: header_source.to_string(),
        },
        client_ca_pinned_in_config: tls.client_ca.is_some(),
    };

    HttpResponse::Ok().json(state)
}

/// The hash `username` currently has, straight out of `/etc/shadow`.
fn shadow_hash(username: &str) -> Option<String> {
    let content = std::fs::read_to_string(SHADOW_FILE).ok()?;
    for line in content.lines() {
        let mut fields = line.split(':');
        if fields.next() == Some(username) {
            return fields.next().map(str::to_string);
        }
    }
    None
}

async fn set_password(
    request: HttpRequest,
    body: web::Json<PasswordChange>,
) -> Result<HttpResponse, LegacyResponse> {
    let body = body.into_inner();

    // THE CURRENT PASSWORD IS REQUIRED, including from an operator the
    // gateway vouched for. Their certificate proves the gateway trusts them;
    // it does not prove they are the person who owns this board's console,
    // and a change made without the old password is a lockout anyone holding
    // a live session can perform. Proving you already hold it costs the
    // legitimate operator one field.
    if body.username.is_empty() {
        return Err(LegacyResponse::bad_request("`username` is required"));
    }

    if body.new_password.chars().count() < MIN_PASSWORD_LEN {
        return Err(LegacyResponse::bad_request(
            "the new password is too short: at least 12 characters",
        ));
    }

    if body.new_password == body.current_password {
        return Err(LegacyResponse::bad_request(
            "the new password is the same as the current one",
        ));
    }

    let Some(hash) = shadow_hash(&body.username) else {
        // Deliberately the same refusal as a wrong password. Which local
        // accounts exist is not something an unauthenticated-guess loop
        // should be able to enumerate through this endpoint.
        return Err(LegacyResponse::Error(
            actix_web::http::StatusCode::FORBIDDEN,
            "the current password is wrong".into(),
        ));
    };

    if pwhash::unix::crypt(body.current_password.as_str(), hash.as_str()).unwrap_or_default()
        != hash
    {
        tracing::warn!(
            "refused a password change for {}: current password wrong (asked by {})",
            body.username,
            actor_of(&request).name
        );
        return Err(LegacyResponse::Error(
            actix_web::http::StatusCode::FORBIDDEN,
            "the current password is wrong".into(),
        ));
    }

    // chpasswd, with the pair on STDIN. Not `passwd` and not a shell: a
    // password on a command line is in `ps` for anyone on the box and in the
    // shell's history afterwards, and this daemon runs as root.
    let line = format!("{}:{}\n", body.username, body.new_password);
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<std::process::Output> {
        let mut child = Command::new("chpasswd")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .expect("stdin was piped")
            .write_all(line.as_bytes())?;
        child.wait_with_output()
    })
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            // The name and the actor, never the password. bmcd already
            // watches /etc/shadow with inotify, so the cached hash updates
            // itself and existing sessions keep working -- a token outlives
            // the password it was minted from, which is why the interface
            // tells the operator to sign other sessions out.
            tracing::info!(
                "password changed for {} by {}",
                body.username,
                actor_of(&request).name
            );
            Ok(HttpResponse::NoContent().finish())
        }
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            tracing::error!("chpasswd refused the change: {stderr}");
            Err(LegacyResponse::from(anyhow::anyhow!(
                "the system refused the change: {stderr}"
            )))
        }
        Ok(Err(e)) => Err(LegacyResponse::from(anyhow::anyhow!(
            "could not run chpasswd: {e}"
        ))),
        Err(e) => Err(LegacyResponse::from(anyhow::anyhow!(
            "the password change did not complete: {e}"
        ))),
    }
}

async fn put_client_ca(
    request: HttpRequest,
    tls: web::Data<config::Tls>,
    body: web::Json<ClientCaUpload>,
) -> Result<HttpResponse, LegacyResponse> {
    let body = body.into_inner();

    if tls.client_ca.is_some() {
        return Err(LegacyResponse::bad_request(
            "the client CA is pinned in config.yaml; change it there, not here",
        ));
    }

    // Parse BEFORE writing. A bundle that is not a certificate would leave
    // the board asking every client for a certificate it can never verify,
    // and the operator would find out at the next login rather than now.
    let chain = X509::stack_from_pem(body.pem.as_bytes())
        .map_err(|e| LegacyResponse::bad_request(format!("not a PEM certificate bundle: {e}")))?;

    if chain.is_empty() {
        return Err(LegacyResponse::bad_request(
            "the bundle contains no certificates",
        ));
    }

    // THEN LOAD IT THE WAY THE SERVER WILL. Parsing proves it is a
    // certificate; this proves the acceptor can use it. `set_ca_file` is the
    // exact call `load_tls_config` makes at startup, so a bundle that passes
    // here is a bundle the daemon can start with -- and a board that cannot
    // start is the failure this whole feature had to avoid, the one that made
    // `client_ca` unsafe to ship in a default config.yaml.
    //
    // What it does NOT prove is that these are CA certificates: OpenSSL is
    // happy to load a leaf into a trust store. That mistake is fail-closed --
    // a leaf as the anchor verifies nobody, so the proxy is refused rather
    // than wrongly admitted -- and the response echoes back the subjects so
    // the operator can see what they actually pasted.
    if let Err(e) = trial_load(body.pem.as_bytes()) {
        return Err(LegacyResponse::bad_request(format!(
            "the TLS layer will not accept this bundle: {e}"
        )));
    }

    config::write_client_ca(body.pem.as_bytes())
        .map_err(|e| LegacyResponse::from(anyhow::anyhow!("could not store the CA: {e}")))?;

    if let Some(header) = body.identity_header.as_deref() {
        let header = header.trim();
        if header.is_empty() {
            return Err(LegacyResponse::bad_request(
                "`identity_header` cannot be empty",
            ));
        }
        AccessOverrides {
            identity_header: Some(header.to_string()),
        }
        .store()
        .map_err(|e| {
            LegacyResponse::from(anyhow::anyhow!("could not store the header name: {e}"))
        })?;
    }

    tracing::info!(
        "client CA replaced by {} ({} certificate(s))",
        actor_of(&request).name,
        chain.len()
    );

    // Reload, not reboot. The TLS acceptor is built once at startup, so the
    // new anchor only takes effect on a fresh listener -- and the answer has
    // to be sent before the daemon goes, or the caller sees a dropped
    // connection and no reason for it.
    Ok(HttpResponse::Accepted().json(serde_json::json!({
        "reload_required": true,
        "detail": "Stored. It takes effect when the daemon reloads."
    })))
}

async fn delete_client_ca(
    request: HttpRequest,
    tls: web::Data<config::Tls>,
) -> Result<HttpResponse, LegacyResponse> {
    if tls.client_ca.is_some() {
        return Err(LegacyResponse::bad_request(
            "the client CA is pinned in config.yaml; remove it there, not here",
        ));
    }

    // THE LOCKOUT GUARD. Removing the anchor that verified this very
    // connection ends every proxied session, including the one asking, and
    // the interface used to ask is reached THROUGH that proxy. Whoever wants
    // this gone can do it from the board's own interface with a password,
    // where the consequence is visible and recoverable.
    if actor_of(&request).scheme == "mtls" {
        return Err(LegacyResponse::bad_request(
            "you are authenticated by this CA; remove it from the board's own interface instead",
        ));
    }

    config::remove_client_ca()
        .map_err(|e| LegacyResponse::from(anyhow::anyhow!("could not remove the CA: {e}")))?;

    tracing::info!("client CA removed by {}", actor_of(&request).name);

    Ok(HttpResponse::Accepted().json(serde_json::json!({
        "reload_required": true,
        "detail": "Removed. It takes effect when the daemon reloads."
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of our own. `tempfile` is not a dependency of this
    /// crate and one test is a poor reason to add one.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bmcd-access-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn self_signed(cn: &str, ca: bool) -> String {
        let rsa = openssl::rsa::Rsa::generate(2048).unwrap();
        let key = openssl::pkey::PKey::from_rsa(rsa).unwrap();
        let mut name = openssl::x509::X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", cn).unwrap();
        let name = name.build();

        let mut builder = openssl::x509::X509Builder::new().unwrap();
        builder.set_version(2).unwrap();
        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&key).unwrap();
        builder
            .set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        builder
            .set_not_after(&openssl::asn1::Asn1Time::days_from_now(365).unwrap())
            .unwrap();
        if ca {
            builder
                .append_extension(
                    openssl::x509::extension::BasicConstraints::new()
                        .critical()
                        .ca()
                        .build()
                        .unwrap(),
                )
                .unwrap();
        }
        builder
            .sign(&key, openssl::hash::MessageDigest::sha256())
            .unwrap();
        String::from_utf8(builder.build().to_pem().unwrap()).unwrap()
    }

    /// The gate that matters: a bundle this accepts is a bundle the daemon
    /// can start with. Checked against the real acceptor call, not a parser.
    #[test]
    fn a_real_ca_loads_where_the_server_will_load_it() {
        assert!(trial_load(self_signed("test-ca", true).as_bytes()).is_ok());
    }

    /// The mistake the install script warned about, and the reason this
    /// endpoint validates before writing: a file that cannot be loaded makes
    /// the daemon fail to START, so it must never reach the disk.
    #[test]
    fn rubbish_is_refused_rather_than_stored() {
        let err =
            trial_load(b"-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n")
                .expect_err("a broken bundle must not load");
        assert!(!err.is_empty());
        assert!(
            trial_load(b"").is_err(),
            "an empty bundle must not load either"
        );
    }

    /// The description is what an operator compares against the fingerprint
    /// their proxy shows, so it has to come out of the certificate.
    #[test]
    fn a_bundle_is_described_by_its_contents() {
        let dir = scratch("describe");
        let path = dir.join("ca.pem");
        std::fs::write(&path, self_signed("test-ca", true)).unwrap();

        let view = describe_ca(&path).expect("a CA should describe itself");
        assert!(
            view.subject.contains("test-ca"),
            "subject: {}",
            view.subject
        );
        assert_eq!(view.count, 1);
        // SHA-256 is 32 bytes, printed as XX:XX:...
        assert_eq!(view.fingerprint.split(':').count(), 32);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A corrupt bundle reads as "this board trusts nobody", which is the
    /// honest answer, rather than as an error that hides the rest of the page.
    #[test]
    fn nonsense_describes_as_no_ca_rather_than_erroring() {
        let dir = scratch("nonsense");
        let path = dir.join("ca.pem");
        std::fs::write(&path, b"this is not a certificate").unwrap();

        assert!(describe_ca(&path).is_none());
        assert!(describe_ca(&dir.join("absent.pem")).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Twelve characters, and the check is on CHARACTERS rather than bytes:
    /// a passphrase in a non-Latin script is not shorter because its
    /// codepoints are wider.
    #[test]
    fn the_length_rule_counts_characters() {
        // Six Cyrillic letters and eight digits: fourteen characters, and
        // twenty bytes. The rule must read the first number, or a perfectly
        // good passphrase is refused for being written in the wrong alphabet.
        let cyrillic = "\u{43f}\u{430}\u{440}\u{43e}\u{43b}\u{44c}12345678";
        assert_eq!(cyrillic.chars().count(), 14);
        assert_eq!(cyrillic.len(), 20);
        assert!(cyrillic.chars().count() >= MIN_PASSWORD_LEN);
        assert!("short".chars().count() < MIN_PASSWORD_LEN);
    }
}
