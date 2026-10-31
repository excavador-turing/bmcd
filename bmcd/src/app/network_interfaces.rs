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
//!
//! Which interface is the board's own, and what is on every interface.
//!
//! The board ships with one bridge, `br0`, over the switch ports, and its
//! address lives there. A board whose `/etc/network/interfaces` was split by
//! hand (`br-wan` on one port, `br-lan` on another) has no `br0` at all, and
//! the address is somewhere else. Everything that wants "the board's
//! address" asks here, so a stock board and a split one give the same kind
//! of answer.
//!
//! The *primary* interface is `br0` when it exists and has a global IPv4
//! address; otherwise the device of the IPv4 default route, or, with no
//! default route, the first up non-loopback interface that has a global IPv4
//! address. Its address is the first global (`scope global`) one that is
//! not an alias (`br0:1`) -- a secondary address never wins over the main one.
//!
//! The parsing is pure and takes the JSON `ip -j -4 addr show` and `ip -j -4
//! route show default` print, so it is tested with captured output.
use crate::app::address_applier::Exec;
use schemars::JsonSchema;
use serde::Serialize;
use std::net::Ipv4Addr;
use std::path::Path;

/// The interface the address page, the switch and the network restart manage.
pub const MANAGED: &str = "br0";

/// An up, non-loopback interface with at least one global IPv4 address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct InterfaceInfo {
    pub name: String,
    /// CIDR form, `192.168.1.10/24`; the main address first, aliases after.
    pub addresses: Vec<String>,
    pub mac: String,
    /// Whether the IPv4 default route leaves through this interface.
    pub default_route: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultRoute {
    pub dev: String,
    pub gateway: Option<Ipv4Addr>,
    pub metric: u32,
}

/// What the network has right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Default route first, then by name.
    pub interfaces: Vec<InterfaceInfo>,
    /// The gateway of the default route that leaves through the primary
    /// interface. A default route through some other device is not this
    /// interface's gateway; with no interface at all, the best route's.
    pub gateway: Option<Ipv4Addr>,
}

impl Snapshot {
    /// The board's own interface: `br0` when it exists and has an address
    /// (so a USB modem, a VPN or a late DHCP lease elsewhere never takes the
    /// page away from the bridge it manages), otherwise the default route's
    /// device, otherwise the first.
    pub fn primary(&self) -> Option<&InterfaceInfo> {
        self.interfaces
            .iter()
            .find(|i| i.name == MANAGED)
            .or_else(|| self.interfaces.first())
    }

    /// The primary address without its prefix length.
    pub fn primary_ipv4(&self) -> Option<String> {
        let cidr = self.primary()?.addresses.first()?;
        Some(cidr.split('/').next().unwrap_or(cidr).to_string())
    }
}

pub fn parse_default_routes(json: &str) -> Vec<DefaultRoute> {
    let Ok(serde_json::Value::Array(routes)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    let mut out: Vec<DefaultRoute> = routes
        .iter()
        .filter(|r| r.get("dst").and_then(|d| d.as_str()).unwrap_or("default") == "default")
        .filter_map(|r| {
            Some(DefaultRoute {
                dev: r.get("dev")?.as_str()?.to_string(),
                gateway: r
                    .get("gateway")
                    .and_then(|g| g.as_str())
                    .and_then(|g| g.parse().ok()),
                metric: r.get("metric").and_then(|m| m.as_u64()).unwrap_or(0) as u32,
            })
        })
        .collect();
    out.sort_by_key(|r| r.metric);
    out
}

pub fn parse_interfaces(addr_json: &str, routes: &[DefaultRoute]) -> Vec<InterfaceInfo> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(addr_json) else {
        return Vec::new();
    };
    let mut found: Vec<InterfaceInfo> = Vec::new();
    for entry in &entries {
        let Some(name) = entry.get("ifname").and_then(|n| n.as_str()) else {
            continue;
        };
        let flags: Vec<&str> = entry
            .get("flags")
            .and_then(|f| f.as_array())
            .map(|f| f.iter().filter_map(|f| f.as_str()).collect())
            .unwrap_or_default();
        if !flags.contains(&"UP") || flags.contains(&"LOOPBACK") {
            continue;
        }
        // (is_alias, cidr): the main address of the interface sorts first.
        let mut addresses: Vec<(bool, String)> = Vec::new();
        for info in entry
            .get("addr_info")
            .and_then(|a| a.as_array())
            .into_iter()
            .flatten()
        {
            if info
                .get("family")
                .and_then(|f| f.as_str())
                .unwrap_or("inet")
                != "inet"
                || info.get("scope").and_then(|s| s.as_str()) != Some("global")
            {
                continue;
            }
            let (Some(local), Some(prefix)) = (
                info.get("local").and_then(|l| l.as_str()),
                info.get("prefixlen").and_then(|p| p.as_u64()),
            ) else {
                continue;
            };
            let alias = info.get("secondary").and_then(|s| s.as_bool()) == Some(true)
                || info
                    .get("label")
                    .and_then(|l| l.as_str())
                    .is_some_and(|label| label != name);
            addresses.push((alias, format!("{local}/{prefix}")));
        }
        if addresses.is_empty() {
            continue;
        }
        addresses.sort_by_key(|(alias, _)| *alias);
        found.push(InterfaceInfo {
            name: name.to_string(),
            addresses: addresses.into_iter().map(|(_, a)| a).collect(),
            mac: entry
                .get("address")
                .and_then(|a| a.as_str())
                .unwrap_or_default()
                .to_string(),
            default_route: false,
        });
    }
    if let Some(route) = routes
        .iter()
        .find(|r| found.iter().any(|i| i.name == r.dev))
    {
        for interface in &mut found {
            interface.default_route = interface.name == route.dev;
        }
    }
    found.sort_by(|a, b| {
        b.default_route
            .cmp(&a.default_route)
            .then_with(|| a.name.cmp(&b.name))
    });
    found
}

/// Parse both outputs into a [`Snapshot`].
pub fn parse_snapshot(addr_json: &str, route_json: &str) -> Snapshot {
    let routes = parse_default_routes(route_json);
    let interfaces = parse_interfaces(addr_json, &routes);
    let mut snapshot = Snapshot {
        interfaces,
        gateway: None,
    };
    snapshot.gateway = match snapshot.primary() {
        Some(primary) => routes
            .iter()
            .find(|r| r.dev == primary.name)
            .and_then(|r| r.gateway),
        None => routes.first().and_then(|r| r.gateway),
    };
    snapshot
}

/// Ask the kernel. A failed `ip` is an empty answer, the same as a board with
/// no address.
pub fn snapshot(exec: &dyn Exec) -> Snapshot {
    let addr = exec
        .run("ip", &["-j", "-4", "addr", "show"])
        .unwrap_or_default();
    let route = exec
        .run("ip", &["-j", "-4", "route", "show", "default"])
        .unwrap_or_default();
    parse_snapshot(&addr, &route)
}

/// Whether the managed interface exists.
pub fn managed_present(net_class: &Path) -> bool {
    net_class.join(MANAGED).exists()
}

/// The bridges in `net_class`, by name.
fn bridges(net_class: &Path) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(net_class) else {
        return Vec::new();
    };
    let mut names: Vec<String> = dir
        .flatten()
        .filter(|e| e.path().join("bridge").is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// `br0` is not there: say so, and say what is.
///
/// The interfaces with an address come first, default route leading, then
/// any bridge that has none. `tail` is what the caller manages and what to
/// do about it.
pub fn missing_message(net_class: &Path, snapshot: &Snapshot, tail: &str) -> String {
    let mut names: Vec<String> = snapshot.interfaces.iter().map(|i| i.name.clone()).collect();
    for bridge in bridges(net_class) {
        if !names.contains(&bridge) {
            names.push(bridge);
        }
    }
    let has = if names.is_empty() {
        "it has no bridge and no interface with an address".to_string()
    } else {
        format!("it has: {}", names.join(", "))
    };
    format!("This board has no {MANAGED} bridge ({has}). {tail}")
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A stock board: `br0` carries the address and the default route.
    pub const ADDR_STOCK: &str = r#"[
      {"ifindex":1,"ifname":"lo","flags":["LOOPBACK","UP","LOWER_UP"],"operstate":"UNKNOWN","address":"00:00:00:00:00:00",
       "addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host","label":"lo"}]},
      {"ifindex":2,"ifname":"node1","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"operstate":"UP","address":"aa:bb:cc:00:00:01","addr_info":[]},
      {"ifindex":6,"ifname":"br0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"operstate":"UP","address":"aa:bb:cc:00:00:10",
       "addr_info":[{"family":"inet","local":"192.168.77.30","prefixlen":24,"broadcast":"192.168.77.255","scope":"global","dynamic":true,"label":"br0","valid_life_time":86000}]}
    ]"#;
    pub const ROUTE_STOCK: &str = r#"[{"dst":"default","gateway":"192.168.77.1","dev":"br0","protocol":"dhcp","metric":0,"flags":[]}]"#;

    /// The user's board: no `br0`; `br-wan` on ge0 holds the default route,
    /// `br-lan` on ge1 has its own network.
    pub const ADDR_SPLIT: &str = r#"[
      {"ifindex":1,"ifname":"lo","flags":["LOOPBACK","UP","LOWER_UP"],"operstate":"UNKNOWN","address":"00:00:00:00:00:00",
       "addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host","label":"lo"}]},
      {"ifindex":3,"ifname":"ge0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"operstate":"UP","address":"aa:bb:cc:00:00:20","addr_info":[]},
      {"ifindex":7,"ifname":"br-lan","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"operstate":"UP","address":"aa:bb:cc:00:00:31",
       "addr_info":[{"family":"inet","local":"192.168.1.10","prefixlen":24,"scope":"global","label":"br-lan"}]},
      {"ifindex":8,"ifname":"br-wan","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"operstate":"UP","address":"aa:bb:cc:00:00:30",
       "addr_info":[{"family":"inet","local":"10.20.0.5","prefixlen":16,"scope":"global","dynamic":true,"label":"br-wan"}]}
    ]"#;
    pub const ROUTE_SPLIT: &str = r#"[{"dst":"default","gateway":"10.20.0.1","dev":"br-wan","protocol":"dhcp","metric":100,"flags":[]}]"#;

    #[test]
    fn a_stock_board_reports_br0_as_primary() {
        let snapshot = parse_snapshot(ADDR_STOCK, ROUTE_STOCK);
        let primary = snapshot.primary().expect("an interface");
        assert_eq!(primary.name, "br0");
        assert_eq!(primary.addresses, vec!["192.168.77.30/24"]);
        assert_eq!(primary.mac, "aa:bb:cc:00:00:10");
        assert!(primary.default_route);
        assert_eq!(snapshot.gateway, Some("192.168.77.1".parse().unwrap()));
        assert_eq!(snapshot.primary_ipv4().as_deref(), Some("192.168.77.30"));
        // Loopback and the ports without an address are not listed.
        assert_eq!(snapshot.interfaces.len(), 1);
    }

    #[test]
    fn a_split_board_takes_the_default_routes_bridge_as_primary() {
        let snapshot = parse_snapshot(ADDR_SPLIT, ROUTE_SPLIT);
        let names: Vec<&str> = snapshot
            .interfaces
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        // Default route first, although br-lan sorts before br-wan.
        assert_eq!(names, vec!["br-wan", "br-lan"]);
        assert!(snapshot.interfaces[0].default_route);
        assert!(!snapshot.interfaces[1].default_route);
        assert_eq!(snapshot.primary().unwrap().addresses, vec!["10.20.0.5/16"]);
        assert_eq!(snapshot.gateway, Some("10.20.0.1".parse().unwrap()));
        assert_eq!(snapshot.primary_ipv4().as_deref(), Some("10.20.0.5"));
    }

    #[test]
    fn without_a_default_route_the_first_interface_by_name_is_primary() {
        let snapshot = parse_snapshot(ADDR_SPLIT, "[]");
        let names: Vec<&str> = snapshot
            .interfaces
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(names, vec!["br-lan", "br-wan"]);
        assert!(snapshot.interfaces.iter().all(|i| !i.default_route));
        assert_eq!(snapshot.primary().unwrap().name, "br-lan");
        assert_eq!(snapshot.gateway, None);
        // And an `ip` that printed nothing at all is the same as no route.
        assert_eq!(
            parse_snapshot(ADDR_SPLIT, "").primary().unwrap().name,
            "br-lan"
        );
    }

    #[test]
    fn a_default_route_through_an_interface_with_no_address_is_ignored() {
        let route = r#"[{"dst":"default","gateway":"1.2.3.4","dev":"wg0","metric":5}]"#;
        let snapshot = parse_snapshot(ADDR_SPLIT, route);
        assert_eq!(snapshot.primary().unwrap().name, "br-lan");
        assert!(snapshot.interfaces.iter().all(|i| !i.default_route));
    }

    #[test]
    fn an_alias_or_link_local_address_never_wins_over_the_main_one() {
        // The alias is listed FIRST by the kernel here, and a link-local
        // address sits between the two.
        let addr = r#"[
          {"ifindex":6,"ifname":"br0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"address":"aa:bb:cc:00:00:10",
           "addr_info":[
             {"family":"inet","local":"10.9.9.9","prefixlen":24,"scope":"global","label":"br0:1"},
             {"family":"inet","local":"169.254.3.4","prefixlen":16,"scope":"link","label":"br0"},
             {"family":"inet","local":"172.16.0.7","prefixlen":24,"scope":"global","secondary":true,"label":"br0"},
             {"family":"inet","local":"192.168.77.30","prefixlen":24,"scope":"global","label":"br0"}
           ]}
        ]"#;
        let snapshot = parse_snapshot(addr, ROUTE_STOCK);
        let primary = snapshot.primary().unwrap();
        assert_eq!(primary.addresses[0], "192.168.77.30/24");
        assert_eq!(primary.addresses.len(), 3, "link-local is not listed");
        assert_eq!(snapshot.primary_ipv4().as_deref(), Some("192.168.77.30"));
    }

    /// A USB modem or a VPN holds the default route; the bridge the page
    /// manages is still the board's own interface.
    #[test]
    fn br0_stays_primary_when_another_interface_holds_the_default_route() {
        let addr = r#"[
          {"ifindex":6,"ifname":"br0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"address":"aa:bb:cc:00:00:10",
           "addr_info":[{"family":"inet","local":"192.168.77.30","prefixlen":24,"scope":"global","label":"br0"}]},
          {"ifindex":9,"ifname":"usb0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"address":"aa:bb:cc:00:00:40",
           "addr_info":[{"family":"inet","local":"192.168.8.2","prefixlen":24,"scope":"global","label":"usb0"}]}
        ]"#;
        let route = r#"[{"dst":"default","gateway":"192.168.8.1","dev":"usb0","metric":10}]"#;
        let snapshot = parse_snapshot(addr, route);
        assert_eq!(snapshot.primary().unwrap().name, "br0");
        assert_eq!(snapshot.primary_ipv4().as_deref(), Some("192.168.77.30"));
        // The list still says who holds the default route, and the modem's
        // gateway is not br0's.
        assert_eq!(snapshot.interfaces[0].name, "usb0");
        assert!(snapshot.interfaces[0].default_route);
        assert_eq!(snapshot.gateway, None);
    }

    #[test]
    fn br0_without_an_address_yields_to_the_default_routes_device() {
        let addr = r#"[
          {"ifindex":6,"ifname":"br0","flags":["BROADCAST","MULTICAST","UP"],"address":"aa:bb:cc:00:00:10","addr_info":[]},
          {"ifindex":9,"ifname":"usb0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"address":"aa:bb:cc:00:00:40",
           "addr_info":[{"family":"inet","local":"192.168.8.2","prefixlen":24,"scope":"global","label":"usb0"}]}
        ]"#;
        let route = r#"[{"dst":"default","gateway":"192.168.8.1","dev":"usb0","metric":10}]"#;
        let snapshot = parse_snapshot(addr, route);
        assert_eq!(snapshot.primary().unwrap().name, "usb0");
        assert_eq!(snapshot.gateway, Some("192.168.8.1".parse().unwrap()));
    }

    #[test]
    fn a_gateway_through_a_device_with_no_address_is_not_reported() {
        let route = r#"[{"dst":"default","gateway":"1.2.3.4","dev":"wg0","metric":5}]"#;
        let snapshot = parse_snapshot(ADDR_SPLIT, route);
        assert_eq!(snapshot.gateway, None);
        // With no address anywhere, the route is all there is to report.
        assert_eq!(
            parse_snapshot("[]", route).gateway,
            Some("1.2.3.4".parse().unwrap())
        );
    }

    #[test]
    fn a_down_interface_is_not_listed() {
        let addr = r#"[{"ifindex":6,"ifname":"br0","flags":["BROADCAST","MULTICAST"],"address":"aa:bb:cc:00:00:10",
          "addr_info":[{"family":"inet","local":"192.168.77.30","prefixlen":24,"scope":"global","label":"br0"}]}]"#;
        assert!(parse_snapshot(addr, ROUTE_STOCK).primary().is_none());
    }

    #[test]
    fn the_interface_list_is_the_same_list_for_the_page() {
        let snapshot = parse_snapshot(ADDR_SPLIT, ROUTE_SPLIT);
        let json = serde_json::to_value(&snapshot.interfaces).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"name":"br-wan","addresses":["10.20.0.5/16"],"mac":"aa:bb:cc:00:00:30","default_route":true},
                {"name":"br-lan","addresses":["192.168.1.10/24"],"mac":"aa:bb:cc:00:00:31","default_route":false}
            ])
        );
    }

    #[test]
    fn the_missing_message_lists_what_is_there_default_route_first() {
        let dir = tempdir::TempDir::new("net-class").expect("temp dir");
        // A bridge with no address still counts as something the board has.
        std::fs::create_dir_all(dir.path().join("br-mgmt/bridge")).unwrap();
        std::fs::create_dir_all(dir.path().join("ge0")).unwrap();
        let snapshot = parse_snapshot(ADDR_SPLIT, ROUTE_SPLIT);
        assert_eq!(
            missing_message(dir.path(), &snapshot, "Do something else."),
            "This board has no br0 bridge (it has: br-wan, br-lan, br-mgmt). Do something else."
        );
        assert!(!managed_present(dir.path()));
        std::fs::create_dir_all(dir.path().join("br0")).unwrap();
        assert!(managed_present(dir.path()));
        let empty = tempdir::TempDir::new("net-class-empty").expect("temp dir");
        assert!(missing_message(empty.path(), &Snapshot::default(), "x")
            .contains("no bridge and no interface"));
    }
}
