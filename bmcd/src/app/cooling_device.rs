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

use anyhow::anyhow;

/// What a caller got wrong, as opposed to what the board could not do.
///
/// The distinction reaches the wire: these become 400, and everything else
/// stays 500. It matters more than it looks, because `problem+json` exists so
/// a generated client can branch on `status` -- and 500 is the canonical
/// retryable one, so a client told 500 for "you named a device that does not
/// exist" will retry forever against an answer that cannot change.
///
/// A typed error rather than matching on the message: the text is for a
/// person, and a status derived from it would break the first time somebody
/// improved the wording.
#[derive(Debug, thiserror::Error)]
pub enum CoolingRequestError {
    #[error("cooling device: `{0}` does not exist")]
    NoSuchDevice(String),
    #[error("given speed '{given}' exceeds maximum speed of '{max}'")]
    SpeedTooHigh { given: c_ulong, max: c_ulong },
    #[error(
        "cooling device `{0}` is not bound to a thermal zone, so there is no governor to pause"
    )]
    NoThermalZone(String),
}
use schemars::JsonSchema;
use serde::Serialize;
use std::{collections::HashMap, ffi::c_ulong, fs, io, path::Path, time::Duration};
use tracing::{info, instrument, warn};

/// Where the kernel exposes both thermal zones and cooling devices.
const THERMAL_CLASS: &str = "/sys/class/thermal";

/// How often a paused governor is checked against the board's temperature.
///
/// Short enough that a fan left low cannot take the board far past the trip
/// it should have reacted to, long enough that it is four sysfs reads a
/// minute on a board with 116 MB of RAM.
const CEILING_POLL: Duration = Duration::from_secs(15);

#[derive(Debug, Serialize, JsonSchema)]
pub struct CoolingDevice {
    pub device: String,
    pub speed: c_ulong,
    pub max_speed: c_ulong,
    /// The thermal zone whose governor drives this device, when one binds it.
    ///
    /// A zone binds a cooling device once per trip it should react to, so
    /// this board's single fan is `cdev0` through `cdev3` of the one zone.
    /// Every one of those links is the same answer.
    pub zone: Option<String>,
    /// Whether that zone's governor is paused, which is the only condition
    /// under which a step written here is a step that holds.
    ///
    /// With the governor running, `step_wise` returns the fan to the trip the
    /// board is above within a poll, so a written step is a request the
    /// kernel is about to overrule. Reported so a client can say which of
    /// those two things is happening, rather than leaving a person to watch a
    /// slider spring back and draw their own conclusion.
    pub overridden: bool,
}

pub async fn get_cooling_state() -> Vec<CoolingDevice> {
    let mut result = Vec::new();

    let bindings = zone_bindings().await;

    if let Ok(mut dir) = tokio::fs::read_dir(THERMAL_CLASS).await {
        while let Some(device) = dir.next_entry().await.unwrap_or(None) {
            let mut device_name = device.file_name().to_string_lossy().into_owned();
            if !device_name.starts_with("cooling_device") {
                continue;
            }

            // The kernel's own directory name, taken before the rename
            // below: a zone's `cdevN` links point at `cooling_deviceN`, and
            // "system fan" would not match any of them.
            let dir_name = device_name.clone();

            let device_path = device.path();
            if let Some(name) = is_system_fan(&device_path).map(|n| n.replace('-', " ")) {
                device_name = name;
            }

            let cur_state_path = device_path.join("cur_state");
            let max_state_path = device_path.join("max_state");

            let cur_state = match tokio::fs::read_to_string(cur_state_path).await {
                Ok(state) => state.trim().parse::<c_ulong>().unwrap_or(0),
                Err(err) => {
                    eprintln!("Error reading cur_state file: {}", err);
                    0
                }
            };

            let max_state = match tokio::fs::read_to_string(max_state_path).await {
                Ok(max_speed) => max_speed.trim().parse::<c_ulong>().unwrap_or(0),
                Err(err) => {
                    eprintln!("Error reading max_state file: {}", err);
                    0
                }
            };

            let zone = bindings.get(&dir_name).cloned();
            // Absent a zone, or absent a readable `mode`, the honest answer
            // is that nothing is being held: `overridden` is a claim that the
            // governor is off, and it is only made where that was read.
            let overridden = match zone.as_deref() {
                Some(zone) => governor_enabled(zone).await == Some(false),
                None => false,
            };

            result.push(CoolingDevice {
                device: device_name,
                speed: cur_state,
                max_speed: max_state,
                zone,
                overridden,
            });
        }
    }

    result
}

#[instrument(level = "debug", ret)]
fn is_system_fan(dev_path: &Path) -> Option<String> {
    let typ = std::fs::read_to_string(dev_path.join("type")).ok()?;
    if typ.trim() == "pwm-fan" {
        let pwm_fan_nodes = get_pwm_fan_nodes().ok()?;
        if pwm_fan_nodes.len() > 1 {
            warn!("more as one pwm-fan device detected, selecting first for system_fan");
        }
        return pwm_fan_nodes.first().cloned();
    }
    None
}

#[instrument(level = "debug", ret)]
fn get_pwm_fan_nodes() -> io::Result<Vec<String>> {
    let mut nodes = Vec::new();

    for entry in fs::read_dir("/sys/bus/platform/drivers/pwm-fan")? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                nodes.push(name.to_string());
            }
        }
    }

    Ok(nodes)
}

#[instrument(err)]
pub async fn set_cooling_state(device: &str, speed: &c_ulong) -> anyhow::Result<()> {
    // quick and dirty workaround
    let dev_name = if device == "system fan" {
        "cooling_device0"
    } else {
        device
    };

    let device_path = Path::new("/sys/class/thermal")
        .join(dev_name)
        .join("cur_state");

    let devices = get_cooling_state().await;
    let found = devices
        .iter()
        .find(|d| d.device == device)
        .ok_or_else(|| CoolingRequestError::NoSuchDevice(device.to_string()))?;

    if speed > &found.max_speed {
        return Err(CoolingRequestError::SpeedTooHigh {
            given: *speed,
            max: found.max_speed,
        }
        .into());
    }

    tokio::fs::write(device_path, speed.to_string()).await?;
    Ok(())
}

/// Which zone binds each cooling device, keyed by the device's directory name.
///
/// The kernel records this on the zone's side only: a `cdevN` symlink per
/// bound trip, pointing back at the cooling device, and nothing pointing the
/// other way. So the class is walked once rather than assuming
/// `thermal_zone0` drives `cooling_device0` -- which is true on this board
/// and is not a property of the interface.
async fn zone_bindings() -> HashMap<String, String> {
    let mut bindings = HashMap::new();

    let Ok(mut dir) = tokio::fs::read_dir(THERMAL_CLASS).await else {
        return bindings;
    };

    while let Some(zone_entry) = dir.next_entry().await.unwrap_or(None) {
        let zone = zone_entry.file_name().to_string_lossy().into_owned();
        if !zone.starts_with("thermal_zone") {
            continue;
        }

        let Ok(mut links) = tokio::fs::read_dir(zone_entry.path()).await else {
            continue;
        };

        while let Some(link) = links.next_entry().await.unwrap_or(None) {
            let name = link.file_name().to_string_lossy().into_owned();
            // `cdev0` is the binding. `cdev0_trip_point` and `cdev0_weight`
            // are attributes describing it and are not links to anything.
            if !name.starts_with("cdev") || name.contains('_') {
                continue;
            }
            if let Ok(target) = tokio::fs::read_link(link.path()).await {
                if let Some(device) = target.file_name() {
                    bindings
                        .entry(device.to_string_lossy().into_owned())
                        .or_insert_with(|| zone.clone());
                }
            }
        }
    }

    bindings
}

/// Whether a zone's governor is running.
///
/// `None` when the zone has no readable `mode`, which is a kernel built
/// without writable zone modes and not a governor that is off. The
/// difference matters: it separates "the fan cannot be held" from "the fan
/// is being held right now".
async fn governor_enabled(zone: &str) -> Option<bool> {
    let mode = tokio::fs::read_to_string(Path::new(THERMAL_CLASS).join(zone).join("mode"))
        .await
        .ok()?;

    match mode.trim() {
        "enabled" => Some(true),
        "disabled" => Some(false),
        _ => None,
    }
}

/// Pause or resume a zone's governor.
#[instrument(err)]
async fn set_governor(zone: &str, enabled: bool) -> anyhow::Result<()> {
    let path = Path::new(THERMAL_CLASS).join(zone).join("mode");
    let value = if enabled { "enabled" } else { "disabled" };

    tokio::fs::write(&path, value)
        .await
        .map_err(|e| anyhow!("cannot write `{}` to {}: {}", value, path.display(), e))
}

/// Hold a fan at one step, or hand it back to the governor.
///
/// `Some(step)` pauses the zone's governor and writes the step; `None`
/// resumes the governor and deliberately leaves the step alone, because the
/// governor is about to choose one and writing first would only put a
/// different number on screen for a single poll.
///
/// The governor is paused *before* the step is written, never after. Between
/// those two writes the kernel is still regulating, and a `step_wise` poll is
/// short enough to land in that gap -- which would leave the governor paused
/// on a step nobody asked for. If the step then fails to write, the governor
/// is handed back rather than left off, since a paused governor with no step
/// set is the worst of both.
#[instrument(err)]
pub async fn set_cooling_override(device: &str, step: Option<c_ulong>) -> anyhow::Result<()> {
    let devices = get_cooling_state().await;
    let found = devices
        .iter()
        .find(|d| d.device == device)
        .ok_or_else(|| CoolingRequestError::NoSuchDevice(device.to_string()))?;

    let zone = found
        .zone
        .clone()
        .ok_or_else(|| CoolingRequestError::NoThermalZone(device.to_string()))?;

    let Some(step) = step else {
        set_governor(&zone, true).await?;
        info!("`{}` returned to the `{}` governor", device, zone);
        return Ok(());
    };

    // Validated before anything is paused: a step this board cannot reach
    // must not cost it its governor.
    if step > found.max_speed {
        return Err(CoolingRequestError::SpeedTooHigh {
            given: step,
            max: found.max_speed,
        }
        .into());
    }

    set_governor(&zone, false).await?;

    if let Err(e) = set_cooling_state(device, &step).await {
        let _ = set_governor(&zone, true).await;
        return Err(e);
    }

    warn!(
        "`{}` held at step {}; the `{}` governor is paused until this is cleared",
        device, step, zone
    );
    Ok(())
}

/// Resume every paused governor.
///
/// Called once at startup. An override lives in the kernel and not in this
/// process, so a daemon killed while holding the fan leaves the board
/// unregulated with nothing tracking it. The daemon coming back up is the
/// thing best placed to notice.
pub async fn resume_all_governors() {
    for device in get_cooling_state().await {
        let (Some(zone), true) = (device.zone.as_deref(), device.overridden) else {
            continue;
        };
        match set_governor(zone, true).await {
            Ok(()) => warn!(
                "`{}` was left held by an earlier run; returned to the `{}` governor",
                device.device, zone
            ),
            Err(e) => warn!(
                "cannot return `{}` to the `{}` governor: {}",
                device.device, zone, e
            ),
        }
    }
}

/// The hottest `active` trip a zone declares, in millidegrees.
///
/// `active` is the kind that drives a cooling device, so the hottest one is
/// the temperature above which the governor would already be asking for the
/// top step. `None` when the zone declares none, in which case there is no
/// temperature this module can argue is too hot and the ceiling does nothing.
async fn hottest_active_trip(zone: &str) -> Option<i64> {
    let dir = Path::new(THERMAL_CLASS).join(zone);
    let mut entries = tokio::fs::read_dir(&dir).await.ok()?;
    let mut hottest = None;

    while let Some(entry) = entries.next_entry().await.unwrap_or(None) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix("_type") else {
            continue;
        };
        if !stem.starts_with("trip_point_") {
            continue;
        }
        let kind = tokio::fs::read_to_string(entry.path()).await.ok();
        if kind.as_deref().map(str::trim) != Some("active") {
            continue;
        }
        let temp = tokio::fs::read_to_string(dir.join(format!("{}_temp", stem)))
            .await
            .ok()
            .and_then(|t| t.trim().parse::<i64>().ok());
        if let Some(temp) = temp {
            hottest = Some(hottest.map_or(temp, |h: i64| h.max(temp)));
        }
    }

    hottest
}

/// Take the fan back from an override the board has grown too hot to afford.
///
/// This board declares no `critical` trip -- the hottest thing in its device
/// tree is `hot` at 95 °C, which notifies and does not act -- so nothing else
/// will intervene if a fan is left on a low step and the board climbs. A
/// person who paused the governor and walked away is not expressing a
/// preference about 80 °C.
///
/// The ceiling is the zone's own hottest `active` trip rather than a constant,
/// for the same reason the rest of this daemon reads the trips instead of
/// carrying a table: it is a fact about this board.
pub async fn enforce_override_ceiling() {
    for device in get_cooling_state().await {
        let (Some(zone), true) = (device.zone.as_deref(), device.overridden) else {
            continue;
        };

        let Some(ceiling) = hottest_active_trip(zone).await else {
            continue;
        };

        let temperature =
            tokio::fs::read_to_string(Path::new(THERMAL_CLASS).join(zone).join("temp"))
                .await
                .ok()
                .and_then(|t| t.trim().parse::<i64>().ok());

        let Some(temperature) = temperature else {
            continue;
        };

        if temperature <= ceiling {
            continue;
        }

        match set_governor(zone, true).await {
            Ok(()) => warn!(
                "`{}` is at {} m°C, above its hottest active trip of {} m°C: \
                 the hold on `{}` was released and the governor has it back",
                zone, temperature, ceiling, device.device
            ),
            Err(e) => warn!(
                "`{}` is at {} m°C and the hold on `{}` could not be released: {}",
                zone, temperature, device.device, e
            ),
        }
    }
}

/// Run [`enforce_override_ceiling`] for as long as the daemon lives.
pub fn spawn_override_ceiling() {
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(CEILING_POLL);
        loop {
            ticker.tick().await;
            enforce_override_ceiling().await;
        }
    });
}
