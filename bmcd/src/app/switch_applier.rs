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

/// The kernel's name for each front-panel / node port, as the device tree
/// spells it.
///
/// The BMC's own port has no netdev, so it is absent here. Its VLAN membership
/// is programmed on the bridge with the `self` flag — see [`vlan_target`].
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

/// Where a port's VLAN membership is written: netdev name, and whether the
/// command needs the bridge `self` flag.
///
/// Front-panel ports are their own devices. The BMC is the CPU port: there is
/// no `bmc` netdev, so membership is `bridge vlan … dev br0 … self`.
fn vlan_target(port: PortId) -> (&'static str, bool) {
    match interface_name(port) {
        Some(name) => (name, false),
        None => (BRIDGE, true),
    }
}

/// One command, as argv. A string would have to be parsed by a shell, and a
/// shell is not something this needs between it and the kernel.
pub type Command = Vec<String>;

fn cmd(parts: &[&str]) -> Command {
    parts.iter().map(|part| part.to_string()).collect()
}

fn vlan_del(dev: &str, vid: u16, bridge_self: bool) -> Command {
    // `self` last: that is how `bridge` marks the bridge's own VLAN entry.
    let mut out = vec![
        "bridge".into(),
        "vlan".into(),
        "del".into(),
        "dev".into(),
        dev.into(),
        "vid".into(),
        vid.to_string(),
    ];
    if bridge_self {
        out.push("self".into());
    }
    out
}

fn vlan_add(dev: &str, vid: u16, pvid_untagged: bool, bridge_self: bool) -> Command {
    let mut out = vec![
        "bridge".into(),
        "vlan".into(),
        "add".into(),
        "dev".into(),
        dev.into(),
        "vid".into(),
        vid.to_string(),
    ];
    if pvid_untagged {
        out.push("pvid".into());
        out.push("untagged".into());
    }
    if bridge_self {
        out.push("self".into());
    }
    out
}

/// The `bridge vlan del` lines that take `port` out of the VLANs it is leaving.
fn removals(port: PortId, running: &SwitchDocument, wanted: &SwitchDocument) -> Vec<Command> {
    let (dev, bridge_self) = vlan_target(port);
    let was = running.ports.get(&port).cloned().unwrap_or_default();
    let now = wanted.ports.get(&port).cloned().unwrap_or_default();
    was.vlans()
        .difference(&now.vlans())
        .map(|vid| vlan_del(dev, *vid, bridge_self))
        .collect()
}

/// The `bridge vlan add` lines that put `port` into the VLANs it is joining.
fn additions(port: PortId, running: &SwitchDocument, wanted: &SwitchDocument) -> Vec<Command> {
    let (dev, bridge_self) = vlan_target(port);
    let was = running.ports.get(&port).cloned().unwrap_or_default();
    let now = wanted.ports.get(&port).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for vid in &now.tagged {
        if !was.tagged.contains(vid) {
            out.push(vlan_add(dev, *vid, false, bridge_self));
        }
    }
    if let Some(vid) = now.untagged {
        if was.untagged != Some(vid) {
            out.push(vlan_add(dev, vid, true, bridge_self));
        }
    }
    out
}

/// True for a command whose failure does not stop the plan: deleting the
/// bridge's own (`self`) VLAN entry.
///
/// What a delete wants is "the entry is gone", and a board whose bridge never
/// had it (a hand-written `interfaces` file, say) already is. The BMC's new
/// VLAN was added before this runs, so a delete that fails for any other
/// reason leaves the BMC in one VLAN too many, which is harmless, whereas
/// stopping here would leave the front ports half-moved with no rollback.
pub fn is_best_effort(command: &Command) -> bool {
    matches!(
        command
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["bridge", "vlan", "del", "dev", BRIDGE, "vid", _, "self"]
    )
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

    // The BMC first, and its own order inside: join the new VLAN, THEN leave
    // the old one. The BMC is what the board is managed through; being in two
    // VLANs on the bridge itself is harmless for a moment, being in none is
    // not, and a plan that stops part-way is not rolled back. A failed delete
    // here leaves the BMC in one VLAN too many (see [`is_best_effort`]).
    for port in PortId::ALL
        .into_iter()
        .filter(|p| interface_name(*p).is_none())
    {
        commands.extend(additions(port, running, wanted));
        commands.extend(removals(port, running, wanted));
    }

    // Then the front ports: remove what is no longer wanted, before adding, so
    // a port moving from one untagged VLAN to another is never briefly in both.
    for port in PortId::ALL
        .into_iter()
        .filter(|p| interface_name(*p).is_some())
    {
        commands.extend(removals(port, running, wanted));
    }
    for port in PortId::ALL
        .into_iter()
        .filter(|p| interface_name(*p).is_some())
    {
        commands.extend(additions(port, running, wanted));
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

    /// The BMC has no `bmc` netdev, so no command can name one; its VLANs are
    /// `dev br0 ... self` lines, covered by the tests below.
    #[test]
    fn no_command_ever_names_the_bmc_port() {
        let lines = joined(&plan(&Preset::Flat.expand(), &trunk()));
        assert!(!lines.iter().any(|l| l.contains("dev bmc")), "{lines:#?}");
        assert_eq!(interface_name(PortId::Bmc), None);
    }

    /// Only the `br0 ... self` lines of a plan: the BMC's own.
    fn self_lines(plan: &[Command]) -> Vec<String> {
        joined(plan)
            .into_iter()
            .filter(|l| l.contains("dev br0") && l.ends_with(" self"))
            .collect()
    }

    /// The BMC joins its new VLAN before it leaves the old one, so it is never
    /// in none: a plan that stops part-way is not rolled back.
    #[test]
    fn the_bmc_vlan_is_programmed_on_br0_self_add_first() {
        let to_trunk = plan(&Preset::Flat.expand(), &trunk());
        assert_eq!(
            self_lines(&to_trunk),
            [
                "bridge vlan add dev br0 vid 10 pvid untagged self",
                "bridge vlan del dev br0 vid 1 self",
            ]
        );
        assert_eq!(
            self_lines(&plan(&Preset::Flat.expand(), &Preset::Split.expand())),
            [
                "bridge vlan add dev br0 vid 4093 pvid untagged self",
                "bridge vlan del dev br0 vid 1 self",
            ]
        );
    }

    /// The path that runs when a change is not confirmed: back to Flat, the BMC
    /// on VID 1 again, added before the management VLAN is dropped.
    #[test]
    fn reverting_trunk_to_flat_puts_the_bmc_back_on_vid_1_first() {
        let back = plan(&trunk(), &Preset::Flat.expand());
        assert_eq!(
            self_lines(&back),
            [
                "bridge vlan add dev br0 vid 1 pvid untagged self",
                "bridge vlan del dev br0 vid 10 self",
            ]
        );
        // Filtering still comes off before anything else.
        assert!(joined(&back)[0].contains("vlan_filtering 0"), "{back:#?}");
    }

    #[test]
    fn changing_the_management_vid_moves_the_bmc_add_first() {
        let mut ten = trunk();
        let mut twenty = trunk();
        for (doc, vid) in [(&mut ten, 10), (&mut twenty, 20)] {
            doc.ports.get_mut(&PortId::Bmc).unwrap().untagged = Some(vid);
        }
        assert_eq!(
            self_lines(&plan(&ten, &twenty)),
            [
                "bridge vlan add dev br0 vid 20 pvid untagged self",
                "bridge vlan del dev br0 vid 10 self",
            ]
        );
    }

    #[test]
    fn an_unchanged_management_vid_emits_no_self_lines() {
        // Trunk to a Trunk that differs only in the node VLAN.
        let mut other = trunk();
        other.ports.get_mut(&PortId::Node1).unwrap().untagged = Some(30);
        let lines = plan(&trunk(), &other);
        assert!(!lines.is_empty(), "the node port did change");
        assert!(self_lines(&lines).is_empty(), "{lines:#?}");
    }

    /// Whatever else is in the plan, the BMC's lines come after filtering is
    /// switched off, before any front port is touched, and before it is
    /// switched on.
    #[test]
    fn the_bmc_lines_sit_between_filtering_off_and_the_front_ports() {
        let lines = joined(&plan(&trunk(), &Preset::Flat.expand()));
        let first_self = lines.iter().position(|l| l.ends_with(" self")).unwrap();
        let first_front = lines.iter().position(|l| l.contains("dev node")).unwrap();
        assert!(first_self < first_front, "{lines:#?}");

        let lines = joined(&plan(&Preset::Flat.expand(), &trunk()));
        let last_self = lines.iter().rposition(|l| l.ends_with(" self")).unwrap();
        let on = lines
            .iter()
            .position(|l| l.contains("vlan_filtering 1"))
            .unwrap();
        assert!(last_self < on, "{lines:#?}");
    }

    /// Only the delete of the bridge's own entry may fail without stopping the
    /// plan; every other command still does.
    #[test]
    fn only_a_self_delete_is_best_effort() {
        let plan = plan(&Preset::Flat.expand(), &trunk());
        let tolerated: Vec<_> = plan.iter().filter(|c| is_best_effort(c)).collect();
        assert_eq!(
            joined(&tolerated.iter().map(|c| (*c).clone()).collect::<Vec<_>>()),
            ["bridge vlan del dev br0 vid 1 self"]
        );
        assert!(!is_best_effort(&cmd(&[
            "bridge", "vlan", "del", "dev", "node1", "vid", "1"
        ])));
        assert!(!is_best_effort(&cmd(&[
            "bridge", "vlan", "add", "dev", "br0", "vid", "10", "pvid", "untagged", "self"
        ])));
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
        let lines: Vec<_> = joined(&plan(&Preset::Split.expand(), &trunk()))
            .into_iter()
            .filter(|l| l.starts_with("bridge vlan") && !l.ends_with(" self"))
            .collect();
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
