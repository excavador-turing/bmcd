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

//! Putting a document on the switch, and reading one back off it.
//!
//! The kernel's bridge is the interface to the switch: the DSA driver turns
//! `bridge vlan add` into register writes on the RTL8370MB-CG. So this module
//! builds `bridge` and `ip` command lines, and nothing here knows anything
//! about the hardware beyond the port names.
//!
//! ## Why the commands are built first and run second
//!
//! [`plan`] turns a document into the exact list of commands, and is a pure
//! function with tests. [`Applier::apply`] runs that list. Keeping them apart
//! means the thing most likely to be wrong -- the order and the flags -- can
//! be checked without a switch, and the thing that cannot be tested without
//! hardware is reduced to "run these, stop on the first failure".
//!
//! ## Order matters, and the order is not obvious
//!
//! Filtering goes on BEFORE the VLANs are written and off AFTER they are
//! cleared, because a bridge with filtering on and no VLANs configured
//! forwards nothing at all. Doing it the other way round takes the board off
//! the network in the middle of its own reconfiguration -- which the confirm
//! window would recover, but only after making an operator watch it happen.

use crate::app::switch_document::{PortId, SwitchDocument};

/// The kernel's name for each port, as the device tree spells it.
///
/// The BMC's own port is deliberately absent: it is the CPU port, it has no
/// netdev, and its VLAN membership is a property of the bridge itself rather
/// than of an interface that can be named on a command line.
pub fn interface_name(port: PortId) -> Option<&'static str> {
    match port {
        PortId::Node1 => Some("node1"),
        PortId::Node2 => Some("node2"),
        PortId::Node3 => Some("node3"),
        PortId::Node4 => Some("node4"),
        PortId::Ge0 => Some("ge0"),
        PortId::Ge1 => Some("ge1"),
        PortId::Bmc => None,
    }
}

/// The bridge every switch port is enslaved to.
pub const BRIDGE: &str = "br0";

/// One command, as argv. A string would have to be parsed by a shell, and a
/// shell is not something this needs between it and the kernel.
pub type Command = Vec<String>;

fn cmd(parts: &[&str]) -> Command {
    parts.iter().map(|part| part.to_string()).collect()
}

/// Every command needed to move the switch from `running` to `wanted`.
///
/// Computed as a whole rather than emitted as the document is walked, because
/// the order is the part that matters and an order is not something you can
/// get right one line at a time.
pub fn plan(running: &SwitchDocument, wanted: &SwitchDocument) -> Vec<Command> {
    let mut commands = Vec::new();

    let turning_on = wanted.vlan_filtering && !running.vlan_filtering;
    let turning_off = !wanted.vlan_filtering && running.vlan_filtering;

    // ON FIRST. A bridge with filtering on and no VLANs yet forwards nothing,
    // so the VLANs have to exist before it starts caring about them... and
    // yet the kernel will not accept a VLAN on a bridge that is not filtering.
    // The resolution is that `bridge vlan add` works either way, and the
    // window between the two is the whole point of enabling filtering last.
    if turning_off {
        commands.push(cmd(&[
            "ip",
            "link",
            "set",
            "dev",
            BRIDGE,
            "type",
            "bridge",
            "vlan_filtering",
            "0",
        ]));
    }

    // Remove what is no longer wanted, before adding, so a port moving from
    // one untagged VLAN to another is never briefly in both.
    for port in PortId::ALL {
        let Some(name) = interface_name(port) else {
            continue;
        };
        let was = running.ports.get(&port).cloned().unwrap_or_default();
        let now = wanted.ports.get(&port).cloned().unwrap_or_default();
        for vid in was.vlans().difference(&now.vlans()) {
            commands.push(cmd(&[
                "bridge",
                "vlan",
                "del",
                "dev",
                name,
                "vid",
                &vid.to_string(),
            ]));
        }
    }

    for port in PortId::ALL {
        let Some(name) = interface_name(port) else {
            continue;
        };
        let was = running.ports.get(&port).cloned().unwrap_or_default();
        let now = wanted.ports.get(&port).cloned().unwrap_or_default();

        for vid in &now.tagged {
            if was.tagged.contains(vid) {
                continue;
            }
            commands.push(cmd(&[
                "bridge",
                "vlan",
                "add",
                "dev",
                name,
                "vid",
                &vid.to_string(),
            ]));
        }
        if let Some(vid) = now.untagged {
            if was.untagged != Some(vid) {
                commands.push(cmd(&[
                    "bridge",
                    "vlan",
                    "add",
                    "dev",
                    name,
                    "vid",
                    &vid.to_string(),
                    "pvid",
                    "untagged",
                ]));
            }
        }
    }

    if turning_on {
        commands.push(cmd(&[
            "ip",
            "link",
            "set",
            "dev",
            BRIDGE,
            "type",
            "bridge",
            "vlan_filtering",
            "1",
        ]));
    }

    if wanted.stp != running.stp {
        commands.push(cmd(&[
            "ip",
            "link",
            "set",
            "dev",
            BRIDGE,
            "type",
            "bridge",
            "stp_state",
            if wanted.stp { "1" } else { "0" },
        ]));
    }

    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::switch_document::{Preset, SecondUplink};

    fn trunk() -> SwitchDocument {
        Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Redundant,
        }
        .expand()
    }

    fn joined(commands: &[Command]) -> Vec<String> {
        commands.iter().map(|c| c.join(" ")).collect()
    }

    #[test]
    fn applying_a_document_to_itself_does_nothing() {
        assert!(plan(&trunk(), &trunk()).is_empty());
        assert!(plan(&Preset::Flat.expand(), &Preset::Flat.expand()).is_empty());
    }

    /// The BMC's own port has no netdev, so it can never appear on a command
    /// line. Emitting one would fail at the first apply.
    #[test]
    fn no_command_ever_names_the_bmc_port() {
        let lines = joined(&plan(&Preset::Flat.expand(), &trunk()));
        assert!(!lines.iter().any(|l| l.contains("dev bmc")), "{lines:#?}");
        assert_eq!(interface_name(PortId::Bmc), None);
    }

    /// A name is not a switch setting. Renaming a VLAN must not touch the
    /// hardware, or every relabelling would cost the board a reconfiguration
    /// of something that was already right.
    #[test]
    fn renaming_a_vlan_plans_nothing() {
        let running = trunk();
        let mut renamed = running.clone();
        renamed
            .names
            .insert(20, "the modules, out the back".to_string());
        assert_ne!(running, renamed, "the documents do differ");
        assert!(plan(&running, &renamed).is_empty());
    }

    /// Filtering must not be switched on before the VLANs exist, and must be
    /// switched off before they are removed. A bridge filtering with no VLANs
    /// forwards nothing.
    #[test]
    fn filtering_goes_on_last_and_comes_off_first() {
        let on = joined(&plan(&Preset::Flat.expand(), &trunk()));
        let index = on
            .iter()
            .position(|l| l.contains("vlan_filtering 1"))
            .expect("filtering is turned on");
        assert_eq!(
            index,
            on.len() - 2,
            "filtering should be enabled after the VLANs: {on:#?}"
        );

        let off = joined(&plan(&trunk(), &Preset::Flat.expand()));
        let index = off
            .iter()
            .position(|l| l.contains("vlan_filtering 0"))
            .expect("filtering is turned off");
        assert_eq!(
            index, 0,
            "filtering should be disabled before anything else: {off:#?}"
        );
    }

    /// A port moving between untagged VLANs must leave the old one before it
    /// joins the new one, or it is briefly in both.
    #[test]
    fn removals_come_before_additions() {
        let lines = joined(&plan(&Preset::Split.expand(), &trunk()));
        let first_add = lines.iter().position(|l| l.contains("vlan add")).unwrap();
        let last_del = lines.iter().rposition(|l| l.contains("vlan del")).unwrap();
        assert!(last_del < first_add, "{lines:#?}");
    }

    #[test]
    fn an_untagged_vlan_is_added_as_pvid_untagged() {
        let lines = joined(&plan(&Preset::Flat.expand(), &trunk()));
        assert!(
            lines
                .iter()
                .any(|l| l == "bridge vlan add dev node1 vid 20 pvid untagged"),
            "{lines:#?}"
        );
    }

    #[test]
    fn a_tagged_vlan_is_added_without_pvid() {
        let lines = joined(&plan(&Preset::Flat.expand(), &trunk()));
        assert!(
            lines.iter().any(|l| l == "bridge vlan add dev ge0 vid 10"),
            "{lines:#?}"
        );
        assert!(!lines
            .iter()
            .any(|l| l == "bridge vlan add dev ge0 vid 10 pvid untagged"));
    }

    #[test]
    fn spanning_tree_is_only_touched_when_it_changes() {
        let with_stp = joined(&plan(&Preset::Flat.expand(), &trunk()));
        assert!(with_stp.iter().any(|l| l.contains("stp_state 1")));

        let no_change = joined(&plan(&trunk(), &trunk()));
        assert!(!no_change.iter().any(|l| l.contains("stp_state")));
    }

    /// Taking the second uplink out of service should touch ge1 and nothing
    /// else, which is the cheapest possible proof that the plan is a
    /// difference and not a rewrite.
    #[test]
    fn a_narrow_change_produces_a_narrow_plan() {
        let redundant = trunk();
        let off = Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Off,
        }
        .expand();
        let lines = joined(&plan(&redundant, &off));
        assert!(
            lines
                .iter()
                .all(|l| l.contains("ge1") || l.contains("stp_state")),
            "{lines:#?}"
        );
    }
}
