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
//! Putting an address on the bridge, live, and reading what is there.
//!
//! ## Never `ifdown br0`
//!
//! The obvious way to apply a new `interfaces` file is `ifdown br0 && ifup
//! br0`. On this board `br0` is the bridge over the switch ports, and taking
//! it down removes the compute modules' ports from it: every module loses
//! its network for as long as the bridge is down. The address is a property
//! *on* the bridge, so it is changed with `ip` on the bridge that stays up,
//! and udhcpc is started or stopped by hand the way `ifup` would have done
//! it. What this module runs is exactly what the boot path arrives at; it
//! just arrives there without the detour through link-down.
//!
//! ## What "live" means
//!
//! `ip -j` is iproute2 on this image, not BusyBox, so the address and the
//! default route are read as JSON. The resolvers are `/etc/resolv.conf`.
//! Whether the board is on DHCP is whether udhcpc's pid file names a process
//! that is still alive.
use crate::app::address_document::{resolv_conf, AddressDocument, StaticAddress, INTERFACE};
use schemars::JsonSchema;
use serde::Serialize;
use std::net::Ipv4Addr;
use std::path::Path;

const UDHCPC: &str = "/sbin/udhcpc";
const UDHCPC_PID: &str = "/var/run/udhcpc.br0.pid";
pub const RESOLV_CONF: &str = "/etc/resolv.conf";
const IP: &str = "ip";
/// mdnsd is started with `-i br0` and does not watch for a new address on
/// it; restarting it is how the board re-announces itself.
const MDNSD_INIT: &str = "/etc/init.d/S50mdnsd";

/// What the bridge has right now, as opposed to what any file says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, Default)]
pub struct LiveAddress {
    /// `dhcp` when udhcpc is running, `static` otherwise. There is no third
    /// state the board can be in and still be reached.
    pub mode: String,
    /// `192.168.77.30/24`, or `None` for a bridge with no IPv4 address.
    pub address: Option<String>,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub search: Option<String>,
}

#[derive(Debug)]
pub enum ApplyError {
    Command { command: String, detail: String },
    Io(std::io::Error),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Command { command, detail } => write!(f, "`{command}` failed: {detail}"),
            ApplyError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ApplyError {}

fn run(program: &str, args: &[&str]) -> Result<String, ApplyError> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(ApplyError::Io)?;
    if !output.status.success() {
        return Err(ApplyError::Command {
            command: format!("{program} {}", args.join(" ")),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Like [`run`], for the commands whose failure means "there was nothing to
/// do": deleting a route that is not there, killing a daemon that has gone.
fn run_lenient(program: &str, args: &[&str]) {
    if let Err(e) = run(program, args) {
        tracing::debug!("{e} (ignored)");
    }
}

fn udhcpc_pid() -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(UDHCPC_PID)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Path::new(&format!("/proc/{pid}")).exists().then_some(pid)
}

/// Stop udhcpc if it is running.
///
/// It was started with `-R`, so on SIGTERM it releases the lease and runs
/// the `deconfig` script, which flushes the address it had put on the
/// bridge. Waiting for it to go is what stops a late `bound` event from
/// putting a lease back on top of the static address that follows.
async fn stop_udhcpc() {
    let Some(pid) = udhcpc_pid() else {
        return;
    };
    run_lenient("kill", &["-TERM", &pid.to_string()]);
    for _ in 0..40 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let _ = std::fs::remove_file(UDHCPC_PID);
}

fn start_udhcpc() -> Result<(), ApplyError> {
    // The same line `ifup` produces for the image's stanza, with the
    // board's name as the DHCP hostname option. `-b` backgrounds it once a
    // lease is obtained, or at once if none is, so this returns promptly.
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let mut args = vec!["-b", "-R", "-p", UDHCPC_PID, "-i", INTERFACE];
    let host_opt = format!("hostname:{hostname}");
    if !hostname.is_empty() {
        args.push("-x");
        args.push(&host_opt);
    }
    run(UDHCPC, &args).map(|_| ())
}

fn flush_address() {
    run_lenient(IP, &["-4", "route", "del", "default", "dev", INTERFACE]);
    run_lenient(IP, &["-4", "addr", "flush", "dev", INTERFACE]);
}

fn set_static(s: &StaticAddress) -> Result<(), ApplyError> {
    run(
        IP,
        &["-4", "addr", "add", &s.cidr(), "brd", "+", "dev", INTERFACE],
    )?;
    if let Some(gw) = s.gateway {
        run(
            IP,
            &[
                "-4",
                "route",
                "replace",
                "default",
                "via",
                &gw.to_string(),
                "dev",
                INTERFACE,
            ],
        )?;
    }
    std::fs::write(RESOLV_CONF, resolv_conf(&s.dns, s.search.as_deref()))
        .map_err(ApplyError::Io)?;
    Ok(())
}

/// Put a document on the bridge, without touching the bridge.
pub async fn apply(document: &AddressDocument) -> Result<(), ApplyError> {
    stop_udhcpc().await;
    flush_address();
    match document {
        AddressDocument::Dhcp => start_udhcpc()?,
        AddressDocument::Static(s) => set_static(s)?,
    }
    // Best effort: a board whose mDNS name points at the old address is a
    // nuisance, not a lockout.
    run_lenient(MDNSD_INIT, &["restart"]);
    Ok(())
}

fn parse_resolv(contents: &str) -> (Vec<Ipv4Addr>, Option<String>) {
    let mut dns = Vec::new();
    let mut search = None;
    for line in contents.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("nameserver") => {
                if let Some(ip) = words.next().and_then(|w| w.parse().ok()) {
                    dns.push(ip);
                }
            }
            Some("search") => search = words.next().map(str::to_string),
            _ => {}
        }
    }
    (dns, search)
}

/// What the bridge has right now.
pub async fn live() -> LiveAddress {
    let address = run(IP, &["-j", "-4", "addr", "show", "dev", INTERFACE])
        .ok()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|v| {
            let info = v.get(0)?.get("addr_info")?.as_array()?.first()?;
            Some(format!(
                "{}/{}",
                info.get("local")?.as_str()?,
                info.get("prefixlen")?.as_u64()?
            ))
        });
    let gateway = run(IP, &["-j", "-4", "route", "show", "default"])
        .ok()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|v| v.get(0)?.get("gateway")?.as_str()?.parse().ok());
    let (dns, search) = std::fs::read_to_string(RESOLV_CONF)
        .map(|c| parse_resolv(&c))
        .unwrap_or_default();
    LiveAddress {
        mode: if udhcpc_pid().is_some() {
            "dhcp"
        } else {
            "static"
        }
        .to_string(),
        address,
        gateway,
        dns,
        search,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolv_conf_is_read_the_way_udhcpc_writes_it() {
        let (dns, search) =
            parse_resolv("search haarlem.internal # br0\nnameserver 192.168.77.1 # br0\nnameserver 1.1.1.1 # br0\n");
        assert_eq!(search.as_deref(), Some("haarlem.internal"));
        assert_eq!(
            dns,
            vec![
                "192.168.77.1".parse::<Ipv4Addr>().unwrap(),
                "1.1.1.1".parse().unwrap()
            ]
        );
    }
}
