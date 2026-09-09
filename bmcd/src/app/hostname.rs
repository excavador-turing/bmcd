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
//!
//! The board's name.
//!
//! Renaming a board was an SSH session: write `/etc/hostname`, call
//! `hostname`, remember that mDNS advertises the old name until something
//! restarts. There is a board B in the plan, and a name that assumes one board
//! is a name that has to change later, under time pressure, with the second
//! board already on the bench.
//!
//! The name is not only a label. It is what `about` reports and the interface
//! puts in its header, what `mdnsd` advertises as `<name>.local`, and the
//! `instance` label on every metrics series — so changing it breaks the
//! continuity of a Prometheus series, which is a decision rather than a side
//! effect and belongs to whoever presses the button.

use std::process::Command;

/// The running name. Writing this sysctl is `sethostname(2)`; the `about`
/// endpoint has always read the name from here.
const LIVE_HOSTNAME: &str = "/proc/sys/kernel/hostname";

/// On the overlay, so a rename survives a firmware upgrade rather than
/// reverting to the image's `turingpi` at the next update.
const HOSTNAME_FILE: &str = "/etc/hostname";

/// RFC 1123 allows 63 octets per label. Longer is refused rather than
/// truncated: a silently shortened name is one that disagrees with what was
/// typed everywhere it is later read.
const MAX_LEN: usize = 63;

/// Rejects anything that is not a single DNS label.
///
/// Deliberately stricter than the file format, which would accept any bytes.
/// A name with a dot in it is a fully-qualified name, and `mdnsd` would then
/// advertise something like `a.b.local`; a name with a space is one that
/// `hostname` accepts and every consumer disagrees about.
pub fn validate(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a hostname cannot be empty".to_string());
    }
    if name.len() > MAX_LEN {
        return Err(format!(
            "{name:?} is {} characters; a hostname label is at most {MAX_LEN}",
            name.len()
        ));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!(
            "{name:?} is not a hostname; it may contain letters, digits and hyphens, \
             and no dots — this is one label, not a fully-qualified name"
        ));
    }
    // A leading or trailing hyphen is invalid in a DNS label, and resolvers
    // differ on what they do with it rather than agreeing to reject it.
    if name.starts_with('-') || name.ends_with('-') {
        return Err(format!("{name:?} starts or ends with a hyphen"));
    }
    Ok(())
}

/// The live name, from the kernel rather than the file.
///
/// The file is what the board will be called after a reboot; these differ if
/// someone has run `hostname` by hand, and the interface should show what the
/// board answers to now.
pub async fn current() -> Option<String> {
    tokio::fs::read_to_string(LIVE_HOSTNAME)
        .await
        .ok()
        .map(|s| s.trim().to_string())
}

/// Renames the board, now and after the next reboot.
///
/// Both halves matter and in this order. The file first, because a live
/// rename that is not persisted is the surprise nobody expects at the next
/// boot; then `sethostname`, so `about` and the interface header agree with
/// the answer to this request rather than with the one before it.
pub async fn set(name: &str) -> Result<(), String> {
    validate(name)?;

    tokio::fs::write(HOSTNAME_FILE, format!("{name}\n"))
        .await
        .map_err(|e| format!("cannot write {HOSTNAME_FILE}: {e}"))?;

    // Writing this sysctl is `sethostname(2)`, and it is the same file the
    // `about` endpoint has always read the name back from -- so one path
    // writes and reads the live name, rather than a syscall here and a file
    // read there that could disagree.
    tokio::fs::write(LIVE_HOSTNAME, name)
        .await
        .map_err(|e| format!("the name was saved, but the running system refused it: {e}"))?;

    // mdnsd advertises the name it read at start-up, so until it is told
    // otherwise the board answers to a name it no longer has. Never fatal:
    // the rename has happened, and a board that is correctly named but
    // still advertising the old one is not a failed rename.
    restart_mdns().await;

    Ok(())
}

async fn restart_mdns() {
    let result = tokio::task::spawn_blocking(|| {
        Command::new("/etc/init.d/S50mdnsd").arg("restart").status()
    })
    .await;

    match result {
        Ok(Ok(status)) if status.success() => {}
        Ok(Ok(status)) => {
            tracing::warn!("mdnsd did not restart ({status}); it still advertises the old name")
        }
        Ok(Err(e)) => tracing::warn!("could not restart mdnsd: {e}"),
        Err(e) => tracing::warn!("could not wait for the mdnsd restart: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rename_this_exists_for_is_accepted() {
        assert!(validate("hive-a-bmc").is_ok());
        assert!(validate("turingpi").is_ok());
        assert!(validate("bmc2").is_ok());
    }

    #[test]
    fn a_qualified_name_is_not_a_hostname() {
        // mdnsd would advertise `a.b.local`, and `about` would report a name
        // that is not what anything else on the network calls the board.
        assert!(validate("hive-a-bmc.excavador.xyz").is_err());
        assert!(validate("bmc.local").is_err());
    }

    #[test]
    fn what_the_file_would_accept_and_dns_would_not() {
        for bad in [
            "",
            "hive bmc",
            "hive_bmc",
            "-leading",
            "trailing-",
            "hive/bmc",
            "hive\nbmc",
        ] {
            assert!(validate(bad).is_err(), "{bad:?} should have been refused");
        }
    }

    #[test]
    fn a_label_is_at_most_sixty_three_characters() {
        let sixty_three = "a".repeat(63);
        let sixty_four = "a".repeat(64);
        assert!(validate(&sixty_three).is_ok());
        assert!(validate(&sixty_four).is_err());
    }
}
