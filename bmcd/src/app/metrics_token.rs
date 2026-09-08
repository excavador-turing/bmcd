// Copyright 2026 excavador
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
//! A credential that reads `/metrics` and can do nothing else.
//!
//! ## Why this exists rather than a second Linux account
//!
//! `LinuxAuthenticator` parses `/etc/shadow` and verifies a hash. That is the
//! whole of it: there is no role, no permission and no per-route check
//! anywhere in the authentication module, so **every account with a valid
//! shadow entry can power-cycle four compute modules and flash the
//! firmware**. Adding a `metrics` user to `/etc/shadow` would therefore not
//! have reduced the blast radius of a scrape credential -- it would have
//! created a second credential with root's authority and put it in a
//! Prometheus config.
//!
//! So this deliberately does NOT go through the shadow file. It is a
//! separate secret, checked only on the `/metrics` scope, and it is useless
//! against `/api/bmc`. Rotating it cannot lock anyone out of the web
//! interface, and rotating the root password cannot break a scrape.
//!
//! ## Where it lives
//!
//! `/mnt/overlay/metrics-token`, mode 0600. The overlay because both
//! firmware images mount it, so the token survives an A/B promotion -- a
//! scrape must not break because the board took an update. `postupdate.log`
//! and the staged-firmware note live there for the same reason.
use rand::RngCore;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Where the token is stored.
pub const TOKEN_PATH: &str = "/mnt/overlay/metrics-token";

/// The HTTP Basic username a scrape must present. It is deliberately NOT a
/// Unix account: nothing resolves it against `/etc/passwd`, and a real
/// account by this name would gain nothing from it.
pub const TOKEN_USERNAME: &str = "metrics";

/// 32 bytes, hex encoded. Long enough that guessing is not a strategy, and
/// short enough to paste into a scrape config without wrapping.
const TOKEN_BYTES: usize = 32;

/// A token as stored, without the secret when it is not being handed out.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MetricsToken {
    /// The secret itself.
    pub token: String,
    /// When it was generated, in the board's own idea of the time.
    pub created_at: String,
}

/// Generates a token. Not a constructor for reading one -- see [`load`].
fn generate() -> MetricsToken {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    MetricsToken {
        token: hex::encode(bytes),
        created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    }
}

/// Parses the stored form. `KEY=VALUE`, so it can be read and written by a
/// shell script during recovery without a tool.
fn parse(body: &str) -> Option<MetricsToken> {
    let mut token = None;
    let mut created_at = None;
    for line in body.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim() {
            "TOKEN" => token = Some(value),
            "CREATED_AT" => created_at = Some(value),
            _ => {}
        }
    }
    let token = token?;
    if token.is_empty() {
        return None;
    }
    Some(MetricsToken {
        token,
        created_at: created_at.unwrap_or_default(),
    })
}

fn render(token: &MetricsToken) -> String {
    format!("TOKEN={}\nCREATED_AT={}\n", token.token, token.created_at)
}

/// Reads the stored token, if there is one.
pub async fn load() -> Option<MetricsToken> {
    let body = tokio::fs::read_to_string(TOKEN_PATH).await.ok()?;
    parse(&body)
}

/// Writes a token with 0600 permissions.
///
/// The mode is set after the write rather than at create time because
/// `tokio::fs::write` does not take a mode; a token readable by anything
/// else on the board would defeat the point of it being separate.
async fn store(token: &MetricsToken) -> io::Result<()> {
    tokio::fs::write(TOKEN_PATH, render(token)).await?;
    let mut perms = tokio::fs::metadata(TOKEN_PATH).await?.permissions();
    perms.set_mode(0o600);
    tokio::fs::set_permissions(TOKEN_PATH, perms).await
}

/// Returns the current token, generating one on first use.
///
/// Generating on demand rather than at install time means a board that has
/// never been asked for a token does not carry one, and a board restored
/// from a backup without the overlay gets a fresh one instead of failing.
pub async fn ensure() -> io::Result<MetricsToken> {
    if let Some(existing) = load().await {
        return Ok(existing);
    }
    let fresh = generate();
    store(&fresh).await?;
    Ok(fresh)
}

/// Replaces the token. The previous one stops working immediately, which is
/// the entire point of rotation.
pub async fn rotate() -> io::Result<MetricsToken> {
    let fresh = generate();
    store(&fresh).await?;
    Ok(fresh)
}

/// Whether the token directory exists at all. A board with no overlay
/// mounted cannot store a token, and saying so beats writing one somewhere
/// that will not survive a reboot.
pub fn storage_available() -> bool {
    Path::new(TOKEN_PATH)
        .parent()
        .map(|p| p.is_dir())
        .unwrap_or(false)
}

/// Compares two secrets without leaking their common prefix through timing.
///
/// Hand-written because this daemon is cross-compiled into a firmware image
/// at 78 % of its flash slot and does not carry a crypto crate for four
/// lines. The early return on length is deliberate and safe: the length of a
/// token is not secret, and a candidate of the wrong length cannot match.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

/// Whether a presented credential is the current metrics token.
///
/// Returns false when no token has been generated: a board with no token
/// grants no metrics access, rather than granting it to everyone.
pub async fn verify(username: &str, password: &str) -> bool {
    if username != TOKEN_USERNAME {
        return false;
    }
    let Some(stored) = load().await else {
        return false;
    };
    constant_time_eq(stored.token.as_bytes(), password.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_token_is_64_hex_characters() {
        let t = generate();
        assert_eq!(t.token.len(), TOKEN_BYTES * 2);
        assert!(t.token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_generated_tokens_differ() {
        assert_ne!(generate().token, generate().token);
    }

    #[test]
    fn the_stored_form_round_trips() {
        let t = generate();
        let parsed = parse(&render(&t)).expect("parses");
        assert_eq!(parsed, t);
    }

    /// A truncated or empty file must read as "no token", never as a token
    /// that happens to be the empty string -- which would otherwise match a
    /// caller presenting no password at all.
    #[test]
    fn an_empty_or_broken_file_is_not_a_token() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("TOKEN=\n"), None);
        assert_eq!(parse("CREATED_AT=2026-01-01T00:00:00Z\n"), None);
        assert_eq!(parse("garbage\n"), None);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let p = parse("TOKEN=abc\nNOTE=hello\nCREATED_AT=x\n").expect("parses");
        assert_eq!(p.token, "abc");
    }

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
