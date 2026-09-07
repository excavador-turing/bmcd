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
//! Temperature and cooling state, read from `/sys/class/thermal`.
//!
//! The kernel's thermal framework puts two kinds of directory in this one
//! class: a `thermal_zoneN` per sensor, and a `cooling_deviceN` per thing that
//! can be turned up to cool one. Everything here is what the kernel already
//! knows; nothing in this module talks to a sensor or to the fan itself, and
//! nothing in it writes.
use crate::app::sysfs::{read_attribute, read_attribute_string};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Where the kernel exposes both thermal zones and cooling devices.
const THERMAL_CLASS: &str = "/sys/class/thermal";

/// One thermal zone: a sensor the kernel can read a temperature from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ThermalSensor {
    /// The zone's `type`, which is the name its driver registered -- ours is
    /// `bmc-thermal`, from the device tree node. Falls back to the directory
    /// name if the kernel will not answer, so a sensor is never nameless.
    pub name: String,
    /// Degrees Celsius to one decimal, converted from the millidegrees sysfs
    /// reports. `None` when the zone is there but would not give a reading.
    pub temperature_c: Option<f64>,
    /// Whether a temperature was actually read. False is "the zone exists and
    /// the kernel would not tell us", which is not the same as a reading of
    /// zero, and not the same as the zone not existing -- a zone that does not
    /// exist is not in the list at all.
    pub present: bool,
}

/// One cooling device: something the thermal framework can turn up, in whole
/// steps, to cool a zone. On this board that is the fan.
///
/// This is deliberately not [`crate::app::cooling_device::CoolingDevice`],
/// which is the shape `opt=get&type=cooling` has served for years and which
/// the fan control in the web UI writes back to. That one renames `pwm-fan`
/// to the platform node behind it and reports the raw steps as `speed` and
/// `max_speed`. This one passes the kernel's own names and numbers through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cooler {
    /// The device's `type`, as the driver spells it: `pwm-fan`. Falls back to
    /// the directory name.
    pub name: String,
    /// Which step the device is on right now. Steps are an index, not a
    /// percentage and not an RPM: `4` of `6` is the fifth of seven settings.
    pub cur_state: Option<u64>,
    /// The highest step it has.
    pub max_state: Option<u64>,
    /// Whether both states were read.
    pub present: bool,
}

/// Everything `/sys/class/thermal` has to say.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Thermal {
    pub sensors: Vec<ThermalSensor>,
    pub cooling: Vec<Cooler>,
}

/// Reads every thermal zone and cooling device the kernel has.
///
/// Both lists are empty on a board that cannot measure anything: a v2.4 board
/// has no fan, and every image before the one that added the SoC sensor to the
/// device tree has no zone either. That is a 200 with nothing in it, never an
/// error and never a made-up reading -- a caller has to be able to tell "this
/// board cannot measure temperature" from "this board is at 0 degrees".
pub async fn get_thermal_state() -> Thermal {
    read_thermal(Path::new(THERMAL_CLASS)).await
}

async fn read_thermal(thermal_class: &Path) -> Thermal {
    let (zones, coolers) = entries(thermal_class).await;

    let mut sensors = Vec::with_capacity(zones.len());
    for zone in zones {
        sensors.push(read_zone(&zone).await);
    }

    let mut cooling = Vec::with_capacity(coolers.len());
    for cooler in coolers {
        cooling.push(read_cooler(&cooler).await);
    }

    Thermal { sensors, cooling }
}

/// Splits the class directory into its zones and its cooling devices, each in
/// index order. A `/sys/class/thermal` that does not exist, or that we cannot
/// open, yields two empty lists rather than an error: on this board that is
/// what a kernel without thermal support looks like, and it is a fact about
/// the board worth reporting, not a failure of the request.
async fn entries(thermal_class: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut zones: Vec<(u32, PathBuf)> = Vec::new();
    let mut coolers: Vec<(u32, PathBuf)> = Vec::new();

    let Ok(mut dir) = tokio::fs::read_dir(thermal_class).await else {
        return (Vec::new(), Vec::new());
    };

    while let Some(entry) = dir.next_entry().await.unwrap_or(None) {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if let Some(index) = index_of(&file_name, "thermal_zone") {
            zones.push((index, entry.path()));
        } else if let Some(index) = index_of(&file_name, "cooling_device") {
            coolers.push((index, entry.path()));
        }
    }

    zones.sort_by_key(|(index, _)| *index);
    coolers.sort_by_key(|(index, _)| *index);

    (
        zones.into_iter().map(|(_, path)| path).collect(),
        coolers.into_iter().map(|(_, path)| path).collect(),
    )
}

/// `thermal_zone10` -> `10`. readdir hands these back in whatever order the
/// filesystem likes, and sorting the names as text would put `thermal_zone10`
/// before `thermal_zone2`, so the index is parsed out and sorted on. A name
/// without one is not something the thermal framework created, and is skipped.
fn index_of(file_name: &str, prefix: &str) -> Option<u32> {
    file_name.strip_prefix(prefix)?.parse().ok()
}

async fn read_zone(dir: &Path) -> ThermalSensor {
    let name = read_name(dir).await;
    let temperature_c = read_attribute::<i64>(dir, "temp")
        .await
        .map(millidegrees_to_celsius);

    ThermalSensor {
        name,
        temperature_c,
        present: temperature_c.is_some(),
    }
}

async fn read_cooler(dir: &Path) -> Cooler {
    let name = read_name(dir).await;
    let cur_state = read_state(dir, "cur_state").await;
    let max_state = read_state(dir, "max_state").await;

    Cooler {
        name,
        cur_state,
        max_state,
        present: cur_state.is_some() && max_state.is_some(),
    }
}

/// The `type` attribute both kinds of directory carry, falling back to the
/// directory's own name. A zone with an unreadable `type` is still a zone, and
/// `thermal_zone0` is a more useful thing to show a reader than an empty name.
async fn read_name(dir: &Path) -> String {
    match read_attribute_string(dir, "type").await {
        Some(name) => name,
        None => dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// A cooling device's state, as a step index. The kernel formats these with a
/// signed conversion, and a driver that has not been asked for a state yet can
/// print `-1`; that means "unknown", not "one step below the bottom", so it
/// becomes `None` the same way an unreadable attribute does.
async fn read_state(dir: &Path, attribute: &str) -> Option<u64> {
    read_attribute::<i64>(dir, attribute)
        .await
        .and_then(|state| u64::try_from(state).ok())
}

/// Sysfs reports zone temperatures in millidegrees Celsius; the board reads
/// `52539`. Raw millidegrees would be a number nobody can read at a glance and
/// a whole degree throws away detail the sensor has, so this is one decimal:
/// 52.5. Rounding, not truncation, so 52999 is 53.0 rather than 52.9.
fn millidegrees_to_celsius(millidegrees: i64) -> f64 {
    (millidegrees as f64 / 100.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Builds a `/sys/class/thermal` lookalike. Every value here was copied
    /// off a running board: one zone named by the device-tree node we added,
    /// reading 52539 millidegrees, and the fan the kernel now drives from it
    /// sitting on step 4 of 6.
    fn fake_thermal_class() -> (tempdir::TempDir, PathBuf) {
        let dir = tempdir::TempDir::new("thermal_class").expect("tempdir");
        let root = dir.path().to_path_buf();

        write_zone(&root, "thermal_zone0", "bmc-thermal", "52539");
        write_cooler(&root, "cooling_device0", "pwm-fan", "4", "6");

        (dir, root)
    }

    fn write_zone(root: &Path, dir_name: &str, zone_type: &str, temp: &str) {
        let zone = root.join(dir_name);
        fs::create_dir_all(&zone).expect("create zone dir");
        fs::write(zone.join("type"), format!("{}\n", zone_type)).expect("type");
        fs::write(zone.join("temp"), format!("{}\n", temp)).expect("temp");
    }

    fn write_cooler(root: &Path, dir_name: &str, dev_type: &str, cur: &str, max: &str) {
        let device = root.join(dir_name);
        fs::create_dir_all(&device).expect("create cooling dir");
        fs::write(device.join("type"), format!("{}\n", dev_type)).expect("type");
        fs::write(device.join("cur_state"), format!("{}\n", cur)).expect("cur_state");
        fs::write(device.join("max_state"), format!("{}\n", max)).expect("max_state");
    }

    /// The board as it reads today. This is the exact `Thermal` value the
    /// serialization test in `api::legacy` asserts the bytes of.
    #[tokio::test]
    async fn reports_the_soc_sensor_and_the_fan_it_drives() {
        let (_guard, root) = fake_thermal_class();

        assert_eq!(
            read_thermal(&root).await,
            Thermal {
                sensors: vec![ThermalSensor {
                    name: "bmc-thermal".to_string(),
                    temperature_c: Some(52.5),
                    present: true,
                }],
                cooling: vec![Cooler {
                    name: "pwm-fan".to_string(),
                    cur_state: Some(4),
                    max_state: Some(6),
                    present: true,
                }],
            }
        );
    }

    /// Millidegrees are converted, not passed through and not rounded to a
    /// whole degree. The first case is the board's own reading.
    #[test]
    fn a_reading_is_degrees_to_one_decimal() {
        assert_eq!(millidegrees_to_celsius(52539), 52.5);
        assert_eq!(millidegrees_to_celsius(0), 0.0);
        assert_eq!(
            millidegrees_to_celsius(52999),
            53.0,
            "rounded, not truncated"
        );
        assert_eq!(millidegrees_to_celsius(-5250), -5.3);
        assert_eq!(millidegrees_to_celsius(100_000), 100.0);
    }

    /// Every image before the one that added the sensor to the device tree,
    /// and every v2.4 board, which has no fan. Empty lists, and the caller can
    /// see that the board has nothing to measure rather than reading a zero.
    #[tokio::test]
    async fn a_board_with_no_thermal_zones_reports_empty_lists() {
        let empty = tempdir::TempDir::new("thermal_class").expect("tempdir");

        assert_eq!(
            read_thermal(empty.path()).await,
            Thermal {
                sensors: Vec::new(),
                cooling: Vec::new(),
            }
        );
    }

    /// A kernel with no thermal support at all: the class directory is not
    /// there to open. Still two empty lists, not an error.
    #[tokio::test]
    async fn a_missing_thermal_class_reports_empty_lists() {
        let empty = tempdir::TempDir::new("thermal_class").expect("tempdir");
        let missing = empty.path().join("not-a-directory");

        let thermal = read_thermal(&missing).await;

        assert!(thermal.sensors.is_empty());
        assert!(thermal.cooling.is_empty());
    }

    /// A zone whose driver answers the read with an error, which sysfs does
    /// routinely. The zone is still listed, and says it has no reading, rather
    /// than taking the whole request down or inventing a temperature.
    #[tokio::test]
    async fn a_zone_that_will_not_give_a_reading_is_listed_without_one() {
        let dir = tempdir::TempDir::new("thermal_class").expect("tempdir");
        let zone = dir.path().join("thermal_zone0");
        fs::create_dir_all(&zone).expect("create zone dir");
        fs::write(zone.join("type"), "bmc-thermal\n").expect("type");

        let sensors = read_thermal(dir.path()).await.sensors;

        assert_eq!(sensors.len(), 1, "the zone is still reported");
        assert_eq!(sensors[0].name, "bmc-thermal");
        assert_eq!(sensors[0].temperature_c, None);
        assert!(!sensors[0].present);
    }

    /// The same for a cooling device, and for a driver that reports `-1`
    /// because nothing has set a state yet.
    #[tokio::test]
    async fn a_cooler_without_states_is_listed_without_them() {
        let dir = tempdir::TempDir::new("thermal_class").expect("tempdir");
        write_cooler(dir.path(), "cooling_device0", "pwm-fan", "-1", "6");
        let bare = dir.path().join("cooling_device1");
        fs::create_dir_all(&bare).expect("create cooling dir");

        let cooling = read_thermal(dir.path()).await.cooling;

        assert_eq!(cooling.len(), 2);
        assert_eq!(cooling[0].cur_state, None, "-1 is not a step");
        assert_eq!(cooling[0].max_state, Some(6));
        assert!(!cooling[0].present);
        // no `type` either, so it is named after its directory
        assert_eq!(cooling[1].name, "cooling_device1");
        assert!(!cooling[1].present);
    }

    /// readdir order is not index order, and text order puts `thermal_zone10`
    /// before `thermal_zone2`. Zones come back numerically so that the first
    /// entry is `thermal_zone0` on every read.
    #[tokio::test]
    async fn zones_and_coolers_come_back_in_index_order() {
        let dir = tempdir::TempDir::new("thermal_class").expect("tempdir");
        for index in [10, 2, 0, 1] {
            write_zone(
                dir.path(),
                &format!("thermal_zone{}", index),
                &format!("zone{}", index),
                "40000",
            );
            write_cooler(
                dir.path(),
                &format!("cooling_device{}", index),
                &format!("cooler{}", index),
                "0",
                "1",
            );
        }

        let thermal = read_thermal(dir.path()).await;

        assert_eq!(
            thermal
                .sensors
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zone0", "zone1", "zone2", "zone10"]
        );
        assert_eq!(
            thermal
                .cooling
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["cooler0", "cooler1", "cooler2", "cooler10"]
        );
    }

    /// The class directory holds other things -- `thermal_zone` has no index,
    /// and a kernel may leave anything else in there. Neither list picks them
    /// up, and neither list is a fixed length, so a board with a zone but no
    /// cooling device reports exactly that.
    #[tokio::test]
    async fn only_indexed_zones_and_coolers_are_listed() {
        let dir = tempdir::TempDir::new("thermal_class").expect("tempdir");
        write_zone(dir.path(), "thermal_zone0", "bmc-thermal", "52539");
        fs::create_dir_all(dir.path().join("thermal_zone")).expect("create dir");
        fs::create_dir_all(dir.path().join("cooling_devices")).expect("create dir");
        fs::write(dir.path().join("uevent"), "\n").expect("uevent");

        let thermal = read_thermal(dir.path()).await;

        assert_eq!(thermal.sensors.len(), 1);
        assert_eq!(thermal.sensors[0].name, "bmc-thermal");
        assert!(thermal.cooling.is_empty(), "this board has no fan");
    }
}
