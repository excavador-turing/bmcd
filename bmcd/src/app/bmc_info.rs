// Copyright 2023 Turing Machines
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

use std::path::Path;

use crate::app::address_applier::SystemExec;
use crate::app::network_interfaces::{self, Snapshot};
use schemars::JsonSchema;
use serde::Serialize;

/// The IPv4 address and MAC `tpi info` and `/api/bmc?opt=info` report: those
/// of the board's primary interface.
///
/// That is the device of the IPv4 default route, or failing that the first up
/// interface with a global IPv4 address -- `br0` on a stock board, and
/// whichever bridge carries the default route on one whose
/// `/etc/network/interfaces` was split by hand. It used to be `br0` only, so
/// a board without one reported "Unknown".
pub fn primary_info(snapshot: &Snapshot) -> (Option<String>, Option<String>) {
    let mac = snapshot
        .primary()
        .map(|i| i.mac.clone())
        .filter(|m| !m.is_empty());
    (snapshot.primary_ipv4(), mac)
}

/// [`primary_info`] for the board as it is now. The MAC falls back to what
/// the kernel says `br0` has when `ip` had nothing to say.
pub async fn get_primary_info() -> (Option<String>, String) {
    let snapshot = tokio::task::spawn_blocking(|| network_interfaces::snapshot(&SystemExec))
        .await
        .unwrap_or_default();
    let (ip, mac) = primary_info(&snapshot);
    let mac = match mac {
        Some(mac) => mac,
        None => get_mac_address(network_interfaces::MANAGED).await,
    };
    (ip, mac)
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NetInfo {
    device: String,
    ip: String,
    mac: String,
}

pub async fn get_net_interfaces() -> Vec<NetInfo> {
    let mut result = Vec::new();
    let Some(interfaces) = if_addrs::get_if_addrs().ok() else {
        return result;
    };

    for interface in interfaces
        .iter()
        .filter(|i| !i.is_loopback() || i.is_link_local())
    {
        result.push(NetInfo {
            device: interface.name.clone(),
            ip: interface.ip().to_string(),
            mac: get_mac_address(&interface.name).await,
        });
    }

    result
}

pub async fn get_mac_address(interface: &str) -> String {
    tokio::fs::read_to_string(format!("/sys/class/net/{}/address", interface))
        .await
        .unwrap_or("Unknown".to_owned())
        .trim_end_matches(['\0', '\n'])
        .to_string()
}

pub fn get_fs_stat(device: &str) -> anyhow::Result<(u64, u64)> {
    let stat = nix::sys::statvfs::statvfs(device)?;
    let total = stat.blocks() as u64 * stat.fragment_size() as u64;
    let free = stat.blocks_free() as u64 * stat.fragment_size() as u64;

    Ok((total, free))
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StorageInfo {
    name: String,
    total_bytes: u64,
    bytes_free: u64,
}

pub fn get_storage_info() -> Vec<StorageInfo> {
    let mut info = vec![];

    if let Ok((total, free)) = get_fs_stat("/") {
        info.push(StorageInfo {
            name: "BMC".to_string(),
            total_bytes: total,
            bytes_free: free,
        });
    }

    if Path::new("/dev/mmcblk0").exists() {
        if let Ok((total, free)) = get_fs_stat("/mnt/sdcard") {
            info.push(StorageInfo {
                name: "SD card".to_string(),
                total_bytes: total,
                bytes_free: free,
            });
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::network_interfaces::parse_snapshot;
    use crate::app::network_interfaces::tests::{ADDR_SPLIT, ADDR_STOCK, ROUTE_SPLIT, ROUTE_STOCK};

    #[test]
    fn tpi_info_on_a_stock_board_is_br0s_address_and_mac() {
        let (ip, mac) = primary_info(&parse_snapshot(ADDR_STOCK, ROUTE_STOCK));
        assert_eq!(ip.as_deref(), Some("192.168.77.30"));
        assert_eq!(mac.as_deref(), Some("aa:bb:cc:00:00:10"));
    }

    /// The report this exists for: no `br0`, and `tpi info` said "Unknown".
    #[test]
    fn tpi_info_on_a_split_board_is_the_default_routes_bridge() {
        let (ip, mac) = primary_info(&parse_snapshot(ADDR_SPLIT, ROUTE_SPLIT));
        assert_eq!(ip.as_deref(), Some("10.20.0.5"));
        assert_eq!(mac.as_deref(), Some("aa:bb:cc:00:00:30"));
        // Even with no default route there is an address to report.
        let (ip, _) = primary_info(&parse_snapshot(ADDR_SPLIT, "[]"));
        assert_eq!(ip.as_deref(), Some("192.168.1.10"));
    }

    #[test]
    fn a_board_with_no_address_reports_none() {
        assert_eq!(primary_info(&Snapshot::default()), (None, None));
    }
}
