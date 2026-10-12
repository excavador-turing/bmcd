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
use super::{
    helpers::{bit_iterator, load_lines, LegacyNodeReading},
    NodeId,
};
use anyhow::Context;
use gpiod::{Chip, Lines, Options, Output};
use std::path::PathBuf;
use std::{str::FromStr, time::Duration};
use tokio::time::sleep;
use tracing::{debug, trace};

const SYS_LED: &str = "/sys/class/leds/fp::power/brightness";
const SYS_LED_2_0_5: &str = "/sys/class/leds/fp:sys/brightness";
const STATUS_LED: &str = "/sys/class/leds/fp::status/brightness";
const STATUS_LED_2_0_5: &str = "/sys/class/leds/fp:reset/brightness";
const PORT1_EN: &str = "node1-en";
const PORT2_EN: &str = "node2-en";
const PORT3_EN: &str = "node3-en";
const PORT4_EN: &str = "node4-en";
const CONSUMER: &str = "bmcd-node-en";

// This structure is a thin layer that abstracts away the interaction details
// with Linux's power subsystem.
pub struct PowerController {
    enable: [Lines<Output>; 4],
    sysfs_power: PathBuf,
    sysfs_reset: PathBuf,
    /// What was found on the node enable lines and the regulator-output
    /// devices before this daemon touched them. Only taken on a v2.4 board.
    legacy_readings: Option<[LegacyNodeReading; 4]>,
}

/// What [`claim_enable_lines`] needs from a gpio chip. A trait so the order
/// of operations (read first, drive second, never a different value than was
/// read) can be tested without hardware.
trait EnableChip {
    type Out;

    /// Request the line as an input and read its level. The line is released
    /// again when this returns.
    fn read_input(&self, line: gpiod::LineId) -> anyhow::Result<bool>;

    /// Request the line as an output initialised to `level`.
    fn claim_output(&self, line: gpiod::LineId, level: bool) -> anyhow::Result<Self::Out>;
}

impl EnableChip for Chip {
    type Out = Lines<Output>;

    fn read_input(&self, line: gpiod::LineId) -> anyhow::Result<bool> {
        let input = self
            .request_lines(Options::input([line]).consumer(CONSUMER))
            .context("request as input")?;
        let level = input.get_values(0u8).context("read level")?;
        Ok(level & 1 == 1)
    }

    fn claim_output(&self, line: gpiod::LineId, level: bool) -> anyhow::Result<Self::Out> {
        Ok(self.request_lines(
            Options::output([line])
                .values([level])
                .consumer(CONSUMER),
        )?)
    }
}

/// Output handles plus the per-line read results, see [`claim_enable_lines`].
type Claimed<O> = ([O; 4], [anyhow::Result<bool>; 4]);

/// Reads the level of the four enable lines as inputs, then claims them as
/// outputs initialised to exactly the level that was read, so claiming them
/// does not change what the pin carries. libgpiod 1 (what the `gpiod` crate
/// speaks) cannot reconfigure a requested line, so this is release and
/// re-request; the pin is an input in between.
///
/// A line that could not be read is claimed at level 0, which is what the
/// daemon has always done, and the failure is returned so the startup logic
/// does not trust it.
fn claim_enable_lines<C: EnableChip>(
    chip: &C,
    lines: [gpiod::LineId; 4],
) -> anyhow::Result<Claimed<C::Out>> {
    let levels = lines.map(|line| chip.read_input(line));
    let mut outs = Vec::with_capacity(4);
    for (idx, line) in lines.into_iter().enumerate() {
        let level = matches!(levels[idx], Ok(true));
        outs.push(
            chip.claim_output(line, level)
                .with_context(|| format!("error initializing enable line of node {}", idx + 1))?,
        );
    }
    let outs: [C::Out; 4] = outs.try_into().map_err(|_| anyhow::anyhow!("4 lines"))?;
    Ok((outs, levels))
}

/// Reads `/sys/bus/platform/devices/nodeN-power/state`.
///
/// This is the `regulator-output` (userspace-consumer) state. It is a
/// software view: the driver takes it from the kernel regulator's cached
/// state at probe, not from the pin.
fn read_regulator_state(node_id: usize) -> anyhow::Result<bool> {
    let path = format!("/sys/bus/platform/devices/node{}-power/state", node_id);
    let state = std::fs::read_to_string(&path).context(path)?;
    match state.trim() {
        "enabled" => Ok(true),
        "disabled" => Ok(false),
        other => Err(anyhow::anyhow!("unexpected regulator state {:?}", other)),
    }
}

impl PowerController {
    /// # Arguments
    ///
    /// * `is_legacy_dts`   true on a Turing Pi 2 v2.4 board, which has no
    ///   latch ICs: its node rails are SoC pins and its enable lines live on
    ///   `/dev/gpiochip1`. (This argument used to be called
    ///   `is_latching_system`, which is the opposite of what it means.)
    pub fn new(is_legacy_dts: bool) -> anyhow::Result<Self> {
        let chip1 = if is_legacy_dts {
            "/dev/gpiochip1"
        } else {
            "/dev/gpiochip2"
        };

        let chip1 = Chip::new(chip1).context(chip1)?;
        let lines = load_lines(&chip1);
        let port1 = *lines
            .get(PORT1_EN)
            .ok_or(anyhow::anyhow!("cannot find PORT1_EN"))?;
        let port2 = *lines
            .get(PORT2_EN)
            .ok_or(anyhow::anyhow!("cannot find PORT2_EN"))?;
        let port3 = *lines
            .get(PORT3_EN)
            .ok_or(anyhow::anyhow!("cannot find PORT3_EN"))?;
        let port4 = *lines
            .get(PORT4_EN)
            .ok_or(anyhow::anyhow!("cannot find PORT4_EN"))?;

        let (enable, legacy_readings) = if is_legacy_dts {
            // No latch keeps the state for us: requesting the lines as
            // outputs would drive them, and the read after that would only
            // return what was just driven. Read first, drive the same value.
            let (enable, levels) = claim_enable_lines(&chip1, [port1, port2, port3, port4])?;
            let [l1, l2, l3, l4] = levels;
            let readings = [l1, l2, l3, l4]
                .into_iter()
                .enumerate()
                .map(|(idx, enable)| LegacyNodeReading {
                    regulator: read_regulator_state(idx + 1).map_err(|e| format!("{:#}", e)),
                    enable: enable.map_err(|e| format!("{:#}", e)),
                })
                .collect::<Vec<_>>();
            (enable, readings.try_into().ok())
        } else {
            (
                crate::gpio_output_array!(chip1, port1, port2, port3, port4),
                None,
            )
        };

        let sysfs_power = fallback_if_not_exist(SYS_LED, SYS_LED_2_0_5);
        let sysfs_reset = fallback_if_not_exist(STATUS_LED, STATUS_LED_2_0_5);

        Ok(PowerController {
            enable,
            sysfs_power,
            sysfs_reset,
            legacy_readings,
        })
    }

    /// The readings taken on a v2.4 board before the daemon drove anything.
    /// `None` on a latching (v2.5) board.
    pub fn legacy_startup_readings(&self) -> Option<&[LegacyNodeReading; 4]> {
        self.legacy_readings.as_ref()
    }

    /// Function to power on/off given nodes. Powering of the nodes is controlled by
    /// the Linux subsystem.
    ///
    /// # Arguments
    ///
    /// * `node_states`     bit-field representing the nodes on the turing-pi board,
    ///   where bit 1 is on and 0 equals off.
    /// * `node_mask`       bit-field to describe which nodes to control.
    ///
    /// # Returns
    ///
    /// * `Ok(())` when routine was executed successfully.
    /// * `Err(io error)` in the case there was a failure to write to the Linux
    ///   subsystem that handles the node powering.
    pub async fn set_power_node(&self, node_states: u8, node_mask: u8) -> anyhow::Result<()> {
        let updates = bit_iterator(node_states, node_mask);

        for (idx, state) in updates {
            trace!("setting power of node {}. state:{}", idx + 1, state);
            set_mode(idx + 1, state).await?;
            sleep(Duration::from_millis(100)).await;
            self.enable[idx].set_values(state)?;
        }

        Ok(())
    }

    /// Read back the state of the node enable lines.
    ///
    /// # Returns
    ///
    /// * `Ok(bit-field)` where bit(n) = 1 means node n is enabled. On a
    ///   latching board the kernel keeps these latches across a reboot of
    ///   the BMC, so the value reflects the nodes that are actually powered,
    ///   not what this daemon last requested.
    /// * `Err(gpio error)` when a line could not be read.
    pub fn get_power_node(&self) -> anyhow::Result<u8> {
        let mut node_states = 0u8;
        for (idx, line) in self.enable.iter().enumerate() {
            let state = line
                .get_values(0u8)
                .with_context(|| format!("error reading enable line of node {}", idx + 1))?;
            node_states |= (state & 1) << idx;
        }

        Ok(node_states)
    }

    /// Reset a given node by setting the reset pin logically high for 1 second
    pub async fn reset_node(&self, node: NodeId) -> anyhow::Result<()> {
        debug!("reset node {:?}", node);
        let bits = node.to_bitfield();

        self.set_power_node(0u8, bits).await?;
        sleep(Duration::from_secs(1)).await;
        self.set_power_node(bits, bits).await?;
        Ok(())
    }

    pub async fn power_led(&self, on: bool) -> anyhow::Result<()> {
        tokio::fs::write(&self.sysfs_power, if on { "1" } else { "0" })
            .await
            .context(SYS_LED)
    }

    pub async fn status_led(&self, on: bool) -> anyhow::Result<()> {
        tokio::fs::write(&self.sysfs_reset, if on { "1" } else { "0" })
            .await
            .context(STATUS_LED)
    }
}

async fn set_mode(node_id: usize, node_state: u8) -> std::io::Result<()> {
    let node_value = if node_state > 0 {
        "enabled"
    } else {
        "disabled"
    };

    let sys_path = format!("/sys/bus/platform/devices/node{}-power/state", node_id);
    tokio::fs::write(sys_path, node_value).await
}

fn fallback_if_not_exist(sysfs: &str, fallback: &str) -> PathBuf {
    let mut sysfs = PathBuf::from_str(sysfs).expect("valid utf8 path");
    if !sysfs.exists() {
        sysfs = PathBuf::from_str(fallback).expect("valid utf8 path");
        tracing::info!("power led: falling back to {}", fallback);
    }
    sysfs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct MockChip {
        /// Physical level per line id; a missing entry fails the read.
        levels: Vec<(gpiod::LineId, bool)>,
        /// Chronological record: ("in", line, _) and ("out", line, level).
        calls: RefCell<Vec<(&'static str, gpiod::LineId, bool)>>,
    }

    impl EnableChip for MockChip {
        type Out = (gpiod::LineId, bool);

        fn read_input(&self, line: gpiod::LineId) -> anyhow::Result<bool> {
            let level = self.levels.iter().find(|(l, _)| *l == line).map(|(_, v)| *v);
            self.calls.borrow_mut().push(("in", line, level.unwrap_or(false)));
            level.ok_or_else(|| anyhow::anyhow!("read failed"))
        }

        fn claim_output(&self, line: gpiod::LineId, level: bool) -> anyhow::Result<Self::Out> {
            self.calls.borrow_mut().push(("out", line, level));
            Ok((line, level))
        }
    }

    #[test]
    fn lines_are_driven_to_the_level_that_was_read() {
        let chip = MockChip {
            levels: vec![(10, true), (11, false), (12, true), (13, true)],
            ..Default::default()
        };
        let (outs, levels) = claim_enable_lines(&chip, [10, 11, 12, 13]).unwrap();
        assert_eq!(outs, [(10, true), (11, false), (12, true), (13, true)]);
        assert!(levels.iter().all(|l| l.is_ok()));

        let calls = chip.calls.borrow();
        // Every read comes before any line is driven.
        let first_out = calls.iter().position(|c| c.0 == "out").unwrap();
        assert!(calls[..first_out].iter().all(|c| c.0 == "in"));
        assert_eq!(first_out, 4);
        // No output was requested with a level other than the one read.
        for out in calls.iter().filter(|c| c.0 == "out") {
            let read = calls.iter().find(|c| c.0 == "in" && c.1 == out.1).unwrap();
            assert_eq!(out.2, read.2, "line {} driven differently than read", out.1);
        }
    }

    #[test]
    fn a_failed_read_is_reported_and_driven_low_as_before() {
        let chip = MockChip {
            levels: vec![(10, true), (12, true), (13, true)],
            ..Default::default()
        };
        let (outs, levels) = claim_enable_lines(&chip, [10, 11, 12, 13]).unwrap();
        assert_eq!(outs[1], (11, false));
        assert!(levels[1].is_err());
        assert!(levels[0].is_ok() && levels[2].is_ok() && levels[3].is_ok());
    }
}
