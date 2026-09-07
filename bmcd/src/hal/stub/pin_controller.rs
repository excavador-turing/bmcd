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
use crate::hal::NodeId;
use crate::hal::PowerControllerError;
use crate::hal::UsbArchitecture;
use crate::hal::UsbMode;
use crate::hal::UsbRoute;
use std::sync::atomic::{AtomicU8, Ordering};
use tracing::debug;

/// Simulated counterpart of [`crate::hal::PinController`].
///
/// What is simulated, and how faithfully:
///
/// * `usb_bus_type()` is decided by the `has_usb_switch` argument to
///   [`PinController::new`], the same argument the real controller uses to
///   pick a chip and a switch implementation. It is not a guess about the
///   board: with no device tree to read, `BmcApplication` passes `false`, so a
///   stubbed run presents itself as a v2.5+ [`UsbArchitecture::UsbHub`].
/// * `set_usb_boot()` keeps the rpiboot bit-field it was handed, so a test can
///   read back what it asked for. There are no GPIO lines behind it.
/// * `select_usb()` and `set_usb_route()` are recorded in the log and
///   otherwise do nothing at all. Nothing electrical happens, and no state is
///   invented to make it look as though it had.
///
/// `set_node1_usb_route()` returns the same error the real v2.4 multiplexer
/// returns when the board cannot do it, rather than silently succeeding.
pub struct PinController {
    architecture: UsbArchitecture,
    /// Simulated rpiboot lines: bit(n) is node n+1.
    usb_boot: AtomicU8,
}

impl PinController {
    /// create a new Pin controller
    pub fn new(has_usb_switch: bool) -> anyhow::Result<Self> {
        let architecture = if has_usb_switch {
            UsbArchitecture::UsbMux
        } else {
            UsbArchitecture::UsbHub
        };

        debug!("stub: pin controller simulating a {} board", architecture);
        Ok(Self {
            architecture,
            usb_boot: AtomicU8::new(0),
        })
    }

    pub fn select_usb(&self, node: NodeId, mode: UsbMode) -> Result<(), PowerControllerError> {
        debug!("stub: select USB for node {:?}, mode:{:?}", node, mode);

        if self.architecture == UsbArchitecture::UsbHub && mode == UsbMode::Host {
            return Err(PowerControllerError::HostModeNotSupported);
        }

        if UsbMode::Flash == mode {
            self.set_usb_boot(node.to_bitfield(), node.to_bitfield())
        } else {
            self.set_usb_boot(0, 0b1111)
        }
    }

    pub fn set_usb_route(&self, route: UsbRoute) -> Result<(), PowerControllerError> {
        debug!("stub: select USB route {:?}", route);
        Ok(())
    }

    pub fn set_usb_boot(
        &self,
        nodes_state: u8,
        nodes_mask: u8,
    ) -> Result<(), PowerControllerError> {
        let updates = bit_iterator(nodes_state, nodes_mask);

        for (idx, state) in updates {
            debug!(
                "stub: updating usb_boot state of node {} to {}",
                idx + 1,
                if state != 0 { "enable" } else { "disable" }
            );
            let mask = 1 << idx;
            let value = state << idx;
            self.usb_boot
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    Some((current & !mask) | value)
                })
                .expect("closure never returns None");
        }
        Ok(())
    }

    /// Read back the simulated rpiboot lines. Has no counterpart on the real
    /// controller; it exists so a test can assert on what `set_usb_boot` and
    /// `select_usb` did, and is compiled only into the test binary so that it
    /// cannot become a back door in the daemon itself.
    #[cfg(test)]
    pub fn simulated_usb_boot(&self) -> u8 {
        self.usb_boot.load(Ordering::SeqCst)
    }

    pub fn set_node1_usb_route(&self, alternative_port: bool) -> Result<(), PowerControllerError> {
        if self.architecture == UsbArchitecture::UsbMux {
            return Err(PowerControllerError::Node1UsbNotApplicable);
        }

        debug!("stub: setting alternative port for Node 1 USB: {alternative_port}");
        Ok(())
    }

    pub fn usb_bus_type(&self) -> UsbArchitecture {
        self.architecture
    }
}

impl std::fmt::Debug for PinController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PinController(stub, {})", self.architecture)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn architecture_follows_the_constructor_argument() {
        assert_eq!(
            PinController::new(true).unwrap().usb_bus_type(),
            UsbArchitecture::UsbMux
        );
        assert_eq!(
            PinController::new(false).unwrap().usb_bus_type(),
            UsbArchitecture::UsbHub
        );
    }

    #[test]
    fn node1_usb_route_is_rejected_on_a_multiplexer_board() {
        let mux = PinController::new(true).unwrap();
        assert!(matches!(
            mux.set_node1_usb_route(true),
            Err(PowerControllerError::Node1UsbNotApplicable)
        ));

        let hub = PinController::new(false).unwrap();
        assert!(hub.set_node1_usb_route(true).is_ok());
    }

    #[test]
    fn flash_mode_arms_usb_boot_for_that_node_only() {
        let pins = PinController::new(true).unwrap();

        pins.select_usb(NodeId::Node3, UsbMode::Flash).unwrap();
        assert_eq!(pins.simulated_usb_boot(), 0b0100);

        pins.select_usb(NodeId::Node3, UsbMode::Device).unwrap();
        assert_eq!(pins.simulated_usb_boot(), 0b0000);
    }

    #[test]
    fn a_hub_board_cannot_put_a_node_in_host_mode() {
        let hub = PinController::new(false).unwrap();
        assert!(matches!(
            hub.select_usb(NodeId::Node1, UsbMode::Host),
            Err(PowerControllerError::HostModeNotSupported)
        ));
    }
}
