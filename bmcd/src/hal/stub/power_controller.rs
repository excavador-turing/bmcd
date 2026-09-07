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
use crate::hal::{helpers::bit_iterator, NodeId};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;
use tokio::time::sleep;
use tracing::debug;

/// Simulated counterpart of [`crate::hal::PowerController`].
///
/// What is simulated, and how faithfully:
///
/// * The four node enable lines are a single `AtomicU8`. `set_power_node()`
///   writes into it and `get_power_node()` reads it back, so the daemon sees
///   exactly what it asked for and nothing more. On a real latching board
///   those lines survive a restart of the daemon; here they do not, so a
///   stubbed process always starts from all-off -- the cold-boot branch of
///   `BmcApplication::initialize_power`. That is a real difference from the
///   hardware, not an oversight.
/// * `power_led()` and `status_led()` record the requested state and are
///   readable back through [`PowerController::simulated_leds`]. Nothing is
///   written to `/sys/class/leds`.
/// * `reset_node()` goes through the same off/on sequence as the real
///   controller, including the one-second wait, so a caller that races it
///   races it the same way here.
///
/// There is no fabricated telemetry anywhere in this file: every value a
/// caller can read back is one this process was told to set.
pub struct PowerController {
    /// Simulated node enable lines: bit(n) is node n+1.
    enable: AtomicU8,
    power_led: AtomicBool,
    status_led: AtomicBool,
}

impl PowerController {
    pub fn new(is_latching_system: bool) -> anyhow::Result<Self> {
        debug!(
            "stub: power controller, latching system: {is_latching_system}. \
             All node state is in memory and is lost when this process exits."
        );
        Ok(PowerController {
            enable: AtomicU8::new(0),
            power_led: AtomicBool::new(false),
            status_led: AtomicBool::new(false),
        })
    }

    /// See [`crate::hal::PowerController::set_power_node`]. Here it only
    /// updates the simulated enable lines.
    pub async fn set_power_node(&self, node_states: u8, node_mask: u8) -> anyhow::Result<()> {
        let updates = bit_iterator(node_states, node_mask);

        for (idx, state) in updates {
            debug!("stub: setting power of node {}. state:{}", idx + 1, state);
            let mask = 1 << idx;
            let value = state << idx;
            self.enable
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    Some((current & !mask) | value)
                })
                .expect("closure never returns None");
        }

        Ok(())
    }

    /// Read back the simulated enable lines. The real implementation reads the
    /// GPIO lines, which on a latching board outlive the daemon; this one can
    /// only report what this process has set since it started.
    pub fn get_power_node(&self) -> anyhow::Result<u8> {
        Ok(self.enable.load(Ordering::SeqCst))
    }

    /// Reset a given node by setting the reset pin logically high for 1 second
    pub async fn reset_node(&self, node: NodeId) -> anyhow::Result<()> {
        debug!("stub: reset node {:?}", node);
        let bits = node.to_bitfield();

        self.set_power_node(0u8, bits).await?;
        sleep(Duration::from_secs(1)).await;
        self.set_power_node(bits, bits).await?;
        Ok(())
    }

    pub async fn power_led(&self, on: bool) -> anyhow::Result<()> {
        debug!("stub: power led {}", if on { "on" } else { "off" });
        self.power_led.store(on, Ordering::SeqCst);
        Ok(())
    }

    pub async fn status_led(&self, on: bool) -> anyhow::Result<()> {
        debug!("stub: status led {}", if on { "on" } else { "off" });
        self.status_led.store(on, Ordering::SeqCst);
        Ok(())
    }

    /// `(power, status)` as last requested. Has no counterpart on the real
    /// controller, which cannot read the LEDs back either. Test-only, for the
    /// same reason as `PinController::simulated_usb_boot`.
    #[cfg(test)]
    pub fn simulated_leds(&self) -> (bool, bool) {
        (
            self.power_led.load(Ordering::SeqCst),
            self.status_led.load(Ordering::SeqCst),
        )
    }
}

impl std::fmt::Debug for PowerController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PowerController(stub, enable={:#06b})",
            self.enable.load(Ordering::SeqCst)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enable_lines_read_back_what_was_written() {
        let power = PowerController::new(true).unwrap();
        assert_eq!(power.get_power_node().unwrap(), 0);

        power.set_power_node(0b1111, 0b1111).await.unwrap();
        assert_eq!(power.get_power_node().unwrap(), 0b1111);

        // A masked write must leave the nodes outside the mask alone.
        power.set_power_node(0b0000, 0b0010).await.unwrap();
        assert_eq!(power.get_power_node().unwrap(), 0b1101);
    }

    #[tokio::test]
    async fn leds_record_the_last_request() {
        let power = PowerController::new(false).unwrap();
        assert_eq!(power.simulated_leds(), (false, false));

        power.power_led(true).await.unwrap();
        power.status_led(true).await.unwrap();
        assert_eq!(power.simulated_leds(), (true, true));

        power.power_led(false).await.unwrap();
        assert_eq!(power.simulated_leds(), (false, true));
    }
}
