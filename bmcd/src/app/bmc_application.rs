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
use crate::hal::helpers::bit_iterator;
use crate::hal::{NodeId, PinController, UsbMode, UsbRoute};
use crate::hal::{PowerController, UsbArchitecture};
use crate::persistency::app_persistency::ApplicationPersistency;
use crate::persistency::app_persistency::PersistencyBuilder;
use crate::serial_service::baud::{effective_baud, validate_baud, InvalidBaud};
use crate::serial_service::serial::SerialConnections;
use crate::usb_boot::NodeDrivers;
use crate::utils::{self, get_timestamp_unix};
use crate::{
    app::usb_gadget::append_msd_config_to_usb_gadget,
    app::usb_gadget::remove_msd_function_from_usb_gadget,
};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::c_ulong;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, OnceLock, PoisonError};
use std::time::Duration;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncRead, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, trace};

use super::cooling_device::{
    get_cooling_state, resume_all_governors, set_cooling_override, set_cooling_state,
    spawn_override_ceiling, CoolingDevice,
};

pub type NodeInfos = [NodeInfo; 4];
type CoolingMap = HashMap<u64, c_ulong>;

/// Stores which slots are actually used. This information is used to determine
/// for instance, which nodes need to be powered on, when such command is given
pub const ACTIVATED_NODES_KEY: &str = "activated_nodes";
/// stores to which node the USB multiplexer is configured to.
pub const USB_CONFIG: &str = "usb_config";
/// Stores information about nodes: name alias, time since powered on, and others. See [NodeInfo].
pub const NODE_INFO_KEY: &str = "node_info";
pub const NODE1_USB_MODE: &str = "node1_usb";
pub const COOLING_DEVICES: &str = "cooling_devices";
const COOLING_CAPACITY: usize = 10;

/// Describes the different configuration the USB bus can be setup
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum UsbConfig {
    /// USB-A port is host, NodeId is the device.
    UsbA(NodeId),
    /// BMC is host, NodeId is the device.
    Bmc(NodeId),
    /// NodeId is host, [UsbRoute] is configured for device
    Node(NodeId, UsbRoute),
    /// Configures the given node as a USB device with the usbboot pin high
    Flashing(NodeId, UsbRoute),
}

impl UsbConfig {
    /// The three facts every caller wants out of a configuration: which
    /// module it applies to, what that module is acting as, and where the
    /// other end of the bus is.
    ///
    /// One function because there are two callers -- the legacy `type=usb`
    /// response and the metrics exposition -- and a board that told an
    /// operator one thing through the API and another on a dashboard would
    /// be worse than a board that told them nothing.
    pub fn parts(&self) -> (NodeId, UsbMode, UsbRoute) {
        match *self {
            UsbConfig::UsbA(node) => (node, UsbMode::Device, UsbRoute::AlternativePort),
            UsbConfig::Bmc(node) => (node, UsbMode::Device, UsbRoute::Bmc),
            UsbConfig::Node(node, route) => (node, UsbMode::Host, route),
            UsbConfig::Flashing(node, route) => (node, UsbMode::Flash, route),
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct NodeInfo {
    pub name: Option<String>,
    pub module_name: Option<String>,
    pub power_on_time: Option<u64>,
    /// The speed of the node's serial console, in baud. `null` means the
    /// default, 115200. One of 300, 600, 1200, 2400, 4800, 9600, 19200,
    /// 38400, 57600, 115200, 230400, 460800, 500000, 576000, 921600,
    /// 1000000, 1152000, 1500000, 2000000, 2500000 or 3000000; anything else
    /// is refused with a 400. Takes effect at once on the open port, without
    /// disconnecting the web console, and again at every start of the
    /// daemon. Local tools on the BMC read the same console at
    /// `/dev/ttyNode1` to `/dev/ttyNode4` and cannot change the speed
    /// themselves. Posting `null` for it clears the stored speed and goes
    /// back to the default; leaving it out of a request keeps the speed.
    pub uart_baud: Option<u32>,
}

/// What a request to change a node's settings may carry. Apart from
/// [`NodeInfo`] because a request needs three states for `uart_baud` where
/// the stored setting has two: left out (keep it), a speed (set it) and
/// `null` (clear it, back to the default). `Option<u32>` cannot tell the
/// first from the last, which is how a posted `null` came to be accepted and
/// ignored.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct NodeInfoUpdate {
    pub name: Option<String>,
    pub module_name: Option<String>,
    /// `None` keeps the stored speed, `Some(None)` clears it, `Some(Some(b))`
    /// sets it.
    #[serde(default, with = "serde_with::rust::double_option")]
    pub uart_baud: Option<Option<u32>>,
}

/// A node setting that cannot be accepted. A caller's mistake, answered as a
/// 400: decided by this type and not by reading the message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("node {node}: {source}")]
pub struct NodeInfoRequestError {
    /// The node, 1 to 4, as numbered on the board.
    pub node: usize,
    #[source]
    pub source: InvalidBaud,
}

/// Checks every setting in a request before any of it is stored, so a
/// request with one bad value changes nothing.
fn validate_node_infos(
    infos: &HashMap<NodeId, NodeInfoUpdate>,
) -> Result<(), NodeInfoRequestError> {
    let mut nodes: Vec<_> = infos.iter().collect();
    nodes.sort_by_key(|(node, _)| **node as usize);
    for (node, info) in nodes {
        if let Some(Some(baud)) = info.uart_baud {
            validate_baud(baud).map_err(|source| NodeInfoRequestError {
                node: *node as usize + 1,
                source,
            })?;
        }
    }
    Ok(())
}

/// Applies a request to the stored settings and returns the serial speeds
/// the open ports are to be moved to: the one that was set, or the default for
/// a node whose speed was cleared with `null`.
fn apply_node_updates(
    stored: &mut NodeInfos,
    updates: HashMap<NodeId, NodeInfoUpdate>,
) -> Vec<(NodeId, u32)> {
    let mut new_bauds = Vec::new();
    for (node, info) in updates {
        let store_node = &mut stored[node as usize];

        if let Some(name) = info.name {
            store_node.name = Some(name);
        }

        if let Some(module_name) = info.module_name {
            store_node.module_name = Some(module_name);
        }

        if let Some(uart_baud) = info.uart_baud {
            store_node.uart_baud = uart_baud;
            new_bauds.push((node, effective_baud(uart_baud)));
        }
    }
    new_bauds
}

/// Where the node power state applied on start of the daemon comes from.
#[derive(Debug, PartialEq, Eq)]
enum StartupPowerState {
    /// Nodes were found powered: the daemon restarted underneath running
    /// nodes and must not interfere with them.
    Live(u8),
    /// All nodes were found off: a cold boot, restore the last stored state.
    Persisted(u8),
}

/// Decides which node power state to apply on start of the daemon.
///
/// # Arguments
///
/// * `live`        bit-field of the node enable lines as read back from the
///   hardware.
/// * `persisted`   bit-field of the node states last stored by the daemon.
fn startup_power_state(live: u8, persisted: u8) -> StartupPowerState {
    if live != 0 {
        StartupPowerState::Live(live)
    } else {
        StartupPowerState::Persisted(persisted)
    }
}

/// Lists the node numbers (1-based) set in the given bit-field, for logging.
fn powered_nodes(node_states: u8) -> Vec<usize> {
    bit_iterator(node_states, 0b1111)
        .filter_map(|(idx, state)| (state == 1).then_some(idx + 1))
        .collect()
}

/// Whether a node is changing power state, and to what.
///
/// # Arguments
///
/// * `activated_nodes`     bit-field of the nodes that are on now.
/// * `idx`                 index of the node, 0-based.
/// * `new_state`           the value the node is being set to, 0 or 1, as
///   [`bit_iterator`] yields it.
///
/// # Returns
///
/// `Some(true)` when the node is being powered on, `Some(false)` when it is
/// being powered off, and `None` when it is being left where it is.
///
/// The bit has to be shifted down before it can be compared with what
/// `bit_iterator` yields. `bit_iterator` returns the state as 0 or 1;
/// `activated_nodes & (1 << idx)` is 0 or `1 << idx`. Comparing the two
/// directly is only ever right for node 1.
fn power_transition(activated_nodes: u8, idx: usize, new_state: u8) -> Option<bool> {
    let current_state = (activated_nodes >> idx) & 1;
    (new_state != current_state).then_some(new_state == 1)
}

/// Updates the power-on stamps of the nodes in `mask` for a change from
/// `activated_nodes` to `node_states`: a node that is powered on gets `now`, a
/// node that is powered off loses its stamp, and a node left where it is keeps
/// whatever it has.
fn stamp_transitions(
    node_infos: &mut NodeInfos,
    activated_nodes: u8,
    node_states: u8,
    mask: u8,
    now: Option<u64>,
) {
    for (idx, new_state) in bit_iterator(node_states, mask) {
        match power_transition(activated_nodes, idx, new_state) {
            Some(true) => node_infos[idx].power_on_time = now,
            Some(false) => node_infos[idx].power_on_time = None,
            None => (),
        }
    }
}

/// The power-on stamps of the nodes after a cold boot, when the daemon is
/// about to power on every node in `state`. A cold boot cuts the power of every
/// node, so a node that comes back has just been powered on, whatever the stamp
/// from before the cut says, and a node that stays off has no stamp.
///
/// [`stamp_transitions`] cannot do this: it compares with the persisted state,
/// which is the state being restored, sees no change, and leaves the stamp of
/// the last switch from the UI or the API.
///
/// `now` is the wall clock. The BMC may have no RTC, and at a cold boot the
/// clock may not be synchronised yet, so the stamp can be off by the step the
/// clock takes when NTP lands: a step back saturates the uptime at 0 in
/// [`BmcApplication::get_node_infos`], a step forward adds to it. A stamp from
/// before the reboot carries the same step, and a wrong one is not made worse
/// by this.
fn stamp_cold_boot(node_infos: &mut NodeInfos, state: u8, now: Option<u64>) {
    for (idx, on) in bit_iterator(state, 0b1111) {
        node_infos[idx].power_on_time = if on == 1 { now } else { None };
    }
}

/// The keys of the daemon's store, with the values a board that has never run
/// it starts from.
fn persistency_builder() -> PersistencyBuilder {
    PersistencyBuilder::default()
        .register_key(ACTIVATED_NODES_KEY, &0u8)
        .register_key(USB_CONFIG, &UsbConfig::UsbA(NodeId::Node1))
        .register_key(NODE_INFO_KEY, &NodeInfos::default())
        .register_key(NODE1_USB_MODE, &false)
        .register_key(
            COOLING_DEVICES,
            &CoolingMap::with_capacity(COOLING_CAPACITY),
        )
}

/// A reset of a node that is off. A caller's mistake, answered as a 409: the
/// node's state does not allow it, and the message says what to do. Decided
/// by this type and not by reading the message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0} is off, so there is nothing to reset; switch it on first")]
pub struct NodeIsOff(pub NodeId);

/// What [`BmcApplication::power_off_for_cycle`] hands back: the node, and how
/// many times anything had set its power by then. Spent by
/// [`BmcApplication::power_on_unless_changed`], which switches the node back
/// on only when the count is still the same.
#[derive(Debug, Clone, Copy)]
pub struct PowerTicket {
    node: NodeId,
    epoch: u64,
}

pub struct BmcApplication {
    pub(super) pin_controller: PinController,
    pub(super) power_controller: PowerController,
    pub(super) app_db: ApplicationPersistency,
    node_drivers: NodeDrivers,
    /// Serialises every change of node power: the stored state and the rails
    /// are written together under it, so two requests cannot interleave and
    /// leave the state saying one thing and the rails doing another. It is held
    /// for the writes of one change only, never while waiting out
    /// [`Self::power_settle`].
    power_lock: tokio::sync::Mutex<()>,
    /// How many times each node's power has been set, one counter per node.
    /// Bumped under `power_lock` by every request that names the node, so a
    /// sequence that switches a node off, waits, and switches it on (a reset,
    /// the end of a flash) can tell whether anybody else touched the node while
    /// it waited. Never held across an `.await`.
    power_epochs: std::sync::Mutex<[u64; 4]>,
    /// How long a node stays unpowered in such a sequence.
    power_settle: Duration,
    /// The console ports, once they are open: a new serial speed is applied
    /// to them as it is stored. Set once by [`Self::attach_serial`].
    serial: OnceLock<Arc<SerialConnections>>,
}

impl BmcApplication {
    pub async fn new(database_write_timeout: Option<Duration>) -> anyhow::Result<Self> {
        let model_string = std::fs::read_to_string("/proc/device-tree/model");
        let is_legacy_dts = matches!(model_string, Ok(model) if model.contains("v2.4"));
        let pin_controller = PinController::new(is_legacy_dts).context("pin_controller")?;
        let power_controller = PowerController::new(is_legacy_dts).context("power_controller")?;
        let app_db = persistency_builder()
            .write_timeout(database_write_timeout)
            .build()
            .await?;

        let node_drivers = NodeDrivers::new();

        let instance = Self {
            pin_controller,
            power_controller,
            app_db,
            node_drivers,
            power_lock: tokio::sync::Mutex::new(()),
            power_epochs: std::sync::Mutex::new([0; 4]),
            power_settle: crate::hal::POWER_OFF_SETTLE,
            serial: OnceLock::new(),
        };

        instance.initialize().await?;
        Ok(instance)
    }

    /// A daemon on the simulated board, with its store in `dir` and waiting
    /// `power_settle` between a switch-off and a switch-on. Not `initialize`d:
    /// that reads sysfs and starts the cooling governor, and a test has the
    /// power state to itself.
    #[cfg(all(test, feature = "stubbed"))]
    pub(crate) async fn for_test(dir: &std::path::Path, power_settle: Duration) -> Self {
        let app_db = persistency_builder()
            .build_at(dir.join("bmcd.bin"))
            .await
            .expect("a store in a temporary directory");
        Self {
            pin_controller: PinController::new(false).expect("stub pin controller"),
            power_controller: PowerController::new(false).expect("stub power controller"),
            app_db,
            node_drivers: NodeDrivers::new(),
            power_lock: tokio::sync::Mutex::new(()),
            power_epochs: std::sync::Mutex::new([0; 4]),
            power_settle,
            serial: OnceLock::new(),
        }
    }

    /// The rails of the simulated board, as `get_power_node` reads them.
    #[cfg(all(test, feature = "stubbed"))]
    pub(crate) fn rails(&self) -> u8 {
        self.power_controller
            .get_power_node()
            .expect("the stub reads back")
    }

    /// toggles the power state of the nodes. When `inverse_toggle` == true, and
    /// not all nodes are off nor on, it will turn off all nodes instead of
    /// turning them on.
    ///
    /// # State table
    ///
    /// | state             | long_press | All nodes |
    /// | :---------------- | :--------: | :-------: |
    /// | 0b0000            |  False     | On        |
    /// | 0b0111            |  False     | Off       |
    /// | 0b1111            |  False     | Off       |
    /// | 0b0000            |  True      | On        |
    /// | 0b0111            |  True      | On        |
    /// | 0b1111            |  True      | Off       |
    ///
    /// # Return
    ///
    /// returns Err(e) on an internal gpio error or when there is an error
    /// writing power LED status.
    pub async fn toggle_power_states(&self, inverse_toggle: bool) -> anyhow::Result<()> {
        // The state is read and written under one hold of the lock, so a
        // request that lands in between cannot be toggled over.
        let _power = self.power_lock.lock().await;
        let node_values = self.app_db.try_get::<u8>(ACTIVATED_NODES_KEY).await?;

        let mut on = node_values == 0;
        if inverse_toggle && node_values != 0 && node_values != 0b1111 {
            on = !on;
        }

        let node_values = if on { 0b1111 } else { 0b0000 };
        self.activate_slot_locked(node_values, 0b1111).await
    }

    async fn initialize(&self) -> anyhow::Result<()> {
        self.initialize_usb_mode().await?;
        self.initialize_power().await?;
        self.initialize_cooling().await
    }

    /// Applies the node power state on start of the daemon. A restart of the
    /// daemon, or a reboot of the BMC, must not interfere with nodes that are
    /// running: on a latching board the node power latches outlive a reboot of
    /// the BMC, so when any node reads powered, that live state is the truth
    /// and the persisted state is updated to match. Only when all of them read
    /// off (a cold boot, which clears the latches) the persisted state is
    /// restored. See [`PowerController::get_power_node`] for where the live
    /// state comes from.
    async fn initialize_power(&self) -> anyhow::Result<()> {
        let persisted = self.app_db.try_get::<u8>(ACTIVATED_NODES_KEY).await?;
        let live = self.power_controller.get_power_node().unwrap_or_else(|e| {
            tracing::warn!("cannot read node power state, assuming cold boot: {:#}", e);
            0
        });

        let node_states = match startup_power_state(live, persisted) {
            StartupPowerState::Live(state) => {
                info!("adopted live power state: nodes={:?}", powered_nodes(state));
                state
            }
            StartupPowerState::Persisted(state) => {
                info!(
                    "cold boot: restoring persisted power state: nodes={:?}",
                    powered_nodes(state)
                );
                // The nodes lost their power with the BMC: the ones that come
                // back are powered on now. `activate_slot` compares with the
                // persisted state and would see no change in them.
                self.stamp_cold_boot_power_on_times(state).await?;
                state
            }
        };

        self.activate_slot(node_states, 0b1111).await
    }

    #[instrument(skip(self), fields(alternative_port, config))]
    async fn initialize_usb_mode(&self) -> anyhow::Result<()> {
        if self.pin_controller.usb_bus_type() == UsbArchitecture::UsbHub {
            let alternative_port = self.app_db.try_get::<bool>(NODE1_USB_MODE).await?;
            self.pin_controller.set_node1_usb_route(alternative_port)?;
            tracing::Span::current().record("alternative_port", alternative_port);
        }

        let config = self.app_db.try_get::<UsbConfig>(USB_CONFIG).await?;
        tracing::Span::current().record("config", format!("{:?}", config));
        self.configure_usb(config).await.context("USB configure")
    }

    async fn initialize_cooling(&self) -> anyhow::Result<()> {
        // Before anything is read or replayed: a hold on the fan lives in the
        // kernel, not in this process, so one left by a daemon that died is
        // still in force now. Clearing it first also means the persisted
        // speeds below are replayed under a running governor, as they always
        // were.
        resume_all_governors().await;

        let store = self.app_db.try_get::<CoolingMap>(COOLING_DEVICES).await?;
        let devices = get_cooling_state().await;

        info!(
            "found devices: {}, stored: {}",
            devices
                .iter()
                .map(|d| d.device.clone())
                .collect::<Vec<String>>()
                .join(","),
            store.len()
        );

        let mut set_devices = Vec::new();
        for dev in devices {
            let mut hasher = DefaultHasher::new();
            dev.device.hash(&mut hasher);
            let hash = hasher.finish();

            if let Some(speed) = store.get(&hash) {
                set_cooling_state(&dev.device, speed).await?;
                set_devices.push((hash, *speed));
            }
        }

        // cleanup storage
        let map: CoolingMap = HashMap::from_iter(set_devices);
        info!("loaded cooling devices: {:?}", map);
        self.app_db.try_set(COOLING_DEVICES, map).await?;

        spawn_override_ceiling();

        Ok(())
    }

    pub async fn get_usb_mode(&self) -> anyhow::Result<(UsbConfig, String)> {
        Ok((
            self.app_db.try_get::<UsbConfig>(USB_CONFIG).await?,
            self.pin_controller.usb_bus_type().to_string(),
        ))
    }

    /// routine to support legacy API
    pub async fn get_node_power(&self, node: NodeId) -> anyhow::Result<bool> {
        let state = self.app_db.try_get::<u8>(ACTIVATED_NODES_KEY).await?;
        Ok(state & node.to_bitfield() != 0)
    }

    /// This function is used to active a given node. Call this function if a
    /// module is inserted at that slot. Failing to call this method means that
    /// this slot is not considered for power up and power down commands.
    pub async fn activate_slot(&self, node_states: u8, mask: u8) -> anyhow::Result<()> {
        let _power = self.power_lock.lock().await;
        self.activate_slot_locked(node_states, mask).await
    }

    /// [`Self::activate_slot`] for a caller that already holds `power_lock`.
    async fn activate_slot_locked(&self, node_states: u8, mask: u8) -> anyhow::Result<()> {
        trace!(
            "activate slot. node_states={:#06b}, mask={:#06b}",
            node_states,
            mask
        );
        ensure!(mask != 0);
        self.count_power_request(mask);

        let state = self.app_db.try_get::<u8>(ACTIVATED_NODES_KEY).await?;
        let new_state = (state & !mask) | (node_states & mask);

        self.update_power_on_times(state, node_states, mask).await?;

        self.app_db
            .try_set::<u8>(ACTIVATED_NODES_KEY, new_state)
            .await?;
        debug!("node activated bits updated:{:#06b}.", new_state);

        let led = new_state != 0;
        self.power_controller
            .power_led(led)
            .await
            .unwrap_or_else(|e| tracing::warn!("power LED error: {:#}", e));

        // also update the actual power state accordingly
        self.power_controller
            .set_power_node(node_states, mask)
            .await
    }

    /// Counts one request against every node in `mask`, whether or not it
    /// changes what the node is doing: a power-off for a node that is already
    /// off still says what the caller wants, and has to cancel a switch-on that
    /// is waiting.
    fn count_power_request(&self, mask: u8) {
        let mut epochs = self
            .power_epochs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (idx, epoch) in epochs.iter_mut().enumerate() {
            if mask & (1 << idx) != 0 {
                *epoch = epoch.wrapping_add(1);
            }
        }
    }

    fn power_epoch(&self, node: NodeId) -> u64 {
        self.power_epochs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)[node as usize]
    }

    /// Switches `node` off, through the same path as a power request, and
    /// returns the ticket that [`Self::power_on_unless_changed`] needs to
    /// switch it on again.
    pub async fn power_off_for_cycle(&self, node: NodeId) -> anyhow::Result<PowerTicket> {
        let _power = self.power_lock.lock().await;
        self.power_off_locked(node).await
    }

    async fn power_off_locked(&self, node: NodeId) -> anyhow::Result<PowerTicket> {
        self.activate_slot_locked(0, node.to_bitfield()).await?;
        Ok(PowerTicket {
            node,
            epoch: self.power_epoch(node),
        })
    }

    /// Waits out the settle time, then [`Self::power_on_unless_changed`]. The
    /// wait holds no lock.
    pub async fn power_on_after_settle(&self, ticket: PowerTicket) -> anyhow::Result<bool> {
        sleep(self.power_settle).await;
        self.power_on_unless_changed(ticket).await
    }

    /// Switches the node of `ticket` back on, unless something else has set its
    /// power since the ticket was issued: a power-off that arrived while it
    /// waited stays the last word, and so does a power-on, which has already
    /// done it.
    ///
    /// # Returns
    ///
    /// `Ok(true)` when the node was switched on, `Ok(false)` when it was left
    /// as the other request set it.
    pub async fn power_on_unless_changed(&self, ticket: PowerTicket) -> anyhow::Result<bool> {
        let bit = ticket.node.to_bitfield();
        // The check and the switch-on are one hold of the lock. A power-off
        // that comes after it is applied after the rails are on, and wins.
        let _power = self.power_lock.lock().await;
        if self.power_epoch(ticket.node) != ticket.epoch {
            return Ok(false);
        }
        self.activate_slot_locked(bit, bit).await?;
        Ok(true)
    }

    /// Stores the power-on stamps of a cold boot, see [`stamp_cold_boot`].
    async fn stamp_cold_boot_power_on_times(&self, state: u8) -> anyhow::Result<()> {
        let mut node_infos = self
            .app_db
            .try_get::<NodeInfos>(NODE_INFO_KEY)
            .await
            .unwrap_or_default();
        stamp_cold_boot(&mut node_infos, state, get_timestamp_unix());
        self.app_db
            .try_set::<NodeInfos>(NODE_INFO_KEY, node_infos)
            .await?;
        Ok(())
    }

    #[instrument(skip(self))]
    async fn update_power_on_times(
        &self,
        activated_nodes: u8,
        node_states: u8,
        mask: u8,
    ) -> anyhow::Result<()> {
        let mut node_infos = self
            .app_db
            .try_get::<NodeInfos>(NODE_INFO_KEY)
            .await
            .unwrap_or_default();

        stamp_transitions(
            &mut node_infos,
            activated_nodes,
            node_states,
            mask,
            get_timestamp_unix(),
        );

        self.app_db
            .try_set::<NodeInfos>(NODE_INFO_KEY, node_infos)
            .await?;
        Ok(())
    }

    pub async fn set_node1_usb_route(&self, alternative_port: bool) -> anyhow::Result<()> {
        info!("changed node1 usb route. port= {}", alternative_port);
        self.pin_controller.set_node1_usb_route(alternative_port)?;
        self.app_db
            .try_set(NODE1_USB_MODE, alternative_port)
            .await?;
        Ok(())
    }

    pub async fn get_node1_usb_route(&self) -> anyhow::Result<bool> {
        Ok(self.app_db.try_get::<bool>(NODE1_USB_MODE).await?)
    }

    pub async fn configure_usb(&self, config: UsbConfig) -> anyhow::Result<()> {
        self.configure_usb_internal(config).await?;
        self.app_db.try_set(USB_CONFIG, config).await?;
        Ok(())
    }

    async fn configure_usb_internal(&self, config: UsbConfig) -> anyhow::Result<()> {
        tracing::info!("changing usb config to {:?}", config);
        let (mode, dest, route) = match config {
            UsbConfig::UsbA(device) => (UsbMode::Device, device, UsbRoute::AlternativePort),
            UsbConfig::Bmc(device) => (UsbMode::Device, device, UsbRoute::Bmc),
            UsbConfig::Flashing(device, route) => (UsbMode::Flash, device, route),
            UsbConfig::Node(host, route) => (UsbMode::Host, host, route),
        };

        if mode != UsbMode::Flash {
            if let Err(e) = remove_msd_function_from_usb_gadget().await {
                tracing::error!("{:#}", e);
            }
        }

        self.pin_controller.set_usb_route(route)?;
        self.pin_controller.select_usb(dest, mode)?;

        Ok(())
    }

    pub async fn usb_boot(&self, node: NodeId, on: bool) -> anyhow::Result<()> {
        let node_bits = node.to_bitfield();
        let (state, mask) = if on {
            (node_bits, node_bits)
        } else {
            (0u8, node_bits)
        };
        Ok(self.pin_controller.set_usb_boot(state, mask)?)
    }

    pub async fn rtl_reset(&self) -> anyhow::Result<()> {
        tokio::spawn(async {
            sleep(Duration::from_secs(1)).await;
            info!("restarting br0 ethernet adapter");
            let status = Command::new("sh")
                .arg("-c")
                .arg("ifdown br0 && ifup br0")
                .status()
                .unwrap();

            if !status.success() {
                tracing::error!("reset of network interface returned: {}", status);
            }
        });

        Ok(())
    }

    /// Resets a node: off, [`Self::power_settle`] without power, on.
    ///
    /// Both switches go through the same path and the same lock as a request to
    /// switch the node, so the power the API reports is what the rails do: the
    /// node reads off while it is held off, and on again when it is back. The
    /// lock is not held while waiting, so a power request is not delayed by a
    /// reset; one that names this node cancels the switch-on that is waiting,
    /// and a power-off therefore leaves the node off.
    ///
    /// A node that is off is refused with [`NodeIsOff`]: there is nothing to
    /// reset, and switching it on would be a power-on the caller did not ask
    /// for.
    pub async fn reset_node(&self, node: NodeId) -> anyhow::Result<()> {
        debug!("reset node {:?}", node);
        // The check and the switch-off are one hold of the lock.
        let ticket = {
            let _power = self.power_lock.lock().await;
            if !self.get_node_power(node).await? {
                return Err(NodeIsOff(node).into());
            }
            self.power_off_locked(node).await?
        };

        if !self.power_on_after_settle(ticket).await? {
            info!("reset of {node}: its power was set meanwhile, left as that request set it");
        }
        Ok(())
    }

    pub async fn node_in_msd(&self, node: NodeId) -> anyhow::Result<PathBuf> {
        // stop_usb_gadget_if_running().await?;

        self.reboot_into_usb(node, UsbConfig::Flashing(node, UsbRoute::Bmc))
            .await?;
        let blk_dev = self
            .node_drivers
            .load_as_block_device(node, &CancellationToken::new())
            .await?;

        if let Err(e) = append_msd_config_to_usb_gadget(&blk_dev).await {
            tracing::error!("msd usb-gadget: {:#}", e);
        } else {
            info!("BMC-OTG: Node mass storage CDC enabled");
        }

        Ok(blk_dev)
    }

    pub async fn node_in_flash(
        &self,
        node: NodeId,
        router: UsbRoute,
        cancel: &CancellationToken,
    ) -> anyhow::Result<impl 'static + AsyncRead + AsyncWrite + AsyncSeek + Unpin> {
        self.reboot_into_usb(node, UsbConfig::Flashing(node, router))
            .await?;
        Ok(self.node_drivers.load_as_stream(node, cancel).await?)
    }

    async fn reboot_into_usb(&self, node: NodeId, config: UsbConfig) -> anyhow::Result<()> {
        tracing::info!("Powering off node {:?}...", node);
        self.activate_slot(!node.to_bitfield(), node.to_bitfield())
            .await?;
        self.configure_usb_internal(config).await?;
        // A node that was running needs this long without power to come back
        // up from cold, and only a cold boot reads the USB boot pin.
        tokio::time::sleep(self.power_settle).await;

        tracing::info!("Powering on...");
        self.activate_slot(node.to_bitfield(), node.to_bitfield())
            .await?;

        tokio::time::sleep(Duration::from_secs(1)).await;

        self.clear_usb_boot()
    }

    pub fn clear_usb_boot(&self) -> anyhow::Result<()> {
        self.pin_controller
            .set_usb_boot(0u8, 0b1111)
            .context("error clearing usbboot")
    }

    pub async fn reboot(&self, fel: bool) -> anyhow::Result<()> {
        if fel {
            let mut mem = OpenOptions::new().write(true).open("/dev/mem").await?;
            mem.seek(std::io::SeekFrom::Start(0x0709_0108)).await?;
            mem.write_u32(0x5AA5_A55A).await?;
            tracing::warn!("system reboot into FEL");
        }

        self.power_controller
            .status_led(true)
            .await
            .unwrap_or_else(|e| tracing::warn!("status_led: {:#}", e));

        Command::new("shutdown").args(["-r", "now"]).spawn()?;
        Ok(())
    }

    /// The serial speed of each node's console, as stored: what the ports are
    /// opened at when the daemon starts. 115200 for a node with none, and for
    /// every node when the stored node info cannot be read: the consoles
    /// still open, and the error is logged.
    pub async fn node_bauds(&self) -> [u32; 4] {
        let stored = match self.app_db.try_get::<NodeInfos>(NODE_INFO_KEY).await {
            Ok(stored) => stored,
            Err(e) => {
                tracing::error!("stored node info cannot be read, consoles open at 115200: {e}");
                NodeInfos::default()
            }
        };
        std::array::from_fn(|i| effective_baud(stored[i].uart_baud))
    }

    /// Hands over the console ports, so that a change of a node's serial
    /// speed reaches the open port. Only the first call has an effect.
    pub fn attach_serial(&self, serial: Arc<SerialConnections>) {
        let _ = self.serial.set(serial);
    }

    #[instrument(skip(self))]
    pub async fn set_node_info(
        &self,
        new_info: HashMap<NodeId, NodeInfoUpdate>,
    ) -> anyhow::Result<()> {
        validate_node_infos(&new_info)?;
        let mut stored_nodes = self.app_db.try_get::<NodeInfos>(NODE_INFO_KEY).await?;

        let new_bauds = apply_node_updates(&mut stored_nodes, new_info);

        self.app_db
            .try_set::<NodeInfos>(NODE_INFO_KEY, stored_nodes)
            .await?;

        // After it is stored, so that a speed that was applied is a speed
        // that survives the next start.
        if let Some(serial) = self.serial.get() {
            for (node, baud) in new_bauds {
                serial.set_baud_rate(node, baud);
            }
        }

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn get_node_infos(&self) -> anyhow::Result<NodeInfos> {
        let Some(current_time) = utils::get_timestamp_unix() else {
            anyhow::bail!("Current time before Unix epoch");
        };

        let mut node_infos = self.app_db.try_get::<NodeInfos>(NODE_INFO_KEY).await?;
        node_infos.iter_mut().for_each(|info| {
            if let Some(time) = &mut info.power_on_time {
                // saturating: the stored value is a wall-clock stamp, and the
                // BMC has no RTC, so a clock that steps backwards once NTP
                // lands would otherwise underflow this into an enormous
                // uptime.
                *time = current_time.saturating_sub(*time);
            }
        });

        Ok(node_infos)
    }

    pub async fn set_cooling_speed(&self, device: &str, speed: c_ulong) -> anyhow::Result<()> {
        let res = set_cooling_state(device, &speed).await;

        if res.is_ok() {
            let mut cooling = self.app_db.try_get::<CoolingMap>(COOLING_DEVICES).await?;
            if cooling.len() < COOLING_CAPACITY {
                let mut hasher = DefaultHasher::new();
                device.hash(&mut hasher);
                let value = cooling.entry(hasher.finish()).or_default();
                *value = speed;
                self.app_db.try_set(COOLING_DEVICES, cooling).await?;
            } else {
                tracing::warn!(
                    "cooling devices persistency full, no room to persist speed of '{}'",
                    device
                );
            }
        }

        res
    }

    /// Hold a fan at a step, or hand it back to the kernel's governor.
    ///
    /// Deliberately not persisted, which is the one way this differs from
    /// [`Self::set_cooling_speed`]. A persisted speed is replayed into a
    /// board whose governor immediately overrules it, so the worst it can do
    /// is be briefly wrong. A persisted *hold* would come back after a reboot
    /// with nothing regulating the fan and nobody present who remembers
    /// asking for it, which is not a state to restore from a database.
    pub async fn set_cooling_override(
        &self,
        device: &str,
        speed: Option<c_ulong>,
    ) -> anyhow::Result<()> {
        set_cooling_override(device, speed).await
    }

    pub async fn get_cooling_devices() -> anyhow::Result<Vec<CoolingDevice>> {
        Ok(get_cooling_state().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(entries: &[(NodeId, Option<u32>)]) -> HashMap<NodeId, NodeInfoUpdate> {
        entries
            .iter()
            .map(|(node, uart_baud)| {
                (
                    *node,
                    NodeInfoUpdate {
                        uart_baud: Some(*uart_baud),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    fn parse(json: serde_json::Value) -> HashMap<NodeId, NodeInfoUpdate> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn a_request_tells_a_speed_from_null_from_nothing() {
        let infos = parse(serde_json::json!({
            "Node1": { "name": "only a name" },
            "Node2": { "uart_baud": 1500000 },
            "Node3": { "uart_baud": null },
        }));
        assert_eq!(infos[&NodeId::Node1].uart_baud, None);
        assert_eq!(infos[&NodeId::Node2].uart_baud, Some(Some(1_500_000)));
        assert_eq!(infos[&NodeId::Node3].uart_baud, Some(None));
    }

    #[test]
    fn null_clears_the_stored_speed_and_returns_the_port_to_the_default() {
        let mut stored = NodeInfos::default();
        stored[1].uart_baud = Some(1_500_000);
        stored[2].uart_baud = Some(9_600);

        let moved = apply_node_updates(
            &mut stored,
            parse(serde_json::json!({ "Node2": { "uart_baud": null } })),
        );

        assert_eq!(stored[1].uart_baud, None);
        assert_eq!(moved, vec![(NodeId::Node2, 115_200)]);
        // The node that was not in the request keeps its speed.
        assert_eq!(stored[2].uart_baud, Some(9_600));
    }

    #[test]
    fn a_request_that_leaves_the_speed_out_keeps_it() {
        let mut stored = NodeInfos::default();
        stored[0].uart_baud = Some(57_600);

        let moved = apply_node_updates(
            &mut stored,
            parse(serde_json::json!({ "Node1": { "name": "renamed" } })),
        );

        assert_eq!(stored[0].uart_baud, Some(57_600));
        assert_eq!(stored[0].name.as_deref(), Some("renamed"));
        assert!(moved.is_empty());
    }

    #[test]
    fn a_speed_is_stored_and_applied() {
        let mut stored = NodeInfos::default();
        let moved = apply_node_updates(
            &mut stored,
            parse(serde_json::json!({ "Node4": { "uart_baud": 921600 } })),
        );
        assert_eq!(stored[3].uart_baud, Some(921_600));
        assert_eq!(moved, vec![(NodeId::Node4, 921_600)]);
    }

    #[test]
    fn null_is_accepted_by_the_validation() {
        assert_eq!(
            validate_node_infos(&parse(
                serde_json::json!({ "Node1": { "uart_baud": null } })
            )),
            Ok(())
        );
    }

    #[test]
    fn a_request_with_valid_or_no_speeds_is_accepted() {
        assert_eq!(validate_node_infos(&request(&[])), Ok(()));
        assert_eq!(
            validate_node_infos(&request(&[
                (NodeId::Node1, None),
                (NodeId::Node2, Some(1_500_000))
            ])),
            Ok(())
        );
    }

    #[test]
    fn a_bad_speed_is_refused_and_names_the_node() {
        let err = validate_node_infos(&request(&[
            (NodeId::Node1, Some(115_200)),
            (NodeId::Node3, Some(12_345)),
        ]))
        .unwrap_err();
        assert_eq!(err.node, 3);
        assert_eq!(err.source, InvalidBaud(12_345));
        assert!(err
            .to_string()
            .starts_with("node 3: 12345 is not a supported"));
    }

    #[test]
    fn the_first_bad_node_is_the_one_reported() {
        let err = validate_node_infos(&request(&[
            (NodeId::Node4, Some(1)),
            (NodeId::Node2, Some(2)),
        ]))
        .unwrap_err();
        assert_eq!(err.node, 2);
    }

    #[test]
    fn a_bad_speed_is_a_400_with_the_plain_message() {
        let err = validate_node_infos(&request(&[(NodeId::Node1, Some(7))])).unwrap_err();
        let response =
            crate::api::into_legacy_response::LegacyResponse::from(anyhow::Error::from(err));
        let crate::api::into_legacy_response::LegacyResponse::Error(status, message) = response
        else {
            panic!("expected an error response");
        };
        assert_eq!(status, actix_web::http::StatusCode::BAD_REQUEST);
        assert!(
            message.starts_with("node 1: 7 is not a supported serial speed"),
            "{message}"
        );
    }

    #[test]
    fn startup_adopts_live_state_when_any_node_is_on() {
        assert_eq!(
            startup_power_state(0b0001, 0b1111),
            StartupPowerState::Live(0b0001)
        );
        assert_eq!(
            startup_power_state(0b1111, 0b0000),
            StartupPowerState::Live(0b1111)
        );
        assert_eq!(
            startup_power_state(0b0101, 0b0101),
            StartupPowerState::Live(0b0101)
        );
    }

    #[test]
    fn startup_restores_persisted_state_on_cold_boot() {
        assert_eq!(
            startup_power_state(0b0000, 0b1111),
            StartupPowerState::Persisted(0b1111)
        );
        assert_eq!(
            startup_power_state(0b0000, 0b0000),
            StartupPowerState::Persisted(0b0000)
        );
    }

    #[test]
    fn a_node_that_stays_on_keeps_its_power_on_time() {
        // The regression. All four nodes on, all four asked to stay on, which
        // is what `initialize_power` does on every start of the daemon. Every
        // node has to come back None -- nothing changed, so no timestamp may
        // be rewritten.
        for idx in 0..4 {
            assert_eq!(
                power_transition(0b1111, idx, 1),
                None,
                "node {} was reported as a transition while staying on",
                idx + 1
            );
        }
    }

    #[test]
    fn a_node_that_stays_off_keeps_its_absent_power_on_time() {
        for idx in 0..4 {
            assert_eq!(power_transition(0b0000, idx, 0), None);
        }
    }

    #[test]
    fn powering_a_node_on_and_off_is_a_transition() {
        for idx in 0..4 {
            let others = 0b1111 & !(1 << idx);
            assert_eq!(power_transition(others, idx, 1), Some(true));
            assert_eq!(power_transition(0b1111, idx, 0), Some(false));
        }
    }

    fn stamped(times: [Option<u64>; 4]) -> NodeInfos {
        let mut infos = NodeInfos::default();
        for (info, time) in infos.iter_mut().zip(times) {
            info.power_on_time = time;
        }
        infos
    }

    fn stamps(infos: &NodeInfos) -> [Option<u64>; 4] {
        std::array::from_fn(|idx| infos[idx].power_on_time)
    }

    #[test]
    fn a_cold_restore_resets_the_power_on_time_of_the_nodes_it_switches_on() {
        // The stamps are those of the last switch from the UI, long before the
        // BMC rebooted and cut the power. Nodes 1 and 3 come back on, 2 and 4
        // stay off: the first two are stamped with now, the others have none.
        let mut infos = stamped([Some(100), Some(200), Some(300), Some(400)]);
        stamp_cold_boot(&mut infos, 0b0101, Some(5000));
        assert_eq!(stamps(&infos), [Some(5000), None, Some(5000), None]);

        // Why this is not left to `activate_slot`: against the persisted state
        // the same nodes show no change, and the old stamps would stay.
        let mut infos = stamped([Some(100), None, Some(300), None]);
        stamp_transitions(&mut infos, 0b0101, 0b0101, 0b1111, Some(5000));
        assert_eq!(stamps(&infos), [Some(100), None, Some(300), None]);
    }

    #[test]
    fn a_cold_restore_with_nothing_on_clears_every_stamp() {
        let mut infos = stamped([Some(1), Some(2), Some(3), Some(4)]);
        stamp_cold_boot(&mut infos, 0b0000, Some(5000));
        assert_eq!(stamps(&infos), [None; 4]);
    }

    #[test]
    fn a_live_adopt_keeps_the_power_on_time_of_the_nodes_that_are_on() {
        // The daemon restarted under running nodes: persisted and live agree,
        // and the stamps stay.
        let mut infos = stamped([Some(100), Some(200), None, None]);
        stamp_transitions(&mut infos, 0b0011, 0b0011, 0b1111, Some(5000));
        assert_eq!(stamps(&infos), [Some(100), Some(200), None, None]);
    }

    #[test]
    fn a_live_adopt_makes_the_stamps_follow_the_live_state() {
        // Persisted: nodes 1 and 2 on. Live: nodes 2 and 3. Node 1 is off, so
        // its stamp goes; node 2 stays on, so its stamp stays; node 3 is on
        // without a stamp, so it gets one.
        let mut infos = stamped([Some(100), Some(200), None, None]);
        stamp_transitions(&mut infos, 0b0011, 0b0110, 0b1111, Some(5000));
        assert_eq!(stamps(&infos), [None, Some(200), Some(5000), None]);
    }

    #[test]
    fn powered_nodes_lists_node_numbers() {
        assert_eq!(powered_nodes(0b0000), Vec::<usize>::new());
        assert_eq!(powered_nodes(0b1010), vec![2, 4]);
        assert_eq!(powered_nodes(0b1111), vec![1, 2, 3, 4]);
    }
}

/// The power paths on the simulated board. They need the `stubbed` HAL: the
/// real one opens the board's gpio chips.
#[cfg(all(test, feature = "stubbed"))]
mod power_tests {
    use super::*;
    use tempfile::TempDir;

    /// Long enough that a test can look at a node while it is held off, short
    /// enough that the suite does not wait on it.
    const SETTLE: Duration = Duration::from_millis(400);
    /// A point well inside [`SETTLE`].
    const MID_SETTLE: Duration = Duration::from_millis(100);

    async fn board() -> (Arc<BmcApplication>, TempDir) {
        let dir = TempDir::with_prefix("bmcd-power").unwrap();
        let bmc = Arc::new(BmcApplication::for_test(dir.path(), SETTLE).await);
        (bmc, dir)
    }

    async fn reported(bmc: &BmcApplication, node: NodeId) -> bool {
        bmc.get_node_power(node).await.unwrap()
    }

    #[tokio::test]
    async fn a_reset_of_a_node_that_is_off_is_refused_and_changes_nothing() {
        let (bmc, _dir) = board().await;
        bmc.activate_slot(0b0100, 0b1111).await.unwrap();

        let err = bmc.reset_node(NodeId::Node2).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<NodeIsOff>(),
            Some(&NodeIsOff(NodeId::Node2))
        );
        assert!(!reported(&bmc, NodeId::Node2).await);
        assert_eq!(bmc.rails(), 0b0100, "the rails are where they were");
        // and the node that is on was not touched by the refusal.
        assert!(reported(&bmc, NodeId::Node3).await);
    }

    #[tokio::test]
    async fn a_reset_holds_the_node_off_and_reports_it_then_brings_it_back() {
        let (bmc, _dir) = board().await;
        bmc.activate_slot(0b1111, 0b1111).await.unwrap();

        let reset = tokio::spawn({
            let bmc = bmc.clone();
            async move { bmc.reset_node(NodeId::Node2).await }
        });
        tokio::time::sleep(MID_SETTLE).await;
        assert!(!reported(&bmc, NodeId::Node2).await, "off while held off");
        assert_eq!(bmc.rails(), 0b1101, "only node 2 is held off");

        reset.await.unwrap().unwrap();
        assert!(reported(&bmc, NodeId::Node2).await);
        assert_eq!(bmc.rails(), 0b1111);
    }

    #[tokio::test]
    async fn a_power_off_during_a_reset_wins() {
        let (bmc, _dir) = board().await;
        bmc.activate_slot(0b1111, 0b1111).await.unwrap();

        let reset = tokio::spawn({
            let bmc = bmc.clone();
            async move { bmc.reset_node(NodeId::Node2).await }
        });
        tokio::time::sleep(MID_SETTLE).await;
        // The node already reads off; the request still has to count.
        bmc.activate_slot(0b0000, 0b0010).await.unwrap();

        reset.await.unwrap().unwrap();
        assert!(!reported(&bmc, NodeId::Node2).await);
        assert_eq!(bmc.rails(), 0b1101, "the rail stays off");
        // The reset did not touch the other nodes.
        assert!(reported(&bmc, NodeId::Node1).await);
    }

    #[tokio::test]
    async fn a_power_off_of_the_whole_board_during_a_reset_wins() {
        let (bmc, _dir) = board().await;
        bmc.activate_slot(0b1111, 0b1111).await.unwrap();

        let reset = tokio::spawn({
            let bmc = bmc.clone();
            async move { bmc.reset_node(NodeId::Node4).await }
        });
        tokio::time::sleep(MID_SETTLE).await;
        bmc.toggle_power_states(false).await.unwrap();

        reset.await.unwrap().unwrap();
        assert_eq!(bmc.rails(), 0);
        assert!(!reported(&bmc, NodeId::Node4).await);
    }

    #[tokio::test]
    async fn a_power_on_during_a_reset_ends_with_the_node_on() {
        let (bmc, _dir) = board().await;
        bmc.activate_slot(0b0010, 0b1111).await.unwrap();

        let reset = tokio::spawn({
            let bmc = bmc.clone();
            async move { bmc.reset_node(NodeId::Node2).await }
        });
        tokio::time::sleep(MID_SETTLE).await;
        bmc.activate_slot(0b0010, 0b0010).await.unwrap();
        assert_eq!(
            bmc.rails(),
            0b0010,
            "switched on at once, not after the wait"
        );

        reset.await.unwrap().unwrap();
        assert!(reported(&bmc, NodeId::Node2).await);
        assert_eq!(bmc.rails(), 0b0010);
    }

    #[tokio::test]
    async fn a_request_for_another_node_does_not_cancel_a_reset() {
        let (bmc, _dir) = board().await;
        bmc.activate_slot(0b1111, 0b1111).await.unwrap();

        let reset = tokio::spawn({
            let bmc = bmc.clone();
            async move { bmc.reset_node(NodeId::Node2).await }
        });
        tokio::time::sleep(MID_SETTLE).await;
        bmc.activate_slot(0b0000, 0b0100).await.unwrap();

        reset.await.unwrap().unwrap();
        assert_eq!(bmc.rails(), 0b1011, "node 2 is back, node 3 is off");
    }

    #[tokio::test]
    async fn a_power_request_is_not_held_up_by_a_reset() {
        let (bmc, _dir) = board().await;
        bmc.activate_slot(0b1111, 0b1111).await.unwrap();

        let reset = tokio::spawn({
            let bmc = bmc.clone();
            async move { bmc.reset_node(NodeId::Node1).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Nobody holds the lock while the reset waits, so this answers well
        // before the reset's wait is over.
        let started = std::time::Instant::now();
        bmc.activate_slot(0b0000, 0b1000).await.unwrap();
        assert!(started.elapsed() < SETTLE / 2, "{:?}", started.elapsed());

        reset.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_ticket_switches_the_node_on_only_while_nothing_else_touched_it() {
        let (bmc, _dir) = board().await;
        bmc.activate_slot(0b1111, 0b1111).await.unwrap();

        let ticket = bmc.power_off_for_cycle(NodeId::Node3).await.unwrap();
        assert_eq!(bmc.rails(), 0b1011);
        assert!(bmc.power_on_unless_changed(ticket).await.unwrap());
        assert_eq!(bmc.rails(), 0b1111);

        let ticket = bmc.power_off_for_cycle(NodeId::Node3).await.unwrap();
        bmc.activate_slot(0b0000, 0b0100).await.unwrap();
        assert!(!bmc.power_on_unless_changed(ticket).await.unwrap());
        assert_eq!(bmc.rails(), 0b1011);
    }
}
