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
//! default route are read as JSON. What is reported is the *primary*
//! interface's (see [`super::network_interfaces`]), which is `br0` on a stock
//! board but not on one whose `interfaces` file was split by hand; what is
//! *changed* is always `br0`, and a board without one is refused before
//! anything runs. The resolvers are `/etc/resolv.conf`.
//! Whether the board is on DHCP is whether udhcpc's pid file names a process
//! that is still alive.
use crate::app::address_document::{resolv_conf, AddressDocument, StaticAddress, INTERFACE};
use crate::app::network_interfaces::{self, InterfaceInfo, Snapshot};
use schemars::JsonSchema;
use serde::Serialize;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const UDHCPC: &str = "/sbin/udhcpc";
const UDHCPC_PID: &str = "/var/run/udhcpc.br0.pid";
pub const RESOLV_CONF: &str = "/etc/resolv.conf";
const HOSTNAME_FILE: &str = "/etc/hostname";
/// Where the kernel lists network interfaces.
pub const NET_CLASS: &str = "/sys/class/net";
const IP: &str = "ip";
/// mdnsd is started with `-i br0` and does not watch for a new address on
/// it; restarting it is how the board re-announces itself.
const MDNSD_INIT: &str = "/etc/init.d/S50mdnsd";

/// What the primary interface has right now, as opposed to what any file
/// says.
///
/// The primary interface is the device of the IPv4 default route, or failing
/// that the first up interface with a global IPv4 address. On a stock board
/// that is `br0`; on a board whose `interfaces` file was split by hand
/// (`br-wan`, `br-lan`) it is whichever bridge carries the default route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, Default)]
pub struct LiveAddress {
    /// `dhcp` when udhcpc is running on `br0`, `static` otherwise.
    pub mode: String,
    /// The primary interface's global IPv4 address in CIDR form
    /// (`192.168.77.30/24`), or `None` when no interface has one.
    pub address: Option<String>,
    /// The device `address` was read from, e.g. `br0` or `br-wan`.
    pub interface: Option<String>,
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

/// Runs a program. A trait so the commands this module builds, and the
/// output it reads, can be checked without a board.
pub trait Exec: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, ApplyError>;
}

/// Runs the real thing.
pub struct SystemExec;

impl Exec for SystemExec {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, ApplyError> {
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
}

/// What it takes to change and read the managed interface: the commands it
/// runs and the files it touches. [`Applier::system`] is the board;
/// anything else is a test.
#[derive(Clone)]
pub struct Applier {
    exec: Arc<dyn Exec>,
    udhcpc_pid: PathBuf,
    resolv_conf: PathBuf,
    hostname: PathBuf,
    net_class: PathBuf,
}

impl Applier {
    pub fn system() -> Self {
        Applier {
            exec: Arc::new(SystemExec),
            udhcpc_pid: UDHCPC_PID.into(),
            resolv_conf: RESOLV_CONF.into(),
            hostname: HOSTNAME_FILE.into(),
            net_class: NET_CLASS.into(),
        }
    }

    /// Everything the board has, but somewhere else and run by `exec`.
    #[cfg(test)]
    pub fn with(exec: Arc<dyn Exec>, root: &Path) -> Self {
        Applier {
            exec,
            udhcpc_pid: root.join("udhcpc.br0.pid"),
            resolv_conf: root.join("resolv.conf"),
            hostname: root.join("hostname"),
            net_class: root.join("net"),
        }
    }

    /// Whether the interface this module manages exists.
    pub fn managed_present(&self) -> bool {
        network_interfaces::managed_present(&self.net_class)
    }

    pub fn snapshot(&self) -> Snapshot {
        network_interfaces::snapshot(self.exec.as_ref())
    }

    /// Why the managed interface cannot be changed, or `None` when it can.
    /// `tail` says what the caller manages and what to do instead.
    pub fn missing_message(&self, tail: &str) -> Option<String> {
        if self.managed_present() {
            return None;
        }
        Some(network_interfaces::missing_message(
            &self.net_class,
            &self.snapshot(),
            tail,
        ))
    }

    /// For the commands whose failure means "there was
    /// nothing to do": deleting a route that is not there, killing a daemon
    /// that has gone.
    fn run_lenient(&self, program: &str, args: &[&str]) {
        if let Err(e) = self.exec.run(program, args) {
            tracing::debug!("{e} (ignored)");
        }
    }

    fn udhcpc_pid(&self) -> Option<u32> {
        let pid: u32 = std::fs::read_to_string(&self.udhcpc_pid)
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
    async fn stop_udhcpc(&self) {
        let Some(pid) = self.udhcpc_pid() else {
            return;
        };
        self.run_lenient("kill", &["-TERM", &pid.to_string()]);
        for _ in 0..40 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = std::fs::remove_file(&self.udhcpc_pid);
    }

    fn start_udhcpc(&self) -> Result<(), ApplyError> {
        // The same line `ifup` produces for the image's stanza, with the
        // board's name as the DHCP hostname option. `-b` backgrounds it once a
        // lease is obtained, or at once if none is, so this returns promptly.
        let hostname = std::fs::read_to_string(&self.hostname)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let pid_file = self.udhcpc_pid.to_string_lossy().into_owned();
        let mut args = vec!["-b", "-R", "-p", &pid_file, "-i", INTERFACE];
        let host_opt = format!("hostname:{hostname}");
        if !hostname.is_empty() {
            args.push("-x");
            args.push(&host_opt);
        }
        self.exec.run(UDHCPC, &args).map(|_| ())
    }

    fn flush_address(&self) {
        self.run_lenient(IP, &["-4", "route", "del", "default", "dev", INTERFACE]);
        self.run_lenient(IP, &["-4", "addr", "flush", "dev", INTERFACE]);
    }

    fn set_static(&self, s: &StaticAddress) -> Result<(), ApplyError> {
        self.exec.run(
            IP,
            &["-4", "addr", "add", &s.cidr(), "brd", "+", "dev", INTERFACE],
        )?;
        if let Some(gw) = s.gateway {
            self.exec.run(
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
        std::fs::write(&self.resolv_conf, resolv_conf(&s.dns, s.search.as_deref()))
            .map_err(ApplyError::Io)?;
        Ok(())
    }

    /// Put a document on the bridge, without touching the bridge.
    pub async fn apply(&self, document: &AddressDocument) -> Result<(), ApplyError> {
        self.stop_udhcpc().await;
        self.flush_address();
        match document {
            AddressDocument::Dhcp => self.start_udhcpc()?,
            AddressDocument::Static(s) => self.set_static(s)?,
        }
        // Best effort: a board whose mDNS name points at the old address is a
        // nuisance, not a lockout.
        self.run_lenient(MDNSD_INIT, &["restart"]);
        Ok(())
    }

    /// What the primary interface has right now.
    pub async fn live(&self) -> LiveAddress {
        let snapshot = self.snapshot();
        let primary = snapshot.primary();
        let (dns, search) = std::fs::read_to_string(&self.resolv_conf)
            .map(|c| parse_resolv(&c))
            .unwrap_or_default();
        LiveAddress {
            mode: if self.udhcpc_pid().is_some() {
                "dhcp"
            } else {
                "static"
            }
            .to_string(),
            address: primary.and_then(|i| i.addresses.first().cloned()),
            interface: primary.map(|i| i.name.clone()),
            gateway: snapshot.gateway,
            dns,
            search,
        }
    }

    /// Every up interface with a global IPv4 address.
    pub fn interfaces(&self) -> Vec<InterfaceInfo> {
        self.snapshot().interfaces
    }
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

/// A board that is not there: canned `ip` output, and a record of what was
/// asked to run.
#[cfg(test)]
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    pub struct FakeExec {
        pub addr: String,
        pub route: String,
        /// A command line that fails, e.g. `ip -4 addr add`.
        pub fail_prefix: Mutex<Option<String>>,
        pub calls: Mutex<Vec<String>>,
    }

    impl FakeExec {
        pub fn board(addr: &str, route: &str) -> Arc<Self> {
            Arc::new(FakeExec {
                addr: addr.to_string(),
                route: route.to_string(),
                fail_prefix: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
            })
        }

        pub fn failing(addr: &str, route: &str, prefix: &str) -> Arc<Self> {
            Arc::new(FakeExec {
                addr: addr.to_string(),
                route: route.to_string(),
                fail_prefix: Mutex::new(Some(prefix.to_string())),
                calls: Mutex::new(Vec::new()),
            })
        }

        /// Stop failing.
        pub fn heal(&self) {
            *self.fail_prefix.lock().unwrap() = None;
        }

        /// What was run, mutating commands only (the reads are not news).
        pub fn changes(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| !c.starts_with("ip -j"))
                .cloned()
                .collect()
        }
    }

    impl Exec for FakeExec {
        fn run(&self, program: &str, args: &[&str]) -> Result<String, ApplyError> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.lock().unwrap().push(line.clone());
            if let Some(prefix) = &*self.fail_prefix.lock().unwrap() {
                if line.starts_with(prefix.as_str()) {
                    return Err(ApplyError::Command {
                        command: line,
                        detail: "Cannot find device \"br0\"".to_string(),
                    });
                }
            }
            if line.starts_with("ip -j -4 addr") {
                return Ok(self.addr.clone());
            }
            if line.starts_with("ip -j -4 route") {
                return Ok(self.route.clone());
            }
            Ok(String::new())
        }
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
