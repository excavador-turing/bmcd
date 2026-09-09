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

//! The shapes handlers answer with, where the answer was a `json!` literal.
//!
//! Most read operations serialise a type that already exists, and the OpenAPI
//! document derives their schemas from it. These ten did not: they assembled
//! an object inline from several sources, so there was nothing to derive from
//! and the published document said only "not described here".
//!
//! That is a real cost. `openapi-typescript` generates `unknown` for an
//! undescribed body, which forces a cast at every use and is worse than the
//! hand-written interface it would replace.
//!
//! ## These are reconstructions, not designs
//!
//! Every field below is what the handler already sent, with the name it
//! already used. Nothing here is an improvement to the wire format, and a
//! rename would break `tpi` and the web interface for no gain. Where a shape
//! is odd -- and two of them are -- the oddity is preserved and explained
//! rather than quietly corrected.
//!
//! ## The single-element arrays
//!
//! `power`, `usb` and `sdcard` answer `[{…}]`: an array that always holds
//! exactly one object. That is upstream's convention and predates the fork.
//! It is described here as it is, because a client reading the document has
//! to index it, and a schema claiming an object would be a lie that compiles.

use schemars::JsonSchema;
use serde::Serialize;

/// `GET /api/bmc/about` -- what this board and this daemon are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct About {
    /// From the board's EEPROM. Empty on a board that cannot be read, rather
    /// than absent: upstream's shape, and the interface renders the blank.
    pub board_model: String,
    pub board_revision: String,
    /// `None` when the EEPROM field is missing or blank after trimming.
    pub board_serial: Option<String>,
    pub hostname: String,
    /// The `/api/bmc` contract version, not a release of anything.
    pub api: String,
    /// The firmware, from `/etc/os-release`. `"unknown"` when it cannot be
    /// read -- a string, because an About page with a gap is still worth
    /// rendering and this field has never been nullable.
    pub version: String,
    pub bmcd_version: String,
    /// The same value as `bmcd_version`. The web interface reads this key for
    /// its "Build version" field and renders `vundefined` without it, so it
    /// is duplicated deliberately rather than left for the client to alias.
    pub build_version: String,
    pub buildtime: String,
    pub buildroot: String,
    /// The running kernel. `"unknown"` when `uname` cannot be read; the one
    /// field an operator wants after a kernel bump.
    pub kernel: String,
}

/// `GET /api/bmc/power` -- one element, four nodes.
///
/// Each value is a **string**, not a boolean: `"0"`, `"1"`, or `"Unknown"`
/// when the daemon could not read the rail. Upstream's shape; a client that
/// wants a boolean has to handle the third case anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct NodePower {
    pub node1: String,
    pub node2: String,
    pub node3: String,
    pub node4: String,
}

/// `GET /api/bmc/usb` -- which node holds the bus, in which mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct UsbState {
    /// `Host`, `Device` or `Flash`.
    pub mode: crate::hal::UsbMode,
    /// The node, as a display string such as `Node 1`.
    pub node: String,
    /// Where the bus is routed: the alternative port, the BMC, or a node.
    pub route: crate::hal::UsbRoute,
    /// What the hardware is: which of the board's USB controllers is in play.
    pub bus_type: String,
}

/// `GET /api/bmc/sdcard` -- bytes, from `statvfs` on the mount point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SdCard {
    pub total: u64,
    /// Named `use`, not `used`. Upstream's spelling, and a Rust keyword,
    /// which is why the field is renamed rather than called what it means.
    #[serde(rename = "use")]
    pub used: u64,
    pub free: u64,
}

/// `GET /api/bmc/hostname` -- live, and after the next boot.
///
/// The two differ between a rename and the reboot that settles it, which is
/// the whole reason both are reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Hostname {
    /// What `sethostname(2)` currently says. `None` if it cannot be read.
    pub hostname: Option<String>,
    /// What `/etc/hostname` holds. `None` when the file is absent.
    pub on_next_boot: Option<String>,
}

/// `GET /api/bmc/ntp` -- the time sources, and how the clock is doing.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Ntp {
    pub servers: Vec<String>,
    /// False on an image whose `chrony.conf` predates the `sourcedir` line.
    /// Without it a saved list is written and silently never read, so the
    /// interface must be able to say the setting does nothing.
    pub configurable: bool,
    pub clock: crate::app::health_info::Clock,
}

/// `GET /api/bmc/info` -- what the Overview page reads.
///
/// Both halves are already typed elsewhere; this names the object that wraps
/// them so the document has one schema rather than an inline shape.
#[derive(Debug, Serialize, JsonSchema)]
pub struct BoardInfo {
    pub ip: Vec<crate::app::bmc_info::NetInfo>,
    pub storage: Vec<crate::app::bmc_info::StorageInfo>,
}
