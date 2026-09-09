// Copyright 2026 excavador-turing
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

//! Which USB port a compute module appears on.
//!
//! A v2.5 board puts all four modules behind one GL850 hub, so during a flash
//! every module in maskrom enumerates at once and "the first supported device"
//! is whichever one the bus happened to answer for. That is how a flash of
//! node 2 wrote node 1 and reported success.
//!
//! The mapping from a node to its hub port is **read from the device tree**,
//! not assumed. The v2.5 DTS spells it out:
//!
//! ```text
//! hub@1 {
//!     compatible = "usb5e3,608";
//!     reg = <1>;
//!     node1@1 { reg = <1>; };
//!     ...
//!     node4@4 { reg = <4>; };
//! };
//! ```
//!
//! and the kernel exposes all of it under `/proc/device-tree`. Assuming node N
//! is port N would be right on this board and would be an assumption; getting
//! it wrong would flash the wrong module, which is the exact harm this module
//! exists to prevent, so it is not a place to save four sysfs reads.
//!
//! A board whose device tree describes no such hub -- v2.4, whose single USB
//! mux really does show one node at a time -- yields `None`, and the caller
//! keeps its old first-match behaviour. The device tree is therefore also the
//! board-revision test, which is better than a revision string because it is
//! the same fact the kernel is acting on.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Where the kernel publishes the device tree it booted with.
const DEVICE_TREE: &str = "/proc/device-tree";

/// A USB device's position on the bus, as `rusb` and sysfs both describe it.
///
/// `rusb` gives the bus number and the chain of port numbers from the root;
/// sysfs names the same device `<bus>-<port>.<port>`. Keeping one type for
/// both is what lets a device found over `rusb` be matched against a block
/// device found under `/sys/block`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbPortPath {
    pub bus: u8,
    pub ports: Vec<u8>,
}

impl UsbPortPath {
    /// The directory name sysfs gives this device, e.g. `1-1.3`.
    pub fn sysfs_name(&self) -> String {
        let ports: Vec<String> = self.ports.iter().map(u8::to_string).collect();
        format!("{}-{}", self.bus, ports.join("."))
    }

    /// Whether a canonicalised sysfs path passes through this device.
    ///
    /// A block device behind a hub port lives at
    /// `…/usb1/1-1/1-1.3/1-1.3:1.0/host0/…/block/sda`, so the port's own
    /// directory is a path component. Matched as a whole component with
    /// separators on both sides, because `1-1.3` is a prefix of `1-1.3.2`
    /// and a substring test would accept a device one hub further down.
    pub fn contains(&self, path: &Path) -> bool {
        let needle = format!("/{}/", self.sysfs_name());
        path.to_string_lossy().contains(&needle)
    }
}

impl std::fmt::Display for UsbPortPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.sysfs_name())
    }
}

/// The hub that fans a board's USB out to its compute modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubTopology {
    /// The hub's own port on the controller above it.
    hub_port: u8,
    /// Node number, 1-based, to the hub port it is wired to.
    node_ports: BTreeMap<u8, u8>,
}

impl HubTopology {
    /// Read the board's own description of its hub, or `None` when it
    /// describes no hub with node ports.
    pub fn read() -> Option<Self> {
        Self::read_from(Path::new(DEVICE_TREE))
    }

    /// The port path a device on `node` (1-based) must appear at.
    ///
    /// `None` for a node this hub does not describe, which is not the same as
    /// a node that is absent: it means the board says nothing about where that
    /// module would show up, and guessing is the thing being removed.
    pub fn port_path(&self, bus: u8, node: u8) -> Option<UsbPortPath> {
        self.node_ports.get(&node).map(|port| UsbPortPath {
            bus,
            ports: vec![self.hub_port, *port],
        })
    }

    /// Every node this hub describes, in order. For error messages that say
    /// what the board does have when a request names something it does not.
    pub fn nodes(&self) -> Vec<u8> {
        self.node_ports.keys().copied().collect()
    }

    fn read_from(root: &Path) -> Option<Self> {
        let hub = find_hub(root)?;
        let hub_port = read_reg(&hub.join("reg"))?;

        let mut node_ports = BTreeMap::new();
        for entry in std::fs::read_dir(&hub).ok()? {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            // `node2@2` -- the label carries the node number and the unit
            // address carries the port. They agree on this board; `reg` is
            // what the kernel binds on, so `reg` is what is trusted.
            let Some(node) = name
                .strip_prefix("node")
                .and_then(|rest| rest.split('@').next())
                .and_then(|digits| digits.parse::<u8>().ok())
            else {
                continue;
            };
            if let Some(port) = read_reg(&entry.path().join("reg")) {
                node_ports.insert(node, port);
            }
        }

        if node_ports.is_empty() {
            return None;
        }

        Some(HubTopology {
            hub_port,
            node_ports,
        })
    }
}

/// The first `hub@…` under any `usb@…` controller, searched rather than
/// hard-coded: the controller's unit address is an SoC memory address, and
/// pinning `usb@4200000` here would tie this to one silicon.
fn find_hub(root: &Path) -> Option<PathBuf> {
    for soc in [root.join("soc"), root.to_path_buf()] {
        let Ok(entries) = std::fs::read_dir(&soc) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("usb@") {
                continue;
            }
            let Ok(children) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for child in children.flatten() {
                if child.file_name().to_string_lossy().starts_with("hub@") {
                    return Some(child.path());
                }
            }
        }
    }
    None
}

/// A device-tree `reg` cell: four bytes, big-endian, however small the number.
fn read_reg(path: &Path) -> Option<u8> {
    let bytes = std::fs::read(path).ok()?;
    let cell: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    u32::from_be_bytes(cell).try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the shape `/proc/device-tree` has on a v2.5 board.
    fn v25_tree(dir: &Path) {
        let hub = dir.join("soc/usb@4200000/hub@1");
        std::fs::create_dir_all(&hub).unwrap();
        std::fs::write(hub.join("compatible"), b"usb5e3,608\0").unwrap();
        std::fs::write(hub.join("reg"), 1u32.to_be_bytes()).unwrap();
        for node in 1..=4u8 {
            let port = dir.join(format!("soc/usb@4200000/hub@1/node{node}@{node}"));
            std::fs::create_dir_all(&port).unwrap();
            std::fs::write(port.join("reg"), u32::from(node).to_be_bytes()).unwrap();
        }
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bmcd-topology-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_v25_tree_maps_every_node_to_its_own_port() {
        let dir = tempdir("v25");
        v25_tree(&dir);

        let topology = HubTopology::read_from(&dir).expect("the tree describes a hub");

        assert_eq!(topology.nodes(), vec![1, 2, 3, 4]);
        for node in 1..=4u8 {
            assert_eq!(
                topology.port_path(1, node).map(|p| p.sysfs_name()),
                Some(format!("1-1.{node}")),
                "node {node}"
            );
        }
    }

    /// The mapping is read, not assumed. A board that wired its modules to the
    /// hub in the other order must produce the other answer -- if this test
    /// passes with the reversed tree still reporting `1-1.1` for node 1, the
    /// port numbers are being invented somewhere.
    #[test]
    fn a_reversed_board_is_read_reversed() {
        let dir = tempdir("reversed");
        let hub = dir.join("soc/usb@4200000/hub@1");
        std::fs::create_dir_all(&hub).unwrap();
        std::fs::write(hub.join("reg"), 1u32.to_be_bytes()).unwrap();
        for node in 1..=4u8 {
            let port = hub.join(format!("node{node}@{node}"));
            std::fs::create_dir_all(&port).unwrap();
            std::fs::write(port.join("reg"), u32::from(5 - node).to_be_bytes()).unwrap();
        }

        let topology = HubTopology::read_from(&dir).unwrap();

        assert_eq!(
            topology.port_path(1, 1).map(|p| p.sysfs_name()),
            Some("1-1.4".to_string())
        );
        assert_eq!(
            topology.port_path(1, 4).map(|p| p.sysfs_name()),
            Some("1-1.1".to_string())
        );
    }

    /// v2.4: a single mux, no hub in the tree. The caller must fall back to
    /// its old behaviour rather than refuse every flash on that board.
    #[test]
    fn a_board_with_no_hub_has_no_topology() {
        let dir = tempdir("v24");
        std::fs::create_dir_all(dir.join("soc/usb@4200000")).unwrap();

        assert_eq!(HubTopology::read_from(&dir), None);
    }

    /// A hub with no node children is not a node-fanout hub, and treating it
    /// as one would map every flash onto a port that describes nothing.
    #[test]
    fn a_hub_without_node_ports_has_no_topology() {
        let dir = tempdir("bare-hub");
        let hub = dir.join("soc/usb@4200000/hub@1");
        std::fs::create_dir_all(&hub).unwrap();
        std::fs::write(hub.join("reg"), 1u32.to_be_bytes()).unwrap();

        assert_eq!(HubTopology::read_from(&dir), None);
    }

    /// `1-1.3` is a prefix of `1-1.3.2`, so a substring test would accept a
    /// device one hub further down as if it were the module itself.
    #[test]
    fn a_deeper_hub_is_not_mistaken_for_the_port() {
        let path = UsbPortPath {
            bus: 1,
            ports: vec![1, 3],
        };

        assert!(path.contains(Path::new("/sys/devices/usb1/1-1/1-1.3/1-1.3:1.0/block/sda")));
        assert!(!path.contains(Path::new(
            "/sys/devices/usb1/1-1/1-1.3.2/1-1.3.2:1.0/block/sda"
        )));
    }
}
