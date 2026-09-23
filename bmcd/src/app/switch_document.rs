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

//! The switch's configuration, as one document.
//!
//! The most-asked feature on the public roadmap, and the one with the
//! sharpest failure: a wrong untagged VLAN on the BMC's own port takes the
//! board off the network, and the thing you would use to fix it is the thing
//! you just broke.
//!
//! ## Why a document rather than a set of calls
//!
//! Because partial application is how a board strands itself. Six `bridge
//! vlan add` calls where the fourth fails leave a switch in a state nobody
//! designed and nothing can describe. One object naming every port is
//! computed against the running state and applied as a single operation, and
//! rollback is then simply "apply the previous document" rather than an undo
//! log.
//!
//! It also means a preset is not a special mode. A preset is a named document,
//! and Custom is the document itself, so everything downstream -- validation,
//! apply, revert, the diagram in the interface -- has one shape to handle.
//!
//! ## Why the daemon expands presets, not the client
//!
//! So that every client previews the same thing. `GET .../switch/presets`
//! returns each preset's full table; an interface that expanded `Split`
//! itself would eventually disagree with the board about what `Split` means,
//! and the disagreement would show up as a lockout.
//!
//! This module is the model and its rules. Applying it to hardware, the
//! confirm window and persistence are separate, and deliberately cannot be
//! reached from here.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// 802.1Q reserves 0 and 4095; the usable range is everything between.
pub const VID_MIN: u16 = 1;
pub const VID_MAX: u16 = 4094;

/// The VLANs `Split` uses.
///
/// Arbitrary, and deliberately at the top of the range where nobody puts a
/// real VLAN. Under `Split` no tag ever leaves the board -- every port is
/// untagged -- so these numbers are invisible to the router and to the
/// operator, and the interface does not show them. They exist because the
/// switch needs two VLAN identifiers to keep two groups apart, not because
/// anyone should care which.
pub const SPLIT_MANAGEMENT_VID: u16 = 4093;
pub const SPLIT_NODE_VID: u16 = 4094;

/// The single VLAN `Flat` puts everything in, when filtering is off.
pub const FLAT_VID: u16 = 1;

/// How long a VLAN's name may be.
///
/// Thirty-two characters, which is a word or two -- enough for `storage` or
/// `guest wifi`, not enough for a sentence. A name is a label in a table
/// beside a number, and a table column that can be any width is a table that
/// stops being readable the first time somebody pastes into it.
pub const MAX_VLAN_NAME: usize = 32;

/// A port of the on-board switch.
///
/// Seven, not the six with netdevs. The BMC's own port has no interface in
/// `/sys/class/net` -- it is the CPU port -- but it is the one port whose
/// configuration can strand the board, so a document that could not name it
/// would be a document that could not be checked.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum PortId {
    Node1,
    Node2,
    Node3,
    Node4,
    /// The BMC's own port, port 4 on the switch, behind the 100 Mbit PHY.
    Bmc,
    Ge0,
    Ge1,
}

impl PortId {
    /// Every port, in the order a person reads the board: modules, then the
    /// BMC, then the uplinks.
    pub const ALL: [PortId; 7] = [
        PortId::Node1,
        PortId::Node2,
        PortId::Node3,
        PortId::Node4,
        PortId::Bmc,
        PortId::Ge0,
        PortId::Ge1,
    ];

    pub fn is_uplink(self) -> bool {
        matches!(self, PortId::Ge0 | PortId::Ge1)
    }

    pub fn is_node(self) -> bool {
        matches!(
            self,
            PortId::Node1 | PortId::Node2 | PortId::Node3 | PortId::Node4
        )
    }

    /// What to call it in a message an operator reads.
    pub fn label(self) -> &'static str {
        match self {
            PortId::Node1 => "node 1",
            PortId::Node2 => "node 2",
            PortId::Node3 => "node 3",
            PortId::Node4 => "node 4",
            PortId::Bmc => "the BMC's own port",
            PortId::Ge0 => "ge0",
            PortId::Ge1 => "ge1",
        }
    }
}

/// One port's membership.
///
/// `untagged` is the port's PVID: the VLAN an untagged frame arriving here
/// joins, and the VLAN whose frames leave here with the tag stripped. `tagged`
/// is everything else the port carries with tags on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PortConfig {
    /// `None` means the port is in no VLAN at all, which is how a disabled
    /// uplink is expressed. It is allowed: a port carrying nothing cannot
    /// strand anyone.
    #[serde(default)]
    pub untagged: Option<u16>,
    #[serde(default)]
    pub tagged: BTreeSet<u16>,
}

impl PortConfig {
    fn untagged(vid: u16) -> Self {
        PortConfig {
            untagged: Some(vid),
            tagged: BTreeSet::new(),
        }
    }

    fn tagged(vids: &[u16]) -> Self {
        PortConfig {
            untagged: None,
            tagged: vids.iter().copied().collect(),
        }
    }

    fn none() -> Self {
        PortConfig::default()
    }

    /// Every VLAN this port is in, tagged or not.
    pub fn vlans(&self) -> BTreeSet<u16> {
        let mut all = self.tagged.clone();
        all.extend(self.untagged);
        all
    }
}

/// The whole switch, as one object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SwitchDocument {
    /// False is `Flat`: the switch forwards without looking at VLANs and the
    /// port table below is inert. Kept in the document rather than implied by
    /// the table because "no VLANs" and "everything in one VLAN" are different
    /// things to the hardware, and only one of them is what a board ships as.
    pub vlan_filtering: bool,
    pub stp: bool,
    pub ports: BTreeMap<PortId, PortConfig>,
    /// What the operator calls each VLAN. A word beside a number, so that a
    /// layout is still legible to whoever opens this board next year.
    ///
    /// It reaches the hardware nowhere: [`crate::app::switch_applier::plan`]
    /// never reads it, and two documents differing only here produce no
    /// commands at all. It is carried because the alternative -- a name file
    /// beside the document -- is a second thing to keep in step with the
    /// first, and they would drift the first time somebody applied a preset.
    ///
    /// Naming a VLAN nobody is in yet is allowed: people name a layout while
    /// they are building it.
    ///
    /// Read by [`names_from_wire`], not by serde's own map handling. In JSON
    /// a key is always a string, and serde_json turns `"50"` into a `u16`
    /// when it reads this struct directly -- but `validate` and `PUT` read
    /// it through an `#[serde(untagged)]` enum, which buffers the input
    /// first, and the buffered form does not do that conversion. So a
    /// document with an empty `names` was accepted and a document with one
    /// name was refused as "did not match any variant", on both endpoints,
    /// from the day names existed until BMC-Firmware#59 reported it. Parsing
    /// the keys here works the same by either path.
    #[serde(default, deserialize_with = "names_from_wire")]
    pub names: BTreeMap<u16, String>,
}

/// `names` as it is on the wire: an object whose keys are VLAN ids written as
/// strings. Parsed here so it reads the same whether serde got here directly
/// or through a buffering (`untagged`) path. A key that is not a number is a
/// client plainly confused about what it is sending, and is named as such;
/// the range check stays in [`SwitchDocument::refusal`] with the other rules.
fn names_from_wire<'de, D>(deserializer: D) -> Result<BTreeMap<u16, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: BTreeMap<String, String> = BTreeMap::deserialize(deserializer)?;
    raw.into_iter()
        .map(|(key, name)| {
            key.trim()
                .parse::<u16>()
                .map(|vid| (vid, name))
                .map_err(|_| {
                    serde::de::Error::custom(format!(
                        "VLAN names are keyed by VLAN id; {key:?} is not one"
                    ))
                })
        })
        .collect()
}

/// Which uplink arrangement `Trunk` uses for ge1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SecondUplink {
    /// ge1 carries exactly what ge0 carries, and STP decides which one
    /// forwards. The default, because a second cable that does nothing until
    /// someone reconfigures the board is not redundancy.
    #[default]
    Redundant,
    /// ge1 in no VLAN: present, and carrying nothing.
    Off,
}

/// A named document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "preset", rename_all = "lowercase")]
pub enum Preset {
    /// One bridge, no filtering. What a board ships as, and the reset target.
    Flat,
    /// Two groups that never meet, and nothing tagged leaves the board: the
    /// BMC out of ge0, the modules out of ge1. For someone with two switch
    /// ports and no VLAN configuration on the other end.
    Split,
    /// One cable carrying both, tagged, for a router that knows the VLANs.
    /// The identifiers are the operator's, because the router has to match
    /// them and only they know what is free.
    Trunk {
        management_vid: u16,
        node_vid: u16,
        #[serde(default)]
        second_uplink: SecondUplink,
    },
}

impl Preset {
    /// The document this preset means.
    pub fn expand(self) -> SwitchDocument {
        let mut ports = BTreeMap::new();
        match self {
            Preset::Flat => {
                for port in PortId::ALL {
                    ports.insert(port, PortConfig::untagged(FLAT_VID));
                }
                SwitchDocument {
                    vlan_filtering: false,
                    stp: false,
                    ports,
                    // Nothing to name: filtering is off, so there is one
                    // network and it is the only one there has ever been.
                    names: BTreeMap::new(),
                }
            }
            Preset::Split => {
                for port in PortId::ALL {
                    let config = match port {
                        PortId::Bmc | PortId::Ge0 => PortConfig::untagged(SPLIT_MANAGEMENT_VID),
                        PortId::Ge1 => PortConfig::untagged(SPLIT_NODE_VID),
                        _ => PortConfig::untagged(SPLIT_NODE_VID),
                    };
                    ports.insert(port, config);
                }
                SwitchDocument {
                    vlan_filtering: true,
                    // The two groups share no VLAN and each reaches exactly one
                    // uplink, so there is no path for a loop to form. STP would
                    // cost a forwarding delay on every link change and prevent
                    // nothing.
                    stp: false,
                    ports,
                    // Deliberately unnamed. Under Split no tag leaves the
                    // board, so 4093 and 4094 are invisible to the router and
                    // to the operator; naming them would put two numbers on a
                    // page that exist only because the switch needs two.
                    names: BTreeMap::new(),
                }
            }
            Preset::Trunk {
                management_vid,
                node_vid,
                second_uplink,
            } => {
                for port in PortId::ALL {
                    let config = match port {
                        PortId::Bmc => PortConfig::untagged(management_vid),
                        PortId::Ge0 => PortConfig::tagged(&[management_vid, node_vid]),
                        PortId::Ge1 => match second_uplink {
                            SecondUplink::Redundant => {
                                PortConfig::tagged(&[management_vid, node_vid])
                            }
                            SecondUplink::Off => PortConfig::none(),
                        },
                        _ => PortConfig::untagged(node_vid),
                    };
                    ports.insert(port, config);
                }
                SwitchDocument {
                    vlan_filtering: true,
                    // Both uplinks carrying the same VLANs is a loop unless
                    // something breaks it. That is the refusal below, made
                    // true here.
                    stp: matches!(second_uplink, SecondUplink::Redundant),
                    ports,
                    // Named, unlike the other two presets, because these are
                    // the numbers the operator chose and has to match on the
                    // router. A tagged VLAN that leaves the board is a thing
                    // somebody will have to recognise elsewhere.
                    names: [
                        (management_vid, "management".to_string()),
                        (node_vid, "nodes".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                }
            }
        }
    }
}

/// Why a document will not be applied. Not a warning: the board says no.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Refusal {
    /// One sentence, addressed to the person who asked for it.
    pub reason: String,
}

impl Refusal {
    fn new(reason: impl Into<String>) -> Self {
        Refusal {
            reason: reason.into(),
        }
    }
}

/// Something worth saying that is not worth refusing over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Warning {
    pub port: Option<PortId>,
    pub reason: String,
}

impl SwitchDocument {
    /// Which ports are in `vid`, tagged or untagged.
    pub fn members(&self, vid: u16) -> BTreeSet<PortId> {
        self.ports
            .iter()
            .filter(|(_, config)| config.vlans().contains(&vid))
            .map(|(port, _)| *port)
            .collect()
    }

    /// Every VLAN with at least one member. A VLAN nobody is in is not an
    /// error; it simply is not created.
    pub fn vlans(&self) -> BTreeSet<u16> {
        self.ports
            .values()
            .flat_map(|config| config.vlans())
            .collect()
    }

    /// The rules the board will not bend, checked before anything is applied.
    ///
    /// Each one exists because breaking it produces a board nobody can reach,
    /// which is a different class of mistake from a configuration that merely
    /// does not do what was wanted.
    pub fn refusal(&self) -> Option<Refusal> {
        for (port, config) in &self.ports {
            for vid in config.vlans() {
                if !(VID_MIN..=VID_MAX).contains(&vid) {
                    return Some(Refusal::new(format!(
                        "VLAN {vid} on {} is outside 1-4094; 0 and 4095 are reserved by 802.1Q",
                        port.label()
                    )));
                }
            }
            if let Some(untagged) = config.untagged {
                if config.tagged.contains(&untagged) {
                    return Some(Refusal::new(format!(
                        "VLAN {untagged} is both tagged and untagged on {}; a port strips the tag \
                         or it does not",
                        port.label()
                    )));
                }
            }
        }

        // A name is checked for being a name, and for nothing else. It
        // cannot strand a board -- it never reaches the switch -- so the only
        // job here is to keep the table it will be drawn in readable, and to
        // catch the two cases where a client is plainly confused about what
        // it is sending.
        for (vid, name) in &self.names {
            if !(VID_MIN..=VID_MAX).contains(vid) {
                return Some(Refusal::new(format!(
                    "a name was given for VLAN {vid}, which is outside 1-4094; 0 and 4095 are \
                     reserved by 802.1Q"
                )));
            }
            if name.trim().is_empty() {
                return Some(Refusal::new(format!(
                    "the name for VLAN {vid} is blank. Leave the name out rather than sending an \
                     empty one."
                )));
            }
            if name.chars().count() > MAX_VLAN_NAME {
                return Some(Refusal::new(format!(
                    "the name for VLAN {vid} is longer than {MAX_VLAN_NAME} characters. It is a \
                     label in a table, not a description."
                )));
            }
            if name.chars().any(char::is_control) {
                return Some(Refusal::new(format!(
                    "the name for VLAN {vid} contains a control character."
                )));
            }
        }

        // Filtering off is one bridge. None of what follows can be true, and
        // checking the inert table would refuse documents that describe
        // nothing.
        if !self.vlan_filtering {
            return None;
        }

        let bmc = self.ports.get(&PortId::Bmc).cloned().unwrap_or_default();

        // THE LOCKOUT RULE. Without this the API is a way to make the board
        // unreachable using the board.
        let Some(management) = bmc.untagged else {
            return Some(Refusal::new(
                "the BMC's own port would be in no untagged VLAN, so nothing could reach this \
                 board. Give it exactly one.",
            ));
        };
        if !bmc.tagged.is_empty() {
            return Some(Refusal::new(
                "the BMC's own port carries tagged VLANs. This board's own network stack reads \
                 untagged frames only, so a tag here is traffic it cannot see.",
            ));
        }

        let with_bmc = self.members(management);
        if with_bmc.len() < 2 {
            return Some(Refusal::new(format!(
                "the BMC's own port would be alone in VLAN {management}. Nothing could reach this \
                 board: put an uplink in that VLAN too."
            )));
        }
        if !with_bmc.iter().any(|port| port.is_uplink()) {
            return Some(Refusal::new(format!(
                "VLAN {management} reaches this board but never leaves it -- no uplink is in it, \
                 so the board would answer only to the modules."
            )));
        }

        // A loop is not a mistake you get to discover on a live network.
        if !self.stp {
            let ge0 = self.ports.get(&PortId::Ge0).cloned().unwrap_or_default();
            let ge1 = self.ports.get(&PortId::Ge1).cloned().unwrap_or_default();
            let shared: Vec<u16> = ge0.vlans().intersection(&ge1.vlans()).copied().collect();
            if !shared.is_empty() {
                return Some(Refusal::new(format!(
                    "ge0 and ge1 would both carry VLAN {} with spanning tree off. Plugged into \
                     the same network that is a loop. Turn spanning tree on, or take one uplink \
                     out of that VLAN.",
                    shared[0]
                )));
            }
        }

        None
    }

    /// Everything else worth telling the operator, with the port it is about.
    pub fn warnings(&self) -> Vec<Warning> {
        let mut warnings = Vec::new();
        if !self.vlan_filtering {
            return warnings;
        }

        for port in PortId::ALL {
            let config = self.ports.get(&port).cloned().unwrap_or_default();
            if config.vlans().is_empty() {
                warnings.push(Warning {
                    port: Some(port),
                    reason: format!("{} is in no VLAN and will carry nothing.", port.label()),
                });
                continue;
            }
            if port.is_node() && config.untagged.is_none() {
                warnings.push(Warning {
                    port: Some(port),
                    reason: format!(
                        "{} carries only tagged VLANs. A module that does not tag its frames will \
                         not reach the network.",
                        port.label()
                    ),
                });
            }
        }

        for vid in self.vlans() {
            let members = self.members(vid);
            if members.len() == 1 {
                let only = members.iter().next().copied();
                warnings.push(Warning {
                    port: only,
                    reason: format!(
                        "VLAN {vid} has one member, {}. It can talk to nobody.",
                        only.map(PortId::label).unwrap_or("nothing")
                    ),
                });
            }
        }

        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trunk() -> Preset {
        Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Redundant,
        }
    }

    /// Every preset the board offers must be one the board will accept. A
    /// preset that its own rules refuse would be a button that cannot be
    /// pressed, and nobody would find out until they pressed it.
    #[test]
    fn every_preset_passes_the_rules_it_is_checked_against() {
        for preset in [
            Preset::Flat,
            Preset::Split,
            trunk(),
            Preset::Trunk {
                management_vid: 10,
                node_vid: 20,
                second_uplink: SecondUplink::Off,
            },
        ] {
            let document = preset.expand();
            assert_eq!(
                document.refusal(),
                None,
                "{preset:?} is refused by its own board: {:?}",
                document.refusal()
            );
        }
    }

    #[test]
    fn every_preset_names_all_seven_ports() {
        for preset in [Preset::Flat, Preset::Split, trunk()] {
            let document = preset.expand();
            assert_eq!(
                document.ports.len(),
                7,
                "{preset:?} leaves a port unnamed, so applying it would leave that port at \
                 whatever it happened to be"
            );
        }
    }

    /// The whole point of Split: two groups, each with its own way off the
    /// board, and nothing tagged anywhere.
    #[test]
    fn split_keeps_the_bmc_and_the_modules_apart() {
        let d = Preset::Split.expand();
        let management = d.ports[&PortId::Bmc].untagged.expect("the BMC has a VLAN");
        let nodes = d.ports[&PortId::Node1].untagged.expect("a node has a VLAN");
        assert_ne!(management, nodes);

        assert!(d.members(management).contains(&PortId::Ge0));
        assert!(!d.members(management).contains(&PortId::Node1));
        assert!(d.members(nodes).contains(&PortId::Ge1));
        assert!(!d.members(nodes).contains(&PortId::Bmc));

        for (port, config) in &d.ports {
            assert!(
                config.tagged.is_empty(),
                "{port:?} carries a tag, and under Split no tag leaves the board"
            );
        }
    }

    /// Redundant means the same VLANs on both cables, which is only safe
    /// because spanning tree comes with it.
    #[test]
    fn trunk_redundant_gives_both_uplinks_the_same_vlans_and_turns_on_stp() {
        let d = trunk().expand();
        assert_eq!(d.ports[&PortId::Ge0], d.ports[&PortId::Ge1]);
        assert!(
            d.stp,
            "two uplinks carrying the same VLANs without STP is a loop"
        );
    }

    #[test]
    fn trunk_with_the_second_uplink_off_leaves_ge1_carrying_nothing() {
        let d = Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Off,
        }
        .expand();
        assert!(d.ports[&PortId::Ge1].vlans().is_empty());
        assert!(
            !d.stp,
            "there is no loop to break, so no forwarding delay to pay"
        );
    }

    /// The refusal this whole module exists for.
    #[test]
    fn a_bmc_port_in_no_vlan_is_refused() {
        let mut d = trunk().expand();
        d.ports.insert(PortId::Bmc, PortConfig::none());
        let refusal = d.refusal().expect("that would strand the board");
        assert!(refusal.reason.contains("nothing could reach this board"));
    }

    #[test]
    fn a_bmc_port_alone_in_its_vlan_is_refused() {
        let mut d = trunk().expand();
        // A VLAN nobody else is in.
        d.ports.insert(PortId::Bmc, PortConfig::untagged(999));
        let refusal = d.refusal().expect("alone is unreachable");
        assert!(refusal.reason.contains("alone in VLAN 999"), "{refusal:?}");
    }

    /// Reachable from the modules is not reachable. A board you can only
    /// administer from the computers it administers is a board you cannot
    /// recover.
    #[test]
    fn a_bmc_vlan_that_never_leaves_the_board_is_refused() {
        let mut d = trunk().expand();
        d.ports.insert(PortId::Bmc, PortConfig::untagged(20));
        d.ports.insert(PortId::Ge0, PortConfig::tagged(&[10]));
        d.ports.insert(PortId::Ge1, PortConfig::none());
        let refusal = d.refusal().expect("no uplink carries the BMC's VLAN");
        assert!(refusal.reason.contains("never leaves it"), "{refusal:?}");
    }

    #[test]
    fn a_tagged_bmc_port_is_refused() {
        let mut d = trunk().expand();
        d.ports.insert(
            PortId::Bmc,
            PortConfig {
                untagged: Some(10),
                tagged: [20].into_iter().collect(),
            },
        );
        let refusal = d.refusal().expect("the BMC reads untagged frames only");
        assert!(
            refusal.reason.contains("untagged frames only"),
            "{refusal:?}"
        );
    }

    #[test]
    fn two_uplinks_sharing_a_vlan_without_stp_is_refused() {
        let mut d = trunk().expand();
        d.stp = false;
        let refusal = d.refusal().expect("that is a loop");
        assert!(refusal.reason.contains("loop"), "{refusal:?}");
    }

    /// ...and the same document with spanning tree on is fine, which is what
    /// makes the refusal a rule rather than a ban on redundancy.
    #[test]
    fn the_same_two_uplinks_with_stp_on_are_accepted() {
        assert_eq!(trunk().expand().refusal(), None);
    }

    #[test]
    fn a_reserved_vlan_id_is_refused() {
        for bad in [0u16, 4095] {
            let mut d = trunk().expand();
            d.ports.insert(PortId::Node1, PortConfig::untagged(bad));
            let refusal = d.refusal().unwrap_or_else(|| panic!("{bad} is reserved"));
            assert!(refusal.reason.contains("reserved by 802.1Q"), "{refusal:?}");
        }
    }

    #[test]
    fn a_vlan_both_tagged_and_untagged_on_one_port_is_refused() {
        let mut d = trunk().expand();
        d.ports.insert(
            PortId::Node1,
            PortConfig {
                untagged: Some(20),
                tagged: [20].into_iter().collect(),
            },
        );
        let refusal = d.refusal().expect("a port strips the tag or it does not");
        assert!(refusal.reason.contains("strips the tag"), "{refusal:?}");
    }

    /// Flat is the reset target, so it must be beyond refusing. Its port
    /// table is inert and must not be judged as though filtering were on.
    #[test]
    fn flat_is_never_refused_whatever_its_table_says() {
        let mut d = Preset::Flat.expand();
        d.ports.insert(PortId::Bmc, PortConfig::none());
        assert_eq!(
            d.refusal(),
            None,
            "with filtering off the table means nothing and Flat must stay reachable"
        );
    }

    /// An empty port is allowed -- it is how `off` works -- but it is worth
    /// saying out loud.
    #[test]
    fn a_port_in_no_vlan_warns_rather_than_refuses() {
        let d = Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Off,
        }
        .expand();
        assert_eq!(d.refusal(), None);
        assert!(d
            .warnings()
            .iter()
            .any(|w| w.port == Some(PortId::Ge1) && w.reason.contains("carry nothing")));
    }

    #[test]
    fn a_node_carrying_only_tags_is_warned_about() {
        let mut d = trunk().expand();
        d.ports.insert(PortId::Node2, PortConfig::tagged(&[20]));
        assert_eq!(
            d.refusal(),
            None,
            "it is not a lockout, so it is not refused"
        );
        assert!(
            d.warnings()
                .iter()
                .any(|w| w.port == Some(PortId::Node2)
                    && w.reason.contains("does not tag its frames"))
        );
    }

    #[test]
    fn a_vlan_with_one_member_is_warned_about() {
        let mut d = trunk().expand();
        d.ports.insert(PortId::Node3, PortConfig::untagged(77));
        assert!(d
            .warnings()
            .iter()
            .any(|w| w.reason.contains("VLAN 77 has one member")));
    }

    /// A VLAN nobody is in is not created and is not complained about.
    #[test]
    fn an_unused_vlan_simply_does_not_exist() {
        let d = trunk().expand();
        assert!(!d.vlans().contains(&999));
        assert!(d.members(999).is_empty());
    }

    #[test]
    fn trunk_names_the_two_vlans_the_router_has_to_match() {
        let d = trunk().expand();
        assert_eq!(d.names.get(&10).map(String::as_str), Some("management"));
        assert_eq!(d.names.get(&20).map(String::as_str), Some("nodes"));
    }

    /// Split's identifiers never leave the board, so putting them on a page
    /// would be showing somebody two numbers they can do nothing with.
    #[test]
    fn split_names_nothing() {
        assert!(Preset::Split.expand().names.is_empty());
    }

    #[test]
    fn a_blank_or_overlong_or_unprintable_name_is_refused() {
        for (name, expected) in [
            ("   ".to_string(), "blank"),
            ("x".repeat(MAX_VLAN_NAME + 1), "longer than"),
            ("stor\nage".to_string(), "control character"),
        ] {
            let mut d = trunk().expand();
            d.names.insert(20, name.clone());
            let refusal = d
                .refusal()
                .unwrap_or_else(|| panic!("{name:?} is not a name"));
            assert!(refusal.reason.contains(expected), "{refusal:?}");
        }
    }

    #[test]
    fn a_name_for_a_reserved_vlan_id_is_refused() {
        let mut d = trunk().expand();
        d.names.insert(4095, "nowhere".to_string());
        let refusal = d.refusal().expect("4095 is reserved");
        assert!(refusal.reason.contains("reserved by 802.1Q"), "{refusal:?}");
    }

    /// People name a layout while they are building it, and the VLAN comes
    /// after the word for it as often as the other way round.
    #[test]
    fn a_name_for_a_vlan_nobody_is_in_is_allowed() {
        let mut d = trunk().expand();
        d.names.insert(77, "storage".to_string());
        assert_eq!(d.refusal(), None);
        assert!(!d.vlans().contains(&77));
    }

    /// A name at the limit is a name. The off-by-one here would be found by
    /// somebody typing a name, which is the worst place to find one.
    #[test]
    fn a_name_of_exactly_the_maximum_length_is_accepted() {
        let mut d = trunk().expand();
        d.names.insert(20, "x".repeat(MAX_VLAN_NAME));
        assert_eq!(d.refusal(), None);
    }

    /// The document is what crosses the wire, so it has to survive the trip.
    #[test]
    fn a_document_round_trips_through_json() {
        let mut d = trunk().expand();
        d.names.insert(30, "storage".to_string());
        let json = serde_json::to_string(&d).expect("serialise");
        let back: SwitchDocument = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(d, back);
    }

    /// A client that predates names sends a document without the key, and the
    /// board has to read it rather than answer 400 to everything it says.
    #[test]
    fn a_document_with_no_names_key_is_still_a_document() {
        let json = r#"{"vlan_filtering":false,"stp":false,"ports":{}}"#;
        let d: SwitchDocument = serde_json::from_str(json).expect("names is optional");
        assert!(d.names.is_empty());
    }

    #[test]
    fn a_preset_round_trips_through_json() {
        for preset in [Preset::Flat, Preset::Split, trunk()] {
            let json = serde_json::to_string(&preset).expect("serialise");
            let back: Preset = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(preset, back, "{json}");
        }
    }
}
