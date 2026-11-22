// Copyright 2023 Turing Machines
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The on-board local token, which replaces the loopback exemption.
//!
//! Generated at every start and written where only the daemon's user can
//! read it. The on-board `tpi` sends it as `Authorization: Bearer <token>`,
//! and it is accepted only from a loopback peer. It never expires and a
//! password change does not drop it; a restart replaces it.

use rand::distr::Alphanumeric;
use rand::Rng;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Where the token is written.
pub const LOCAL_TOKEN_DIR: &str = "/run/bmcd";
/// The token file's name in [`LOCAL_TOKEN_DIR`].
pub const LOCAL_TOKEN_FILE: &str = "local-token";

/// 64 random alphanumerics, the same shape as a session token.
pub fn generate() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect()
}

/// Write `token` to `dir/local-token`: the directory 0700, the file 0600,
/// written under a temporary name and renamed so a reader never sees half
/// of it.
pub fn write(dir: &Path, token: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let target = dir.join(LOCAL_TOKEN_FILE);
    let temporary = dir.join(format!(".{LOCAL_TOKEN_FILE}.tmp"));
    let _ = fs::remove_file(&temporary);
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&temporary, &target)?;
    Ok(target)
}

/// Equal in time independent of where the strings first differ.
pub fn matches(expected: &str, candidate: &str) -> bool {
    expected.len() == candidate.len()
        && expected
            .bytes()
            .zip(candidate.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_64_alphanumerics_and_new_every_time() {
        let one = generate();
        assert_eq!(one.len(), 64);
        assert!(one.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(one, generate());
    }

    #[test]
    fn the_file_is_private_and_replaced_whole() {
        let base = std::env::temp_dir().join(format!("bmcd-local-token-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let dir = base.join("run");

        let path = write(&dir, "first").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");
        let path = write(&dir, "second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temporary left");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn matching_is_exact() {
        assert!(matches("abc", "abc"));
        assert!(!matches("abc", "abd"));
        assert!(!matches("abc", "ab"));
        assert!(!matches("abc", ""));
    }
}
