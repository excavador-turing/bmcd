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
//! The board's own address, as one document.
//!
//! The BMC reaches the network through `br0`, the bridge over the switch's
//! seven ports, and until now the only way to give it a fixed address was an
//! SSH session and a hand-edited `/etc/network/interfaces` -- which is what a
//! user on a 2.4 board did on 2026-09-21 and then asked for a page for.
//!
//! This is the document that page edits, and everything it can be rendered
//! to or read from:
//!
//! * the **ifupdown stanza** in `/etc/network/interfaces`, which is what the
//!   board brings the network up from at boot (`S40network` runs `ifup -a`),
//!   and which `S00dsa` migrates between formats before that. Writing that
//!   file, in the shape it already has, is the whole of persistence; nothing
//!   new has to run at boot for the address to be right.
//! * the **live** commands, which [`super::address_applier`] runs to change
//!   the address without bringing the bridge down -- `ifdown br0` would take
//!   the compute modules off the wire with it.
//!
//! ## What the board refuses
//!
//! The address is how you reach the page you are changing it from, so a
//! document that cannot possibly be reached is refused outright rather than
//! applied and reverted: a prefix that leaves no host bits, a gateway that is
//! not on the address's own subnet, an address that is the network or the
//! broadcast of its prefix. Everything else that is merely unwise -- no
//! resolver on a static address, so `pool.ntp.org` will never resolve -- is a
//! warning the client shows and the board does not enforce.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

/// The bridge the BMC's address sits on. There is one, and its name is the
/// image's.
pub const INTERFACE: &str = "br0";
/// Its members, as the image's own `interfaces` file lists them. Written back
/// verbatim so `S00dsa`, which greps for this line, keeps recognising the file.
pub const BRIDGE_PORTS: &str = "node1 node2 node3 node4 ge0 ge1";
/// The first line of a file this daemon wrote. A file without it was written
/// by hand, and the page says so before offering to replace it.
pub const MARKER: &str = "# Written by bmcd: the BMC's own address.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum AddressDocument {
    /// Ask the network for an address. The image's default.
    Dhcp,
    /// A fixed address, gateway and resolvers.
    Static(StaticAddress),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StaticAddress {
    pub address: Ipv4Addr,
    /// `24` for a /24. Between 8 and 30: a /31 or /32 leaves no room for a
    /// gateway, and nothing smaller than a /8 is a LAN.
    pub prefix: u8,
    /// Where packets for other networks go. Optional: a board that only ever
    /// talks to its own subnet has none, and says so instead of inventing one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<Ipv4Addr>,
    /// Resolvers, in order. Empty is allowed and warned about.
    #[serde(default)]
    pub dns: Vec<Ipv4Addr>,
    /// A search domain for `/etc/resolv.conf`, if the network has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

/// One sentence saying why the board will not apply a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Refusal {
    pub reason: String,
}

/// Something the board would apply but the person should know about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Warning {
    pub reason: String,
}

pub const MIN_PREFIX: u8 = 8;
pub const MAX_PREFIX: u8 = 30;

impl StaticAddress {
    fn mask(&self) -> u32 {
        if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(self.prefix))
        }
    }

    pub fn netmask(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.mask())
    }

    fn network(&self) -> u32 {
        u32::from(self.address) & self.mask()
    }

    fn broadcast(&self) -> u32 {
        self.network() | !self.mask()
    }

    fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == self.network()
    }

    /// `192.168.1.20/24`, the form `ip` takes and people type.
    pub fn cidr(&self) -> String {
        format!("{}/{}", self.address, self.prefix)
    }
}

impl AddressDocument {
    /// Why the board will not apply this, or `None`.
    ///
    /// The rules are about reachability and nothing else. A document that
    /// passes here may still be *wrong* -- an address nobody routes to -- and
    /// that is what the confirm window is for.
    pub fn refusal(&self) -> Option<Refusal> {
        let AddressDocument::Static(s) = self else {
            return None;
        };
        let reason = if !(MIN_PREFIX..=MAX_PREFIX).contains(&s.prefix) {
            format!(
                "a /{} prefix cannot be used: it must be between /{MIN_PREFIX} and /{MAX_PREFIX}, \
                 so the network has room for the board and a gateway",
                s.prefix
            )
        } else if s.address.is_loopback()
            || s.address.is_multicast()
            || s.address.is_unspecified()
            || s.address.is_broadcast()
            || s.address.is_link_local()
        {
            format!("{} is not an address a board can be reached at", s.address)
        } else if u32::from(s.address) == s.network() {
            format!(
                "{} is the network address of {}/{}, not a host on it",
                s.address,
                Ipv4Addr::from(s.network()),
                s.prefix
            )
        } else if u32::from(s.address) == s.broadcast() {
            format!(
                "{} is the broadcast address of {}/{}, not a host on it",
                s.address,
                Ipv4Addr::from(s.network()),
                s.prefix
            )
        } else if let Some(gw) = s.gateway.filter(|gw| !s.contains(*gw)) {
            format!(
                "the gateway {gw} is not on {}/{}; the board could never reach it",
                Ipv4Addr::from(s.network()),
                s.prefix
            )
        } else if s.gateway == Some(s.address) {
            format!(
                "the gateway cannot be the board's own address {}",
                s.address
            )
        } else if s.search.as_deref().is_some_and(|d| {
            d.is_empty()
                || !d
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        }) {
            "the search domain may contain letters, digits, dots and hyphens only".to_string()
        } else {
            return None;
        };
        Some(Refusal { reason })
    }

    /// What the board would apply, and the person should still hear.
    pub fn warnings(&self) -> Vec<Warning> {
        let mut out = Vec::new();
        if let AddressDocument::Static(s) = self {
            if s.gateway.is_none() {
                out.push(Warning {
                    reason: "no gateway: the board will reach its own subnet and nothing beyond \
                             it, which includes the public NTP pool and any firmware source on \
                             the internet"
                        .into(),
                });
            }
            if s.dns.is_empty() {
                out.push(Warning {
                    reason: "no resolver: names will not resolve on the board, so `pool.ntp.org` \
                             and a firmware source given by name will fail until one is added"
                        .into(),
                });
            }
        }
        out
    }

    /// The `/etc/network/interfaces` this document means.
    ///
    /// The DHCP stanza is the image's own, line for line, so a board put back
    /// on DHCP has the file it shipped with plus the marker. The static one
    /// keeps every line ifupdown-ng needs to bring the bridge up the same way
    /// -- the ports, the NFS check, the wait -- and adds the address. The
    /// resolvers go in as an `up` hook, because this image has no resolvconf
    /// and its ifupdown-ng has no executor that would write them; the same
    /// line is also kept as `dns-nameservers` so it can be read back without
    /// parsing a shell command.
    pub fn render(&self) -> String {
        let mut out = format!(
            "{MARKER}\n\
             # Edit through the API or the Network tab; this file is replaced whole on\n\
             # every confirmed change. `mode` here is what the board boots with.\n\
             \n\
             auto lo\n\
             iface lo inet loopback\n\
             \n\
             auto {INTERFACE}\n"
        );
        match self {
            AddressDocument::Dhcp => {
                out.push_str(&format!(
                    "iface {INTERFACE} inet dhcp\n\
                     \x20 bridge-ports {BRIDGE_PORTS}\n\
                     \x20 pre-up /etc/network/nfs_check\n\
                     \x20 wait-delay 15\n\
                     \x20 hostname $(hostname)\n"
                ));
            }
            AddressDocument::Static(s) => {
                out.push_str(&format!(
                    "iface {INTERFACE} inet static\n\
                     \x20 address {}\n\
                     \x20 netmask {}\n",
                    s.address,
                    s.netmask()
                ));
                if let Some(gw) = s.gateway {
                    out.push_str(&format!("  gateway {gw}\n"));
                }
                out.push_str(&format!(
                    "  bridge-ports {BRIDGE_PORTS}\n\
                     \x20 pre-up /etc/network/nfs_check\n\
                     \x20 wait-delay 15\n"
                ));
                if !s.dns.is_empty() || s.search.is_some() {
                    let names: Vec<String> = s.dns.iter().map(ToString::to_string).collect();
                    if !names.is_empty() {
                        out.push_str(&format!("  dns-nameservers {}\n", names.join(" ")));
                    }
                    if let Some(search) = &s.search {
                        out.push_str(&format!("  dns-search {search}\n"));
                    }
                    out.push_str(&format!(
                        "  up printf '{}' > /etc/resolv.conf\n",
                        resolv_conf(&s.dns, s.search.as_deref()).replace('\n', "\\n")
                    ));
                }
            }
        }
        out
    }
}

/// What `/etc/resolv.conf` should say for a static address. The `# br0`
/// suffix is udhcpc's convention on this image, kept so a later DHCP lease
/// replaces these lines and not somebody else's.
pub fn resolv_conf(dns: &[Ipv4Addr], search: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(search) = search {
        out.push_str(&format!("search {search} # {INTERFACE}\n"));
    }
    for ns in dns {
        out.push_str(&format!("nameserver {ns} # {INTERFACE}\n"));
    }
    out
}

/// What an `interfaces` file says, as far as this daemon can read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    pub document: Option<AddressDocument>,
    /// Written by this daemon, or by a person.
    pub ours: bool,
}

/// Read a document back out of an `interfaces` file.
///
/// Reads the `br0` stanza only, and only the keys this daemon writes plus the
/// classic `address/netmask` pair a person would use. A file it cannot make
/// sense of yields `document: None`, and the page says the file was written by
/// hand rather than guessing.
pub fn parse(contents: &str) -> ParsedFile {
    let ours = contents.lines().next().is_some_and(|l| l.trim() == MARKER);
    let mut in_br0 = false;
    let mut mode: Option<&str> = None;
    let mut address: Option<Ipv4Addr> = None;
    let mut prefix: Option<u8> = None;
    let mut gateway = None;
    let mut dns = Vec::new();
    let mut search = None;

    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let key = words.next().unwrap_or_default();
        if key == "iface" {
            let name = words.next().unwrap_or_default();
            in_br0 = name == INTERFACE;
            if in_br0 {
                // `iface br0 inet dhcp`
                mode = words.nth(1);
            }
            continue;
        }
        if key == "auto" || key == "source" || key == "mapping" {
            in_br0 = false;
            continue;
        }
        if !in_br0 {
            continue;
        }
        match key {
            "address" => {
                let value = words.next().unwrap_or_default();
                let (ip, p) = value.split_once('/').unwrap_or((value, ""));
                address = ip.parse().ok();
                if let Ok(p) = p.parse::<u8>() {
                    prefix = Some(p);
                }
            }
            "netmask" => {
                if let Ok(mask) = words.next().unwrap_or_default().parse::<Ipv4Addr>() {
                    prefix = Some(u32::from(mask).count_ones() as u8);
                }
            }
            "gateway" => gateway = words.next().and_then(|g| g.parse().ok()),
            "dns-nameservers" => dns = words.filter_map(|w| w.parse().ok()).collect(),
            "dns-search" => search = words.next().map(str::to_string),
            _ => {}
        }
    }

    let document = match mode {
        Some("dhcp") => Some(AddressDocument::Dhcp),
        Some("static") => match (address, prefix) {
            (Some(address), Some(prefix)) => Some(AddressDocument::Static(StaticAddress {
                address,
                prefix,
                gateway,
                dns,
                search,
            })),
            _ => None,
        },
        _ => None,
    };
    ParsedFile { document, ours }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lan() -> StaticAddress {
        StaticAddress {
            address: "192.168.1.20".parse().unwrap(),
            prefix: 24,
            gateway: Some("192.168.1.1".parse().unwrap()),
            dns: vec!["192.168.1.1".parse().unwrap(), "1.1.1.1".parse().unwrap()],
            search: Some("home.lan".into()),
        }
    }

    #[test]
    fn a_sound_static_address_is_accepted_with_no_warnings() {
        let doc = AddressDocument::Static(lan());
        assert_eq!(doc.refusal(), None);
        assert!(doc.warnings().is_empty());
    }

    #[test]
    fn dhcp_is_always_accepted() {
        assert_eq!(AddressDocument::Dhcp.refusal(), None);
        assert!(AddressDocument::Dhcp.warnings().is_empty());
    }

    #[test]
    fn a_gateway_off_the_subnet_is_refused() {
        let mut s = lan();
        s.gateway = Some("10.0.0.1".parse().unwrap());
        let reason = AddressDocument::Static(s).refusal().unwrap().reason;
        assert!(reason.contains("not on 192.168.1.0/24"), "{reason}");
    }

    #[test]
    fn the_network_and_broadcast_addresses_are_refused() {
        for ip in ["192.168.1.0", "192.168.1.255"] {
            let mut s = lan();
            s.address = ip.parse().unwrap();
            let reason = AddressDocument::Static(s).refusal().unwrap().reason;
            assert!(reason.contains("not a host on it"), "{ip}: {reason}");
        }
    }

    #[test]
    fn a_prefix_with_no_room_is_refused() {
        for prefix in [0, 7, 31, 32] {
            let mut s = lan();
            s.prefix = prefix;
            s.gateway = None;
            assert!(
                AddressDocument::Static(s).refusal().is_some(),
                "/{prefix} should be refused"
            );
        }
    }

    #[test]
    fn no_gateway_and_no_resolver_are_warnings_not_refusals() {
        let mut s = lan();
        s.gateway = None;
        s.dns.clear();
        let doc = AddressDocument::Static(s);
        assert_eq!(doc.refusal(), None);
        let reasons: Vec<String> = doc.warnings().into_iter().map(|w| w.reason).collect();
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons[0].contains("no gateway"));
        assert!(reasons[1].contains("pool.ntp.org"));
    }

    #[test]
    fn the_dhcp_stanza_is_the_images_own_plus_the_marker() {
        let text = AddressDocument::Dhcp.render();
        assert!(text.starts_with(MARKER));
        assert!(text.contains("iface br0 inet dhcp\n"));
        assert!(text.contains("  bridge-ports node1 node2 node3 node4 ge0 ge1\n"));
        assert!(text.contains("  pre-up /etc/network/nfs_check\n"));
        assert!(text.contains("  hostname $(hostname)\n"));
    }

    #[test]
    fn a_static_stanza_round_trips_through_parse() {
        let doc = AddressDocument::Static(lan());
        let text = doc.render();
        assert!(text.contains("  address 192.168.1.20\n"));
        assert!(text.contains("  netmask 255.255.255.0\n"));
        assert!(text.contains("  gateway 192.168.1.1\n"));
        assert!(text.contains("  dns-nameservers 192.168.1.1 1.1.1.1\n"));
        assert!(
            text.contains("  up printf 'search home.lan # br0\\nnameserver 192.168.1.1 # br0\\nnameserver 1.1.1.1 # br0\\n' > /etc/resolv.conf\n"),
            "{text}"
        );
        let parsed = parse(&text);
        assert!(parsed.ours);
        assert_eq!(parsed.document, Some(doc));
    }

    #[test]
    fn the_images_own_file_parses_as_dhcp_and_not_ours() {
        let image = "# interface file auto-generated by buildroot\n\nauto lo\niface lo inet loopback\n\nauto br0\niface br0 inet dhcp\n  bridge-ports node1 node2 node3 node4 ge0 ge1\n  pre-up /etc/network/nfs_check\n  wait-delay 15\n  hostname $(hostname)\n";
        let parsed = parse(image);
        assert!(!parsed.ours);
        assert_eq!(parsed.document, Some(AddressDocument::Dhcp));
    }

    #[test]
    fn a_hand_written_static_file_with_cidr_and_without_dns_parses() {
        let hand = "auto br0\niface br0 inet static\n  address 10.1.2.3/16\n  gateway 10.1.0.1\n  bridge-ports node1 node2 node3 node4 ge0 ge1\n";
        let parsed = parse(hand);
        assert!(!parsed.ours);
        let Some(AddressDocument::Static(s)) = parsed.document else {
            panic!("expected static: {parsed:?}");
        };
        assert_eq!(s.prefix, 16);
        assert_eq!(s.gateway, Some("10.1.0.1".parse().unwrap()));
        assert!(s.dns.is_empty());
    }

    #[test]
    fn a_file_this_daemon_cannot_read_yields_no_document() {
        let odd = "auto eth0\niface eth0 inet static\n  address 10.0.0.2\n";
        assert_eq!(parse(odd).document, None);
        let half = "iface br0 inet static\n  gateway 10.0.0.1\n";
        assert_eq!(parse(half).document, None);
    }

    #[test]
    fn the_json_shape_is_tagged_by_mode() {
        let json = serde_json::to_value(AddressDocument::Static(lan())).unwrap();
        assert_eq!(json["mode"], "static");
        assert_eq!(json["address"], "192.168.1.20");
        assert_eq!(json["prefix"], 24);
        let dhcp: AddressDocument = serde_json::from_str(r#"{"mode":"dhcp"}"#).unwrap();
        assert_eq!(dhcp, AddressDocument::Dhcp);
    }
}
