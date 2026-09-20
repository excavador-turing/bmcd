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

//! The certificate the board serves, readable and replaceable.
//!
//! Asked for twice in the Turing Pi Discord by people running their own CA:
//! they want the board to serve a certificate that CA issued, so a browser
//! that trusts it once shows no warning and the serial console works. Before
//! this, the only way to do that was to copy two files over SSH.
//!
//! ## Why this is not the enrolment ticket
//!
//! An earlier plan had the board make its own key and enrol it, and was
//! cancelled: it was the wrong shape for a fleet, and its objection was that
//! uploading a certificate means uploading the key. That objection is right
//! for a PKI where the key must never leave the device. It does not apply to
//! somebody who generated the key on their own workstation and wants to put
//! the result on a board -- that is the operation every router and NAS
//! offers, and the key was already theirs.
//!
//! ## Why its own path and a JSON body
//!
//! The legacy dispatcher writes every `opt=set` call to the audit log with its
//! whole query string. A private key routed through it would be recorded in
//! clear in `/tmp/bmcd.<date>.log`. So this takes a body, and logs the action,
//! the actor and the certificate's subject -- never the key.
//!
//! ## Where it is written
//!
//! To the same two files the board's own generator uses, which are the two
//! files `config.yaml` points the listener at. That is deliberate: the
//! generator refuses to touch a certificate it did not issue, so an installed
//! one is safe there, and `DELETE` is then simply "remove these and let the
//! generator run".

use crate::api::into_legacy_response::LegacyResponse;
use crate::authentication::authentication_context::Actor;
use crate::config;
use crate::tls_store::{Served, ServedCertificate, Source};
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use openssl::pkey::{PKey, Private};
use openssl::x509::X509;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// The script that issues the board's own certificate, run when an installed
/// one is removed so the board is never left without one.
const GENERATOR: &str = "/etc/bmcd/generate_self_signedx509.sh";

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/tls")
            .route("/certificate", web::get().to(get_certificate))
            .route("/certificate", web::put().to(put_certificate))
            .route("/certificate", web::delete().to(delete_certificate)),
    );
}

/// What is on the wire, in the terms somebody deciding whether to replace it
/// actually needs.
#[derive(Debug, Serialize)]
struct CertificateView {
    subject: String,
    issuer: String,
    not_before: String,
    not_after: String,
    /// SHA-256 over the DER, the same digest openssl prints and a browser
    /// shows.
    fingerprint: String,
    /// `ecdsa-p384`, `rsa-2048`, `ed25519`.
    key: Option<String>,
    /// `self-signed` or `installed`. Decides whether the board renews it.
    source: &'static str,
    /// Every name and address this certificate asserts. The reason a browser
    /// accepts or refuses it, so it is shown rather than summarised.
    names: Vec<String>,
    /// Intermediates sent alongside the leaf.
    chain_length: usize,
}

#[derive(Debug, Deserialize)]
struct CertificateUpload {
    /// The certificate, PEM. Intermediates may follow the leaf in the same
    /// string, leaf first, which is the order every tool emits.
    certificate: String,
    /// Its private key, PEM. Never logged, never echoed back.
    private_key: String,
}

fn actor_name(request: &HttpRequest) -> String {
    match request.extensions().get::<Actor>() {
        Some(Actor::User { name, .. }) => name.clone(),
        Some(Actor::Loopback) => "loopback".to_string(),
        None => "unattributed".to_string(),
    }
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

/// Every name and address a certificate asserts, in the form a person would
/// write them.
fn names_in(cert: &X509) -> Vec<String> {
    let Some(alt) = cert.subject_alt_names() else {
        return Vec::new();
    };
    alt.iter()
        .filter_map(|name| {
            if let Some(dns) = name.dnsname() {
                Some(dns.to_string())
            } else {
                name.ipaddress().map(|raw| match raw.len() {
                    4 => std::net::Ipv4Addr::from([raw[0], raw[1], raw[2], raw[3]]).to_string(),
                    16 => {
                        let mut octets = [0u8; 16];
                        octets.copy_from_slice(raw);
                        std::net::Ipv6Addr::from(octets).to_string()
                    }
                    // A SAN that is neither is still worth showing: it is
                    // part of why a browser may refuse the certificate.
                    _ => format!("{raw:02x?}"),
                })
            }
        })
        .collect()
}

fn describe(served: &Served) -> CertificateView {
    let cert = &served.leaf;
    let fingerprint = cert
        .digest(openssl::hash::MessageDigest::sha256())
        .map(|d| {
            d.iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(":")
        })
        .unwrap_or_default();

    CertificateView {
        subject: name_to_string(cert.subject_name()),
        issuer: name_to_string(cert.issuer_name()),
        not_before: cert.not_before().to_string(),
        not_after: cert.not_after().to_string(),
        fingerprint,
        key: served.facts().key,
        source: served.source.as_str(),
        names: names_in(cert),
        chain_length: served.chain.len(),
    }
}

async fn get_certificate(store: web::Data<ServedCertificate>) -> HttpResponse {
    HttpResponse::Ok().json(describe(&store.current()))
}

/// Every name this board can honestly be reached by: its hostname, the two
/// forms a browser might use for it, and every address it currently holds.
///
/// The same set the board's own generator puts in its SAN, derived the same
/// way and for the same reason -- a certificate that names none of these is
/// one no browser will accept, whoever issued it.
fn board_names() -> Vec<String> {
    let mut names = Vec::new();

    if let Ok(host) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let host = host.trim_end_matches(['\0', '\n']).trim().to_string();
        if !host.is_empty() {
            names.push(format!("{host}.local"));
            names.push(host);
        }
    }
    names.push("localhost".to_string());

    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if interface.is_loopback() {
                continue;
            }
            let ip = interface.ip();
            // A certificate asserting a link-local address tells a client
            // nothing it can use, so one is not evidence that the certificate
            // names this board.
            if ip.is_unspecified()
                || matches!(ip, std::net::IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80)
            {
                continue;
            }
            if let std::net::IpAddr::V4(v4) = ip {
                if v4.is_link_local() {
                    continue;
                }
            }
            names.push(ip.to_string());
        }
    }

    names
}

/// True when `cert` asserts at least one name this board answers to.
///
/// Wildcards count: `*.lan` covers `bmc-1.lan`, and a person issuing from
/// their own CA may well have one.
fn names_this_board(cert: &X509, board: &[String]) -> bool {
    let asserted = names_in(cert);
    board.iter().any(|mine| {
        asserted.iter().any(|theirs| {
            theirs.eq_ignore_ascii_case(mine)
                || theirs
                    .strip_prefix("*.")
                    .and_then(|suffix| mine.split_once('.').map(|(_, rest)| rest == suffix))
                    .unwrap_or(false)
        })
    })
}

/// The extended key usage extension, `2.5.29.37`, as it appears in DER.
const OID_EXTENDED_KEY_USAGE: &[u8] = &[0x06, 0x03, 0x55, 0x1d, 0x25];
/// `serverAuth`, `1.3.6.1.5.5.7.3.1`.
const OID_SERVER_AUTH: &[u8] = &[0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
/// `anyExtendedKeyUsage`, `2.5.29.37.0`, which permits every purpose.
const OID_ANY_USAGE: &[u8] = &[0x06, 0x04, 0x55, 0x1d, 0x25, 0x00];

/// Whether the certificate may be used to authenticate a server.
///
/// A certificate with no extended key usage at all is unrestricted, and that
/// is the common case for one issued by a small private CA. One that lists
/// usages and leaves out server authentication is one a browser will refuse,
/// and saying so now beats finding out when the board stops answering.
///
/// Read by scanning the DER for the three object identifiers rather than
/// through OpenSSL, whose accessor for this is not in the safe wrapper and
/// would cost two more dependencies for one call.
///
/// The scan is deliberately permissive where it is uncertain. A byte sequence
/// matching an OID somewhere it is not an OID would make this return `true`,
/// which installs a certificate that might not serve -- recoverable, and
/// visible immediately. The opposite mistake would refuse a certificate that
/// is perfectly good, on a board that may be reachable no other way.
fn may_serve(cert: &X509) -> bool {
    let Ok(der) = cert.to_der() else {
        // Unreadable here means unreadable to the TLS layer too, and the
        // checks around this one will have already said so more precisely.
        return true;
    };
    let contains = |needle: &[u8]| der.windows(needle.len()).any(|w| w == needle);

    !contains(OID_EXTENDED_KEY_USAGE) || contains(OID_SERVER_AUTH) || contains(OID_ANY_USAGE)
}

/// Read the pair, refusing anything the board should not serve.
///
/// Every check happens before a byte is written. A board that has been given
/// a certificate it cannot serve is a board nobody can reach to correct it,
/// so the only safe order is to prove the pair usable, then store it.
fn validate(upload: &CertificateUpload) -> Result<(PKey<Private>, X509, Vec<X509>), String> {
    let mut chain = X509::stack_from_pem(upload.certificate.as_bytes())
        .map_err(|e| format!("not a PEM certificate: {e}"))?;
    if chain.is_empty() {
        return Err("no certificate in the PEM given".to_string());
    }
    let leaf = chain.remove(0);

    let key = PKey::private_key_from_pem(upload.private_key.as_bytes())
        .map_err(|e| format!("not a PEM private key: {e}"))?;

    let cert_key = leaf
        .public_key()
        .map_err(|e| format!("the certificate has no usable public key: {e}"))?;
    if !cert_key.public_eq(&key) {
        return Err("the key does not belong to this certificate".to_string());
    }

    let now = openssl::asn1::Asn1Time::days_from_now(0)
        .map_err(|e| format!("could not read the clock: {e}"))?;
    if leaf.not_before() > now.as_ref() {
        return Err(format!(
            "the certificate is not valid until {}",
            leaf.not_before()
        ));
    }
    if leaf.not_after() < now.as_ref() {
        return Err(format!("the certificate expired on {}", leaf.not_after()));
    }

    if !may_serve(&leaf) {
        return Err(
            "the certificate's extended key usage does not include server authentication"
                .to_string(),
        );
    }

    let board = board_names();
    if !names_this_board(&leaf, &board) {
        return Err(format!(
            "the certificate names {:?}, and this board answers to {:?} -- no browser would accept it",
            names_in(&leaf),
            board
        ));
    }

    Ok((key, leaf, chain))
}

/// Write one file and make sure it is on the disk.
///
/// Through a temporary name in the same directory and a rename, so a daemon
/// reading mid-write never sees half a file and a failure leaves the previous
/// contents alone. The same shape the board's own generator uses.
fn write_atomically(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let tmp = path.with_extension(format!("new.{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

async fn put_certificate(
    request: HttpRequest,
    tls: web::Data<config::Tls>,
    store: web::Data<ServedCertificate>,
    body: web::Json<CertificateUpload>,
) -> Result<HttpResponse, LegacyResponse> {
    let body = body.into_inner();
    let (key, leaf, chain) = validate(&body).map_err(LegacyResponse::bad_request)?;

    // The key first, and only then the certificate. Either order leaves a
    // window where the two files disagree; this one leaves the OLD
    // certificate beside a new key, which nothing reads until the certificate
    // lands. The reverse would leave a new certificate beside the old key,
    // which is the pair a reboot would try to serve.
    write_atomically(&tls.private_key, body.private_key.as_bytes(), 0o600)
        .map_err(|e| LegacyResponse::from(anyhow::anyhow!("could not store the key: {e}")))?;
    write_atomically(&tls.certificate, body.certificate.as_bytes(), 0o644).map_err(|e| {
        LegacyResponse::from(anyhow::anyhow!("could not store the certificate: {e}"))
    })?;

    let source = Source::of(&leaf);
    let subject = name_to_string(leaf.subject_name());
    let not_after = leaf.not_after().to_string();

    store.replace(Served {
        key,
        leaf,
        chain,
        source,
    });

    // The subject and the expiry, because those are what an operator checks
    // afterwards. Never the key, and never the certificate body.
    tracing::info!(
        "certificate installed by {}: {subject}, valid until {not_after}",
        actor_name(&request)
    );

    Ok(HttpResponse::Ok().json(describe(&store.current())))
}

async fn delete_certificate(
    request: HttpRequest,
    tls: web::Data<config::Tls>,
    store: web::Data<ServedCertificate>,
) -> Result<HttpResponse, LegacyResponse> {
    if store.current().source == Source::SelfSigned {
        return Err(LegacyResponse::bad_request(
            "the board is already serving its own certificate; there is nothing to remove",
        ));
    }

    // Remove both, then ask the generator for a new pair. It writes to these
    // same two paths and will not run while one is present, so the removal is
    // what lets it issue.
    for path in [&tls.certificate, &tls.private_key] {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(LegacyResponse::from(anyhow::anyhow!(
                    "could not remove {}: {e}",
                    path.display()
                )));
            }
        }
    }

    let output = std::process::Command::new(GENERATOR)
        .output()
        .map_err(|e| LegacyResponse::from(anyhow::anyhow!("could not run {GENERATOR}: {e}")))?;
    if !output.status.success() {
        return Err(LegacyResponse::from(anyhow::anyhow!(
            "{GENERATOR} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let (key, leaf) = crate::load_keys_from_pem(&tls.private_key, &tls.certificate)
        .map_err(|e| LegacyResponse::from(anyhow::anyhow!("the new pair is unreadable: {e}")))?;
    let source = Source::of(&leaf);
    store.replace(Served {
        key,
        leaf,
        chain: Vec::new(),
        source,
    });

    tracing::info!(
        "installed certificate removed by {}; the board issued its own",
        actor_name(&request)
    );

    Ok(HttpResponse::Ok().json(describe(&store.current())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::x509::extension::{ExtendedKeyUsage, SubjectAlternativeName};
    use openssl::x509::{X509Builder, X509NameBuilder};

    struct Fixture {
        cn: &'static str,
        sans: &'static [&'static str],
        /// Days from now that the certificate becomes valid. `None` means it
        /// was valid in 2020 and expired in 2020 -- an expired fixture.
        ///
        /// Fixed dates rather than an offset from the clock, because
        /// `Asn1Time::from_unix` takes a `time_t`, which is 32 bits on the
        /// board's armv7. An `i64` here compiles on a workstation and fails to
        /// cross-compile, which is exactly the class of fault the board's
        /// cross-compile check exists to catch.
        starts_in: Option<u32>,
        lasts_days: u32,
        /// `None` leaves the extension out entirely, which means
        /// unrestricted.
        usages: Option<&'static [&'static str]>,
    }

    impl Default for Fixture {
        fn default() -> Self {
            Fixture {
                cn: "bmc-test",
                sans: &["DNS:localhost"],
                starts_in: Some(0),
                lasts_days: 30,
                usages: Some(&["serverAuth"]),
            }
        }
    }

    /// A certificate and its key, as the two PEM strings an upload carries.
    fn upload(fixture: Fixture) -> CertificateUpload {
        let group = EcGroup::from_curve_name(Nid::SECP384R1).expect("curve");
        let key = PKey::from_ec_key(EcKey::generate(&group).expect("key")).expect("pkey");

        let mut name = X509NameBuilder::new().expect("name");
        name.append_entry_by_nid(Nid::COMMONNAME, fixture.cn)
            .expect("cn");
        let name = name.build();

        let mut builder = X509Builder::new().expect("builder");
        builder.set_version(2).expect("version");
        builder.set_subject_name(&name).expect("subject");
        builder.set_issuer_name(&name).expect("issuer");
        builder.set_pubkey(&key).expect("pubkey");

        let (not_before, not_after) = match fixture.starts_in {
            Some(days) => (
                Asn1Time::days_from_now(days).expect("not before"),
                Asn1Time::days_from_now(days + fixture.lasts_days).expect("not after"),
            ),
            None => (
                Asn1Time::from_str("20200101000000Z").expect("not before"),
                Asn1Time::from_str("20200201000000Z").expect("not after"),
            ),
        };
        builder.set_not_before(&not_before).expect("not before");
        builder.set_not_after(&not_after).expect("not after");

        if !fixture.sans.is_empty() {
            let context = builder.x509v3_context(None, None);
            let mut san = SubjectAlternativeName::new();
            for entry in fixture.sans {
                match entry.split_once(':') {
                    Some(("DNS", value)) => {
                        san.dns(value);
                    }
                    Some(("IP", value)) => {
                        san.ip(value);
                    }
                    _ => panic!("fixture SAN must be DNS: or IP:"),
                };
            }
            let san = san.build(&context).expect("san");
            builder.append_extension(san).expect("append san");
        }

        if let Some(usages) = fixture.usages {
            let mut eku = ExtendedKeyUsage::new();
            for usage in usages {
                match *usage {
                    "serverAuth" => eku.server_auth(),
                    "clientAuth" => eku.client_auth(),
                    "codeSigning" => eku.code_signing(),
                    other => panic!("fixture does not know the usage {other}"),
                };
            }
            let eku = eku.build().expect("eku");
            builder.append_extension(eku).expect("append eku");
        }

        builder.sign(&key, MessageDigest::sha384()).expect("sign");
        let cert = builder.build();

        CertificateUpload {
            certificate: String::from_utf8(cert.to_pem().expect("cert pem")).expect("utf8"),
            private_key: String::from_utf8(key.private_key_to_pem_pkcs8().expect("key pem"))
                .expect("utf8"),
        }
    }

    /// `localhost` is in `board_names()` on every machine, so a fixture that
    /// names it passes the "does this name the board" check wherever the
    /// tests run -- including in CI, which has no board and no br0.
    #[test]
    fn a_good_pair_is_accepted() {
        let result = validate(&upload(Fixture::default()));
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn a_key_from_a_different_certificate_is_refused() {
        let mut body = upload(Fixture::default());
        body.private_key = upload(Fixture::default()).private_key;

        let error = validate(&body).expect_err("a mismatched pair must be refused");
        assert!(
            error.contains("does not belong"),
            "the reason should name the problem: {error}"
        );
    }

    /// The failure the whole endpoint exists to prevent: a certificate no
    /// browser would accept, installed on a board that may then be reachable
    /// only by ignoring the warning it was meant to remove.
    #[test]
    fn a_certificate_that_does_not_name_this_board_is_refused() {
        let error = validate(&upload(Fixture {
            sans: &["DNS:someone-elses-board.example"],
            ..Default::default()
        }))
        .expect_err("a certificate for another host must be refused");
        assert!(
            error.contains("no browser would accept it"),
            "the reason should say why: {error}"
        );
    }

    #[test]
    fn a_certificate_with_no_names_at_all_is_refused() {
        let error = validate(&upload(Fixture {
            sans: &[],
            ..Default::default()
        }))
        .expect_err("a certificate with no SAN must be refused");
        assert!(error.contains("names []"), "{error}");
    }

    #[test]
    fn an_expired_certificate_is_refused() {
        let error = validate(&upload(Fixture {
            starts_in: None,
            ..Default::default()
        }))
        .expect_err("an expired certificate must be refused");
        assert!(error.contains("expired on"), "{error}");
    }

    #[test]
    fn a_certificate_from_the_future_is_refused() {
        let error = validate(&upload(Fixture {
            starts_in: Some(7),
            ..Default::default()
        }))
        .expect_err("a certificate not yet valid must be refused");
        assert!(error.contains("not valid until"), "{error}");
    }

    /// A certificate that lists its purposes and leaves out server
    /// authentication cannot serve TLS, whatever else is right about it.
    #[test]
    fn a_certificate_that_may_not_serve_is_refused() {
        let error = validate(&upload(Fixture {
            usages: Some(&["clientAuth", "codeSigning"]),
            ..Default::default()
        }))
        .expect_err("a non-server certificate must be refused");
        assert!(error.contains("server authentication"), "{error}");
    }

    /// No extended key usage at all means unrestricted, which is what a small
    /// private CA usually issues. Refusing those would refuse the very people
    /// this endpoint is for.
    #[test]
    fn a_certificate_with_no_stated_usage_is_accepted() {
        let result = validate(&upload(Fixture {
            usages: None,
            ..Default::default()
        }));
        assert!(result.is_ok(), "{:?}", result.err());
    }

    /// Somebody issuing from their own CA may well hold a wildcard.
    #[test]
    fn a_wildcard_covering_the_board_is_accepted() {
        let mut board = board_names();
        board.push("bmc-1.lan".to_string());

        let body = upload(Fixture {
            sans: &["DNS:*.lan"],
            ..Default::default()
        });
        let chain = X509::stack_from_pem(body.certificate.as_bytes()).expect("pem");
        assert!(names_this_board(&chain[0], &board));
    }

    /// ...but a wildcard only covers one label, which is what every browser
    /// enforces and what makes `*.lan` useless for `a.b.lan`.
    #[test]
    fn a_wildcard_does_not_cover_a_deeper_name() {
        let board = vec!["a.b.lan".to_string()];
        let body = upload(Fixture {
            sans: &["DNS:*.lan"],
            ..Default::default()
        });
        let chain = X509::stack_from_pem(body.certificate.as_bytes()).expect("pem");
        assert!(!names_this_board(&chain[0], &board));
    }

    /// Nothing that reaches this endpoint may ever appear in a log line, so
    /// the view it returns is checked for the key rather than trusted to
    /// leave it out.
    #[test]
    fn the_returned_view_carries_no_key_material() {
        let body = upload(Fixture::default());
        let (key, leaf, chain) = validate(&body).expect("valid");
        let view = describe(&Served {
            source: Source::of(&leaf),
            key,
            leaf,
            chain,
        });

        let rendered = serde_json::to_string(&view).expect("json");
        assert!(!rendered.contains("PRIVATE KEY"), "{rendered}");
        for fragment in body.private_key.lines().filter(|l| l.len() > 20) {
            assert!(
                !rendered.contains(fragment),
                "a line of the key appeared in the response"
            );
        }
    }
}
