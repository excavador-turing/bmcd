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
//! Which time sources the board uses.
//!
//! The image ships `pool pool.ntp.org iburst`, so a board synchronises
//! straight to the public pool and there is no way to change that short of
//! editing chrony's config over SSH. The RTC covers boot, so this is not about
//! correctness at power-on; it is about the one time somebody is actually
//! standing at the board -- a WAN outage -- when chrony loses its only source
//! and anything time-sensitive drifts with no correction.
//!
//! Written as a chrony `sourcedir` file rather than by rewriting
//! `/etc/chrony.conf`. Two reasons: the config is in the read-only image, and
//! `chronyc reload sources` picks up a sourcedir change without restarting the
//! daemon or losing the discipline it has built up.

use serde::{Deserialize, Serialize};
use std::process::Command;

/// Where chrony looks for sources it did not ship with. The image's
/// `chrony.conf` names this directory; the two must agree or the file here is
/// simply never read.
const SOURCE_DIR: &str = "/mnt/overlay/chrony.d";
/// One file, ours. A `.sources` suffix is what chrony looks for.
const SOURCE_FILE: &str = "/mnt/overlay/chrony.d/bmcd.sources";
const CHRONYC: &str = "chronyc";

/// More than this is a mistake rather than a configuration: chrony polls each
/// one, and a board that talks to a dozen servers has not improved its clock.
const MAX_SERVERS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NtpConfig {
    /// In preference order. The first is written with chrony's `prefer`, so a
    /// LAN source wins over a pool that answers faster.
    pub servers: Vec<String>,
}

/// Rejects a host that chrony could not use, or that would let a caller write
/// arbitrary directives into a config file.
///
/// The second is the point: these lines are `server <value> iburst`, and a
/// value containing whitespace or a newline would add directives of its own.
/// An allow-list of what a hostname or address can contain closes that without
/// having to reason about chrony's parser.
pub fn validate(server: &str) -> Result<(), String> {
    if server.is_empty() {
        return Err("a time server cannot be an empty string".to_string());
    }
    if server.len() > 253 {
        return Err(format!("{server:?} is longer than a hostname can be"));
    }
    // Hostnames, IPv4, and IPv6 in the form chrony accepts unbracketed.
    if !server
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
    {
        return Err(format!(
            "{server:?} is not a hostname or an address; it may contain \
             letters, digits, dots, hyphens, underscores and colons"
        ));
    }
    Ok(())
}

pub fn validate_all(servers: &[String]) -> Result<(), String> {
    if servers.len() > MAX_SERVERS {
        return Err(format!(
            "{} time servers given; at most {MAX_SERVERS} are useful",
            servers.len()
        ));
    }
    for server in servers {
        validate(server)?;
    }
    Ok(())
}

/// Renders the sourcedir file.
///
/// Exposed for the tests: what this writes is a config file for another
/// daemon, and getting it wrong is only visible as a clock that quietly stops
/// being disciplined.
pub fn render(servers: &[String]) -> String {
    let mut out = String::from(
        "# Written by bmcd. Edit through the API or the Settings tab; this\n\
         # file is replaced whole on every change.\n",
    );
    for (index, server) in servers.iter().enumerate() {
        // `prefer` on the first only: chrony treats several preferred sources
        // as equals, which is not what an ordered list means.
        if index == 0 {
            out.push_str(&format!("server {server} iburst prefer\n"));
        } else {
            out.push_str(&format!("server {server} iburst\n"));
        }
    }
    out
}

/// Parses the file back. Anything that is not one of our `server` lines is
/// ignored rather than reported, so a hand-edited file degrades to "the
/// servers bmcd knows about" instead of failing the whole request.
fn parse(contents: &str) -> Vec<String> {
    contents
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("server ")?;
            rest.split_whitespace().next().map(str::to_string)
        })
        .collect()
}

/// What the board is configured to use. An empty list means the image's own
/// `pool` line is the only source, which is the shipped default.
pub async fn load() -> NtpConfig {
    match tokio::fs::read_to_string(SOURCE_FILE).await {
        Ok(contents) => NtpConfig {
            servers: parse(&contents),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => NtpConfig::default(),
        Err(e) => {
            tracing::warn!("cannot read {SOURCE_FILE}: {e}");
            NtpConfig::default()
        }
    }
}

/// Writes the servers and asks chrony to pick them up.
///
/// The reload is not optional and not deferred: a setting that takes effect at
/// the next reboot is one a person changes, sees no effect from, and changes
/// again.
pub async fn store(servers: &[String]) -> Result<(), String> {
    validate_all(servers)?;

    tokio::fs::create_dir_all(SOURCE_DIR)
        .await
        .map_err(|e| format!("cannot create {SOURCE_DIR}: {e}"))?;

    if servers.is_empty() {
        // Back to the image's own pool line: remove the file rather than
        // writing an empty one, so `load()` reports the shipped default
        // instead of "configured with nothing".
        match tokio::fs::remove_file(SOURCE_FILE).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {SOURCE_FILE}: {e}")),
        }
    } else {
        tokio::fs::write(SOURCE_FILE, render(servers))
            .await
            .map_err(|e| format!("cannot write {SOURCE_FILE}: {e}"))?;
    }

    reload().await
}

async fn reload() -> Result<(), String> {
    let output =
        tokio::task::spawn_blocking(|| Command::new(CHRONYC).args(["reload", "sources"]).output())
            .await
            .map_err(|e| format!("could not run {CHRONYC}: {e}"))?
            .map_err(|e| format!("could not run {CHRONYC}: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    // The servers are on disk and correct; only the live reload failed, and
    // the next restart will pick them up. Say exactly that rather than
    // reporting a write that did happen as a failure.
    Err(format!(
        "the servers were saved, but chrony did not reload them: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Creates the sourcedir at start-up.
///
/// chronyd starts at S49 and this daemon at S94, so on a board that has never
/// had a server list set, chronyd logs one "could not open directory" and
/// carries on with the pool from its own config. Creating it here means that
/// happens at most once per board rather than on every boot. It cannot be
/// shipped in the image: `/mnt/overlay` is a mount point, so anything the
/// rootfs holds under it is hidden the moment the real volume mounts.
pub async fn ensure_source_dir() {
    if !sourcedir_configured() {
        // An image older than the one that added the `sourcedir` line. Making
        // the directory would achieve nothing and imply otherwise.
        return;
    }
    if let Err(e) = tokio::fs::create_dir_all(SOURCE_DIR).await {
        tracing::warn!("cannot create {SOURCE_DIR}: {e}");
    }
}

/// Whether the sourcedir is even wired up, for a board running an image older
/// than the one that added the `sourcedir` line to `chrony.conf`.
pub fn sourcedir_configured() -> bool {
    std::fs::read_to_string("/etc/chrony.conf")
        .map(|c| c.lines().any(|l| l.trim().starts_with("sourcedir ")))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_server_is_the_preferred_one() {
        let rendered = render(&["192.168.77.1".into(), "pool.ntp.org".into()]);
        assert!(rendered.contains("server 192.168.77.1 iburst prefer\n"));
        assert!(rendered.contains("server pool.ntp.org iburst\n"));
        // Several preferred sources are equals to chrony, which is not what an
        // ordered list means.
        assert_eq!(rendered.matches("prefer").count(), 1, "{rendered}");
    }

    #[test]
    fn what_is_written_reads_back_the_same() {
        let servers = vec![
            "192.168.77.1".to_string(),
            "pool.ntp.org".to_string(),
            "2001:db8::1".to_string(),
        ];
        assert_eq!(parse(&render(&servers)), servers);
    }

    #[test]
    fn a_hand_edited_file_degrades_to_what_we_understand() {
        let contents = "# someone's note\n\
                        server 10.0.0.1 iburst prefer\n\
                        pool something.else iburst\n\
                        makestep 0.1 5\n\
                        server 10.0.0.2 iburst\n";
        assert_eq!(parse(contents), vec!["10.0.0.1", "10.0.0.2"]);
    }

    /// These lines are `server <value> iburst`. A value carrying a newline
    /// would append directives of its own to another daemon's config.
    #[test]
    fn a_server_cannot_smuggle_a_directive() {
        for bad in [
            "10.0.0.1\nallow all",
            "10.0.0.1 iburst\ncmdallow all",
            "10.0.0.1 offline",
            "; rm -rf /",
            "$(id)",
            "",
        ] {
            assert!(validate(bad).is_err(), "{bad:?} should have been refused");
        }
    }

    #[test]
    fn ordinary_hosts_are_accepted() {
        for good in [
            "192.168.77.1",
            "pool.ntp.org",
            "2001:db8::1",
            "time-a.example-host.net",
            "ntp_1.internal",
        ] {
            assert!(validate(good).is_ok(), "{good:?} should have been accepted");
        }
    }

    #[test]
    fn a_dozen_servers_is_a_mistake_not_a_configuration() {
        let many: Vec<String> = (0..12).map(|i| format!("ntp{i}.example.com")).collect();
        assert!(validate_all(&many).is_err());
        assert!(validate_all(&many[..8]).is_ok());
    }

    #[test]
    fn no_servers_renders_no_server_lines() {
        // Distinct from "configured with nothing": `store` removes the file so
        // the image's own pool line is the source again.
        let rendered = render(&[]);
        assert!(!rendered.contains("server "), "{rendered}");
    }
}
