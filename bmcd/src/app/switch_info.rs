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
//! Link state of the on-board Ethernet switch, read from `/sys/class/net`.
//!
//! The switch driver registers one netdev per port, so everything this module
//! reports is what the kernel already knows; nothing here talks to the switch
//! itself.
use serde::Serialize;
use std::path::Path;
use std::str::FromStr;

/// Where the kernel exposes network interfaces.
const NET_CLASS: &str = "/sys/class/net";

/// The ports of the on-board switch, in slot order, uplinks last. These names
/// come from the device tree, so they are the same on every board that boots
/// our firmware.
const SWITCH_PORTS: [(&str, PortKind); 6] = [
    ("node1", PortKind::Node),
    ("node2", PortKind::Node),
    ("node3", PortKind::Node),
    ("node4", PortKind::Node),
    ("ge0", PortKind::Uplink),
    ("ge1", PortKind::Uplink),
];

/// What sits on the other side of a switch port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PortKind {
    /// Faces a compute module slot.
    Node,
    /// Leaves the board.
    Uplink,
}

/// One port of the switch. Every field the kernel would not give us is
/// `None` rather than a stand-in value: a port that is down has no speed and
/// no duplex, and saying `0` there would read as a working port running at
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwitchPort {
    /// Interface name, as the kernel spells it.
    pub name: String,
    pub kind: PortKind,
    /// Whether the kernel has a netdev for this port at all. False means the
    /// switch driver did not register it -- on a board where the driver fails
    /// to probe, all six read false while the BMC itself stays reachable over
    /// its own interface.
    pub present: bool,
    /// `carrier`: a peer is electrically there and has trained.
    pub link: Option<bool>,
    /// `operstate`: `up`, `down`, `lowerlayerdown`, `unknown`. Passed through
    /// verbatim, because the distinction between `down` (nobody asked for the
    /// port) and `lowerlayerdown` (asked for, nothing on the wire) is the
    /// whole point of reading it.
    pub operstate: Option<String>,
    /// Megabit per second. The kernel reports `-1` for a port with no link,
    /// which becomes `None` here.
    pub speed_mbps: Option<u32>,
    /// `full` or `half`. The kernel reports `unknown` for a port with no
    /// link, which becomes `None` here.
    pub duplex: Option<String>,
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
    pub rx_errors: Option<u64>,
    pub tx_errors: Option<u64>,
}

impl SwitchPort {
    /// A port the kernel does not have.
    fn absent(name: &str, kind: PortKind) -> Self {
        SwitchPort {
            name: name.to_string(),
            kind,
            present: false,
            link: None,
            operstate: None,
            speed_mbps: None,
            duplex: None,
            rx_bytes: None,
            tx_bytes: None,
            rx_errors: None,
            tx_errors: None,
        }
    }
}

/// Reads the state of every switch port. The list always has one entry per
/// port in [`SWITCH_PORTS`], whether or not the kernel knows about it, so a
/// caller can tell "the switch is gone" from "the switch is fine" without
/// knowing the port names itself.
pub async fn get_switch_ports() -> Vec<SwitchPort> {
    read_switch_ports(Path::new(NET_CLASS)).await
}

async fn read_switch_ports(net_class: &Path) -> Vec<SwitchPort> {
    let mut ports = Vec::with_capacity(SWITCH_PORTS.len());
    for (name, kind) in SWITCH_PORTS {
        ports.push(read_port(net_class, name, kind).await);
    }
    ports
}

async fn read_port(net_class: &Path, name: &str, kind: PortKind) -> SwitchPort {
    let dir = net_class.join(name);
    if tokio::fs::metadata(&dir).await.is_err() {
        return SwitchPort::absent(name, kind);
    }

    let statistics = dir.join("statistics");

    SwitchPort {
        name: name.to_string(),
        kind,
        present: true,
        // `carrier` is one of the attributes that answers EINVAL rather than
        // a value while the interface is administratively down, hence the
        // Option.
        link: read_attribute::<u8>(&dir, "carrier").await.map(|c| c != 0),
        operstate: read_attribute_string(&dir, "operstate").await,
        speed_mbps: read_attribute::<i64>(&dir, "speed")
            .await
            .filter(|speed| *speed > 0)
            .map(|speed| speed as u32),
        duplex: read_attribute_string(&dir, "duplex")
            .await
            .filter(|duplex| duplex != "unknown"),
        rx_bytes: read_attribute(&statistics, "rx_bytes").await,
        tx_bytes: read_attribute(&statistics, "tx_bytes").await,
        rx_errors: read_attribute(&statistics, "rx_errors").await,
        tx_errors: read_attribute(&statistics, "tx_errors").await,
    }
}

/// Reads one sysfs attribute as trimmed text. A missing file, an unreadable
/// one, and an empty one are all `None`: sysfs answers a read with an error
/// for attributes the driver cannot supply right now, and that is a normal
/// state for a port, not a fault of ours.
async fn read_attribute_string(dir: &Path, attribute: &str) -> Option<String> {
    let value = tokio::fs::read_to_string(dir.join(attribute)).await.ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

async fn read_attribute<T: FromStr>(dir: &Path, attribute: &str) -> Option<T> {
    read_attribute_string(dir, attribute).await?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Builds a `/sys/class/net` lookalike. Every value here was copied off a
    /// running board.
    fn fake_net_class() -> (tempdir::TempDir, PathBuf) {
        let dir = tempdir::TempDir::new("net_class").expect("tempdir");
        let root = dir.path().to_path_buf();

        for node in ["node1", "node2", "node3", "node4", "ge0"] {
            write_port(&root, node, "up", "1", "1000", "full");
        }
        // the uplink with nothing plugged into it
        write_port(&root, "ge1", "lowerlayerdown", "0", "-1", "unknown");

        (dir, root)
    }

    fn write_port(root: &Path, name: &str, operstate: &str, carrier: &str, speed: &str, dup: &str) {
        let port = root.join(name);
        let statistics = port.join("statistics");
        fs::create_dir_all(&statistics).expect("create port dir");
        fs::write(port.join("operstate"), format!("{}\n", operstate)).expect("operstate");
        fs::write(port.join("carrier"), format!("{}\n", carrier)).expect("carrier");
        fs::write(port.join("speed"), format!("{}\n", speed)).expect("speed");
        fs::write(port.join("duplex"), format!("{}\n", dup)).expect("duplex");
        fs::write(statistics.join("rx_bytes"), "1234\n").expect("rx_bytes");
        fs::write(statistics.join("tx_bytes"), "5678\n").expect("tx_bytes");
        fs::write(statistics.join("rx_errors"), "0\n").expect("rx_errors");
        fs::write(statistics.join("tx_errors"), "0\n").expect("tx_errors");
    }

    #[tokio::test]
    async fn reports_every_port_of_the_switch() {
        let (_guard, root) = fake_net_class();
        let ports = read_switch_ports(&root).await;

        assert_eq!(
            ports.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["node1", "node2", "node3", "node4", "ge0", "ge1"]
        );
        assert_eq!(
            ports.iter().map(|p| p.kind).collect::<Vec<_>>(),
            vec![
                PortKind::Node,
                PortKind::Node,
                PortKind::Node,
                PortKind::Node,
                PortKind::Uplink,
                PortKind::Uplink
            ]
        );
    }

    #[tokio::test]
    async fn a_linked_port_reports_its_speed_and_counters() {
        let (_guard, root) = fake_net_class();
        let ports = read_switch_ports(&root).await;
        let node1 = &ports[0];

        assert!(node1.present);
        assert_eq!(node1.link, Some(true));
        assert_eq!(node1.operstate.as_deref(), Some("up"));
        assert_eq!(node1.speed_mbps, Some(1000));
        assert_eq!(node1.duplex.as_deref(), Some("full"));
        assert_eq!(node1.rx_bytes, Some(1234));
        assert_eq!(node1.tx_bytes, Some(5678));
        assert_eq!(node1.rx_errors, Some(0));
        assert_eq!(node1.tx_errors, Some(0));
    }

    /// The state the board is actually in: the second uplink has no cable.
    #[tokio::test]
    async fn a_port_without_a_link_has_no_speed_or_duplex() {
        let (_guard, root) = fake_net_class();
        let ge1 = read_switch_ports(&root)
            .await
            .into_iter()
            .find(|p| p.name == "ge1")
            .expect("ge1");

        assert!(ge1.present);
        assert_eq!(ge1.link, Some(false));
        assert_eq!(ge1.operstate.as_deref(), Some("lowerlayerdown"));
        assert_eq!(ge1.speed_mbps, None, "-1 is not a speed");
        assert_eq!(ge1.duplex, None, "'unknown' is not a duplex");
        // counters are still real
        assert_eq!(ge1.rx_bytes, Some(1234));
    }

    /// The failure this endpoint exists for: the switch driver did not probe,
    /// so the kernel has no netdev for any port. The BMC is still perfectly
    /// reachable, and every compute module is cut off.
    #[tokio::test]
    async fn a_switch_that_did_not_probe_is_reported_as_absent() {
        let empty = tempdir::TempDir::new("net_class").expect("tempdir");
        let ports = read_switch_ports(empty.path()).await;

        assert_eq!(ports.len(), 6, "every port is still listed");
        assert!(ports.iter().all(|p| !p.present));
        assert!(ports.iter().all(|p| p.link.is_none()));
        assert!(ports.iter().all(|p| p.speed_mbps.is_none()));
    }

    /// A driver that registers the interface but answers reads with EINVAL,
    /// or an older kernel without one of these attributes, must not take the
    /// whole port down with it.
    #[tokio::test]
    async fn missing_attributes_do_not_fail_the_port() {
        let dir = tempdir::TempDir::new("net_class").expect("tempdir");
        fs::create_dir_all(dir.path().join("node1")).expect("create port dir");

        let node1 = read_switch_ports(dir.path())
            .await
            .into_iter()
            .find(|p| p.name == "node1")
            .expect("node1");

        assert!(node1.present, "the netdev exists");
        assert_eq!(node1.link, None);
        assert_eq!(node1.operstate, None);
        assert_eq!(node1.rx_bytes, None);
    }
}
