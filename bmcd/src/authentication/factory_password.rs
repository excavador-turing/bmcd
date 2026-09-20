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
//! logging in and changing it. Not a warning banner: a banner is a thing
//! people close.
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
//! `/api/bmc` already drops authentication entirely for the loopback
//! interface: that is how the on-board `tpi` works without credentials, and
//! how `S99postupdate` reads the metrics that decide whether a freshly
//! flashed image is promoted or rolled back.
//!
//! Refusing loopback would therefore fail the promotion check on the first
//! boot of every factory board -- the exact boards this protects -- and roll
//! the firmware back. It would also protect nothing: loopback is not the
//! management LAN, and somebody with a shell on the board owns the board
//! already. The threat here is a stranger who can reach port 443, and that
//! stranger is not on loopback.
//!
//! `tpi` pointed at a factory board **from another machine** is over the
//! network, is gated, and prints the reason.

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
/// Nothing else, and in particular nothing that reads the board's state: a
/// stranger who knows the published password should not be able to inventory
/// somebody's cluster before deciding whether to bother with it.
fn is_permitted(path: &str) -> bool {
    matches!(
        path,
        "/api/bmc/authenticate" | "/api/bmc/access/password" | "/api/bmc/access"
    )
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
    pub fn permits(&self, path: &str) -> bool {
        is_permitted(path) || !self.still_set()
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

        assert!(gate.permits("/api/bmc/authenticate"));
        assert!(gate.permits("/api/bmc/access"));
        assert!(gate.permits("/api/bmc/access/password"));

        for refused in [
            "/api/bmc",
            "/api/bmc/network/switch",
            "/api/bmc/tls/certificate",
            "/api/bmc/access/client-ca",
            "/api/bmc/serial/status",
        ] {
            assert!(!gate.permits(refused), "{refused} should be refused");
        }
    }

    /// And once it is changed, everything is permitted -- including the paths
    /// that were never on the list.
    #[test]
    fn a_changed_password_permits_everything() {
        let dir = tempdir::TempDir::new("open").expect("temp dir");
        let path = shadow(dir.path(), &hash_of("a much better password"));
        let gate = FactoryPassword::new(path);
        assert!(gate.permits("/api/bmc"));
        assert!(gate.permits("/api/bmc/network/switch"));
    }
}
