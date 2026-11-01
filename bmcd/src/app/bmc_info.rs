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
use crate::app::sdcard_files::SDCARD_ROOT;
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

/// What `tpi info` used to read, for when `ip` has nothing to say: the
/// IPv4 of `br0` -- link-local included, which a board that has not got a
/// lease yet still shows -- else the first other non-loopback IPv4 by name.
pub fn fallback_ipv4(addresses: &[(String, std::net::Ipv4Addr)]) -> Option<(String, String)> {
    let mut sorted: Vec<&(String, std::net::Ipv4Addr)> = addresses.iter().collect();
    sorted.sort_by(|a, b| {
        (a.0 != network_interfaces::MANAGED, &a.0).cmp(&(b.0 != network_interfaces::MANAGED, &b.0))
    });
    sorted
        .first()
        .map(|(name, ip)| (name.clone(), ip.to_string()))
}

/// [`primary_info`] for the board as it is now. When `ip` is missing or the
/// board has no global IPv4 address, the IP is read the way it used to be
/// (through the kernel's address list), so `tpi info` does not turn into
/// "Unknown" on a board that has an address of some kind. The MAC falls back
/// to what the kernel says the interface has.
pub async fn get_primary_info() -> (Option<String>, String) {
    let snapshot = tokio::task::spawn_blocking(|| network_interfaces::snapshot(&SystemExec))
        .await
        .unwrap_or_default();
    let (ip, mac) = primary_info(&snapshot);
    if let (Some(ip), Some(mac)) = (&ip, &mac) {
        return (Some(ip.clone()), mac.clone());
    }
    let mut device = snapshot.primary().map(|i| i.name.clone());
    let ip = match ip {
        Some(ip) => Some(ip),
        None => {
            let addresses: Vec<(String, std::net::Ipv4Addr)> = if_addrs::get_if_addrs()
                .unwrap_or_default()
                .into_iter()
                .filter(|i| !i.is_loopback())
                .filter_map(|i| match i.ip() {
                    std::net::IpAddr::V4(ip) => Some((i.name, ip)),
                    _ => None,
                })
                .collect();
            fallback_ipv4(&addresses).map(|(name, ip)| {
                device = Some(name);
                ip
            })
        }
    };
    let mac = match mac {
        Some(mac) => mac,
        None => get_mac_address(device.as_deref().unwrap_or(network_interfaces::MANAGED)).await,
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
    /// Whether a card is actually mounted at `/mnt/sdcard`. Only the `SD card`
    /// entry carries it; the `BMC` entry has no such question and omits it.
    ///
    /// `total_bytes` and `bytes_free` of the `SD card` entry are NOT a
    /// statement about a card when this is false: `statvfs` on the empty mount
    /// point answers for the volume underneath it, which is the BMC's own, and
    /// they are left as reported so a client that reads only the sizes behaves
    /// as it always did. Read this field instead of comparing sizes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mounted: Option<bool>,
}

/// The `SD card` entry. `mounted` is decided by the caller, from the mount
/// table's own test, never from the sizes.
fn sd_card_entry(total: u64, free: u64, mounted: bool) -> StorageInfo {
    StorageInfo {
        name: "SD card".to_string(),
        total_bytes: total,
        bytes_free: free,
        mounted: Some(mounted),
    }
}

pub fn get_storage_info() -> Vec<StorageInfo> {
    let mut info = vec![];

    if let Ok((total, free)) = get_fs_stat("/") {
        info.push(StorageInfo {
            name: "BMC".to_string(),
            total_bytes: total,
            bytes_free: free,
            mounted: None,
        });
    }

    // The entry's presence rule is unchanged: the device exists and statvfs
    // answers. Whether a card is in it is the new, separate fact.
    if Path::new("/dev/mmcblk0").exists() {
        if let Ok((total, free)) = get_fs_stat(SDCARD_ROOT) {
            let mounted = crate::app::upgrade_worker::is_mount_point(Path::new(SDCARD_ROOT));
            info.push(sd_card_entry(total, free, mounted));
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
    fn the_fallback_prefers_br0_including_link_local_then_the_first_by_name() {
        let a = |n: &str, ip: &str| (n.to_string(), ip.parse().unwrap());
        assert_eq!(
            fallback_ipv4(&[a("usb0", "192.168.8.2"), a("br0", "169.254.7.7")]),
            Some(("br0".to_string(), "169.254.7.7".to_string()))
        );
        assert_eq!(
            fallback_ipv4(&[a("br-wan", "10.0.0.2"), a("br-lan", "169.254.1.1")]),
            Some(("br-lan".to_string(), "169.254.1.1".to_string()))
        );
        assert_eq!(fallback_ipv4(&[]), None);
    }

    /// The SD card entry says whether a card is in; the BMC entry's shape is
    /// what it always was. Sizes are untouched when no card is mounted, so an
    /// older client that compares them with the BMC's keeps working.
    #[test]
    fn the_sd_card_entry_carries_mounted_and_the_bmc_entry_does_not() {
        let card = serde_json::to_value(sd_card_entry(100, 40, true)).unwrap();
        assert_eq!(
            card,
            serde_json::json!({
                "name": "SD card", "total_bytes": 100, "bytes_free": 40, "mounted": true
            })
        );

        let empty = serde_json::to_value(sd_card_entry(100, 40, false)).unwrap();
        assert_eq!(empty["mounted"], false);
        assert_eq!(empty["total_bytes"], 100);

        let bmc = serde_json::to_value(StorageInfo {
            name: "BMC".to_string(),
            total_bytes: 100,
            bytes_free: 40,
            mounted: None,
        })
        .unwrap();
        assert_eq!(
            bmc,
            serde_json::json!({"name": "BMC", "total_bytes": 100, "bytes_free": 40})
        );
    }

    #[test]
    fn a_board_with_no_address_reports_none() {
        assert_eq!(primary_info(&Snapshot::default()), (None, None));
    }
}
