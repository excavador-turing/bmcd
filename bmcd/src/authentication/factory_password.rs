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

//! A board still on the password it shipped with can do exactly one thing.
//!
//! Every one of these boards leaves the factory as `root` / `turing`, on a
//! project whose own front page says a board should not face the internet.
//! Upstream has had an issue open about it since 2023. The password is
//! printed in the quick-start guide, which is to say it is public, and it is
//! the same on every board anyone has ever bought.
//!
//! So until it is changed, the daemon answers **403 to everything** except
//! logging in and changing it, and the two storage endpoints below. Not a
//! warning banner: a banner is a thing people close.
//!
//! ## The storage endpoints are open on purpose
//!
//! A board that could not mount its settings volume runs on a layer in RAM,
//! and a board on that layer is *always* on the factory password: every
//! reboot is a factory reset. The page that has to tell its owner so is the
//! very page this gate shows, and the repair is the only thing that helps.
//! Making the owner set a password first would have them set one that is
//! lost at the next reboot, just to reach the button.
//!
//! It opens nothing new, and that is the whole argument. The password is
//! public, and the stock firmware's sshd has `PermitRootLogin yes` and
//! `PasswordAuthentication yes`, so `root` / `turing` already gives a root
//! shell and with it `ubirmvol` and `reboot`. **If SSH is ever hardened this
//! exemption has to be revisited**: it would then be a way to erase the
//! settings that SSH is not. The repair is still authenticated and still
//! refused unless the board's own facts say it is on offer. Exactly two
//! operations are opened, `GET /api/bmc/storage` and `POST
//! /api/bmc/storage/repair`, each for its own method only.
//!
//! ## The factory password, not the first login
//!
//! The test is "does `root`'s hash still verify the word `turing`", asked of
//! `/etc/shadow` itself. Never a flag in a file somewhere saying somebody has
//! been through a wizard.
//!
//! The difference matters in both directions. A board whose password was
//! changed over SSH before anyone opened the interface is never asked -- it
//! is not a factory board, whatever it has or has not been shown. And a board
//! reset to factory defaults **is** asked again, because it is a factory
//! board again; a flag would have been reset too, or worse, would not have
//! been.
//!
//! ## Loopback is exempt, deliberately
//!
//! Because refusing it would protect nothing. `/api/bmc` already drops
//! authentication entirely for the loopback interface, and somebody with a
//! shell on the board is already root on it -- they can read `/etc/shadow`,
//! run `chpasswd`, or stop this daemon. The threat this gate exists for is a
//! stranger who can reach port 443 on the management LAN, and that stranger
//! is not on loopback.
//!
//! What the exemption buys, concretely: the on-board `tpi` keeps working on a
//! board that has not been set up. Per the known-faults page it is the only
//! thing left that relies on the bypass at all.
//!
//! `tpi` pointed at a factory board **from another machine** is over the
//! network, is gated, and prints the reason -- which is what the ticket's
//! gate actually asks for.
//!
//! ## What this exemption is NOT for
//!
//! It was first justified here as protecting the first-boot promotion check,
//! on the grounds that `S99postupdate` reads metrics over loopback. **That is
//! wrong and the claim is withdrawn.** `/metrics` is served by a separate
//! plain-HTTP listener on 9110 with no authentication middleware at all, and
//! the gate's liveness probe hits `https://127.0.0.1/` -- the static
//! interface, not `/api/bmc`. Neither passes through here, so promotion is
//! unaffected whichever way this goes. `S99postupdate` says as much in its own
//! comments: its last dependence on the loopback bypass was removed when
//! `/metrics` moved.
//!
//! Recorded rather than quietly deleted, because a security exemption with a
//! reason that does not hold is worse than one with a thin reason: the next
//! person to read it inherits the confidence without the fact.

use actix_web::http::Method;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

/// What every board ships as.
pub const FACTORY_ACCOUNT: &str = "root";
pub const FACTORY_PASSWORD: &str = "turing";

pub const SHADOW_FILE: &str = "/etc/shadow";

/// What the board says to everything else while the factory password stands.
///
/// One sentence for a person and one instruction. The same words are in the
/// interface and in `tpi`, written once here so the three cannot drift.
pub const REFUSAL: &str =
    "This board is still using the password it shipped with, which is printed in \
     the quick-start guide and is the same on every board. Until it is changed, this is the \
     only thing it will do. Change it from the interface, or POST /api/bmc/access/password.";

/// The paths that still answer while the factory password stands.
///
/// Logging in, because you have to get far enough to change it. Changing it,
/// obviously. And reading the access card, because that is the page doing the
/// changing and it needs to know which account it is changing.
///
/// And the two storage operations, each for its own method (see the module
/// documentation for why they, and only they, are open).
///
/// Nothing else, and in particular nothing that reads the board's state: a
/// stranger who knows the published password should not be able to inventory
/// somebody's cluster before deciding whether to bother with it. What the
/// storage report tells such a stranger is whether the board is in safe mode
/// and why, which a board in that state has no way to keep private anyway.
fn is_permitted(method: &Method, path: &str) -> bool {
    match path {
        "/api/bmc/authenticate" | "/api/bmc/access/password" | "/api/bmc/access" => true,
        "/api/bmc/storage" => method == Method::GET,
        "/api/bmc/storage/repair" => method == Method::POST,
        _ => false,
    }
}

/// Whether the board is still on its factory password, cached against the
/// shadow file's modification time.
///
/// Cached because this is asked on every request; keyed on the mtime rather
/// than on a clock because the answer must change the instant the password
/// does. A password changed through this daemon, over SSH, or by an operator
/// editing the file all land the same way: the file is written, the mtime
/// moves, the next request re-reads.
pub struct FactoryPassword {
    path: PathBuf,
    cached: RwLock<Option<(SystemTime, bool)>>,
}

impl FactoryPassword {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FactoryPassword {
            path: path.into(),
            cached: RwLock::new(None),
        }
    }

    /// Is this request allowed through?
    pub fn permits(&self, method: &Method, path: &str) -> bool {
        is_permitted(method, path) || !self.still_set()
    }

    pub fn still_set(&self) -> bool {
        let stamp = std::fs::metadata(&self.path)
            .and_then(|meta| meta.modified())
            .ok();

        if let Ok(cached) = self.cached.read() {
            if let (Some((when, answer)), Some(now)) = (*cached, stamp) {
                if when == now {
                    return answer;
                }
            }
        }

        let answer = Self::read(&self.path);
        if let (Ok(mut cached), Some(now)) = (self.cached.write(), stamp) {
            *cached = Some((now, answer));
        }
        answer
    }

    /// The question, asked of the file.
    ///
    /// A shadow file that cannot be read answers **false**. The alternative is
    /// a daemon that refuses every request on a board whose `/etc/shadow` it
    /// cannot open -- which is a board nobody can administer, caused by a
    /// check that exists to keep boards administrable. Being unable to read
    /// the file is already loud elsewhere: the authenticator needs it to let
    /// anybody in at all.
    fn read(path: &Path) -> bool {
        let Ok(content) = std::fs::read_to_string(path) else {
            return false;
        };
        for line in content.lines() {
            let mut fields = line.split(':');
            if fields.next() != Some(FACTORY_ACCOUNT) {
                continue;
            }
            let Some(hash) = fields.next() else {
                return false;
            };
            // An account with no usable hash is locked or passwordless, not
            // factory. `verify` on such a field is not a question worth
            // asking.
            if hash.is_empty() || hash.starts_with('*') || hash.starts_with('!') {
                return false;
            }
            return pwhash::unix::verify(FACTORY_PASSWORD, hash);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shadow(dir: &Path, hash: &str) -> PathBuf {
        let path = dir.join("shadow");
        std::fs::write(
            &path,
            format!("daemon:*:19000:0:99999:7:::\nroot:{hash}:19000:0:99999:7:::\n"),
        )
        .expect("write");
        path
    }

    fn hash_of(password: &str) -> String {
        pwhash::sha512_crypt::hash(password).expect("a shadow hash")
    }

    #[test]
    fn a_board_on_the_factory_password_is_recognised() {
        let dir = tempdir::TempDir::new("factory").expect("temp dir");
        let path = shadow(dir.path(), &hash_of(FACTORY_PASSWORD));
        assert!(FactoryPassword::new(path).still_set());
    }

    #[test]
    fn a_board_whose_password_was_changed_is_not() {
        let dir = tempdir::TempDir::new("changed").expect("temp dir");
        let path = shadow(dir.path(), &hash_of("a much better password"));
        assert!(!FactoryPassword::new(path).still_set());
    }

    /// The whole reason this reads the file rather than keeping a flag: a
    /// password changed by anything at all -- this daemon, `passwd` over SSH,
    /// an operator with an editor -- must be noticed at once.
    #[test]
    fn a_change_is_noticed_without_a_restart() {
        let dir = tempdir::TempDir::new("noticed").expect("temp dir");
        let path = shadow(dir.path(), &hash_of(FACTORY_PASSWORD));
        let gate = FactoryPassword::new(&path);
        assert!(gate.still_set());

        // A second write in the same second would keep the mtime and be
        // served from the cache, which is the cache working rather than a
        // fault -- so make the file genuinely newer.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        shadow(dir.path(), &hash_of("a much better password"));
        assert!(!gate.still_set(), "the cache outlived the password");
    }

    #[test]
    fn a_locked_account_is_not_a_factory_account() {
        for field in ["*", "!", "!!", ""] {
            let dir = tempdir::TempDir::new("locked").expect("temp dir");
            let path = shadow(dir.path(), field);
            assert!(
                !FactoryPassword::new(path).still_set(),
                "{field:?} is not the factory password"
            );
        }
    }

    /// A board whose shadow file cannot be read must stay administrable.
    #[test]
    fn an_unreadable_shadow_file_does_not_lock_the_board() {
        assert!(!FactoryPassword::new("/nonexistent/shadow").still_set());
    }

    #[test]
    fn only_login_the_access_card_and_the_password_change_are_permitted() {
        let dir = tempdir::TempDir::new("permits").expect("temp dir");
        let path = shadow(dir.path(), &hash_of(FACTORY_PASSWORD));
        let gate = FactoryPassword::new(path);

        assert!(gate.permits(&Method::POST, "/api/bmc/authenticate"));
        assert!(gate.permits(&Method::GET, "/api/bmc/access"));
        assert!(gate.permits(&Method::POST, "/api/bmc/access/password"));

        for refused in [
            "/api/bmc",
            "/api/bmc/network/switch",
            "/api/bmc/tls/certificate",
            "/api/bmc/access/client-ca",
            "/api/bmc/serial/status",
        ] {
            for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
                assert!(
                    !gate.permits(&method, refused),
                    "{method} {refused} should be refused"
                );
            }
        }
    }

    /// The storage report and the repair are open on a factory board, each
    /// for its own method and for nothing else: a board in safe mode is
    /// always on the factory password, and the repair is the way out.
    #[test]
    fn exactly_the_storage_report_and_the_storage_repair_are_opened() {
        let dir = tempdir::TempDir::new("storage-gate").expect("temp dir");
        let path = shadow(dir.path(), &hash_of(FACTORY_PASSWORD));
        let gate = FactoryPassword::new(path);

        assert!(gate.permits(&Method::GET, "/api/bmc/storage"));
        assert!(gate.permits(&Method::POST, "/api/bmc/storage/repair"));

        // Not the other verb on either, and nothing nearby.
        for (method, refused) in [
            (Method::POST, "/api/bmc/storage"),
            (Method::PUT, "/api/bmc/storage"),
            (Method::DELETE, "/api/bmc/storage"),
            (Method::GET, "/api/bmc/storage/repair"),
            (Method::PUT, "/api/bmc/storage/repair"),
            (Method::POST, "/api/bmc/storage/other"),
            (Method::POST, "/api/bmc/storage/repair/x"),
            (Method::POST, "/api/bmc/storage/"),
        ] {
            assert!(
                !gate.permits(&method, refused),
                "{method} {refused} should be refused"
            );
        }

        // No other mutating endpoint was opened with them.
        for refused in [
            "/api/bmc/network/switch",
            "/api/bmc/network/switch/confirm",
            "/api/bmc/network/address",
            "/api/bmc/network/address/confirm",
            "/api/bmc/tls/certificate",
            "/api/bmc/access/client-ca",
            "/api/bmc/reboot",
            "/api/bmc/firmware/install",
            "/api/bmc/hostname",
            "/api/bmc/network/reset",
            "/api/bmc/usb",
            "/api/bmc/power",
            "/api/bmc",
        ] {
            for method in [Method::POST, Method::PUT, Method::DELETE] {
                assert!(
                    !gate.permits(&method, refused),
                    "{method} {refused} should be refused"
                );
            }
        }
    }

    /// And once it is changed, everything is permitted -- including the paths
    /// that were never on the list.
    #[test]
    fn a_changed_password_permits_everything() {
        let dir = tempdir::TempDir::new("open").expect("temp dir");
        let path = shadow(dir.path(), &hash_of("a much better password"));
        let gate = FactoryPassword::new(path);
        assert!(gate.permits(&Method::GET, "/api/bmc"));
        assert!(gate.permits(&Method::PUT, "/api/bmc/network/switch"));
    }
}
