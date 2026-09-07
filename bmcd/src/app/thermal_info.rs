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
//!
//! One number the thermal class does not have is what a step actually *does*.
//! `cur_state` 4 of `max_state` 6 is an index into a table that lives in the
//! device tree, and the board carries its own -- so the table is read from
//! there rather than left for a UI to hardcode. See [`cooling_levels`].
use crate::app::sysfs::{read_attribute, read_attribute_string};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The root of sysfs. Two things under it are read here: the thermal class,
/// and the platform bus, which is the only way back from a cooling device to
/// the device-tree node that describes it.
const SYSFS_ROOT: &str = "/sys";

/// Where the kernel exposes both thermal zones and cooling devices.
const THERMAL_CLASS: &str = "class/thermal";

/// Where the kernel lists, per driver, the devices that driver has bound.
const PLATFORM_DRIVERS: &str = "bus/platform/drivers";

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
    /// What each step does, indexed by step: `levels[cur_state]` is the PWM
    /// duty the fan is being driven at, out of the 255 the pwm-fan binding
    /// counts in. Read out of the board's own device tree, so it is a fact
    /// about this board and not a table copied into a client. `None` on a
    /// board that does not describe one, which is not the same as a fan that
    /// runs flat out -- see [`cooling_levels`].
    pub levels: Option<Vec<u32>>,
    /// The duty at `max_state`, which is the last entry of `levels`: the
    /// denominator for turning a step into a percentage. 254 here, not 255 --
    /// this board's top step is not quite full duty, and saying 255 would be
    /// inventing the difference.
    pub max_level: Option<u32>,
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
    read_thermal(Path::new(SYSFS_ROOT)).await
}

async fn read_thermal(sysfs: &Path) -> Thermal {
    let (zones, coolers) = entries(&sysfs.join(THERMAL_CLASS)).await;

    let mut sensors = Vec::with_capacity(zones.len());
    for zone in zones {
        sensors.push(read_zone(&zone).await);
    }

    let mut cooling = Vec::with_capacity(coolers.len());
    for cooler in coolers {
        cooling.push(read_cooler(sysfs, &cooler).await);
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

async fn read_cooler(sysfs: &Path, dir: &Path) -> Cooler {
    let name = read_name(dir).await;
    let cur_state = read_state(dir, "cur_state").await;
    let max_state = read_state(dir, "max_state").await;
    let levels = cooling_levels(sysfs, dir, max_state).await;
    let max_level = levels.as_ref().and_then(|levels| levels.last().copied());

    Cooler {
        name,
        cur_state,
        max_state,
        present: cur_state.is_some() && max_state.is_some(),
        levels,
        max_level,
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

/// The duty behind each of a cooling device's steps, read out of the board's
/// own device tree.
///
/// `cur_state` and `max_state` are an index and a count; the numbers they
/// index live in the `cooling-levels` property of the device-tree node the
/// driver was probed from, and nothing in `/sys/class/thermal` reports them.
/// Without them the only honest thing a UI can show is "step 4 of 6" -- a
/// percentage would be one board's table hardcoded into a client and presented
/// as a measurement. The board has the table, so it is read from the board.
///
/// # Finding the node
///
/// The cooling device does not point at it. `__thermal_cooling_device_register`
/// keeps the `device_node` it was given in `cdev->np` and sets neither
/// `device.of_node` nor `device.parent` on the class device, so
/// `/sys/class/thermal/cooling_deviceN` has no `of_node` symlink and no
/// `device` symlink to follow. The one thing it does carry is its `type`,
/// which the driver passes to that same call -- `pwm-fan` registers the string
/// `"pwm-fan"`, which is also its platform driver name. So the route back is
/// `type` -> `/sys/bus/platform/drivers/<type>/` -> the devices bound to that
/// driver -> each device's `of_node` symlink, which lands in
/// `/sys/firmware/devicetree/base` -- the tree `/proc/device-tree` is a
/// symlink to. No path in the device tree is assumed anywhere; on this board
/// it resolves to the `system-fan` node, and on a board that calls it
/// something else it resolves to whatever that is.
///
/// # When it refuses
///
/// The match is checked before the table is believed. The pwm-fan driver sets
/// `max_state` to one less than the number of levels it parsed, so a table of
/// the wrong length is not this device's table, and `None` is the answer. So
/// is a driver with no bound device, a driver with more than one -- there is
/// nothing in sysfs that says which of them this cooling device is, and
/// guessing would publish another fan's numbers -- a node with no
/// `cooling-levels` at all, and a device whose `max_state` would not read.
/// Every one of those reports `"levels": null`, which a caller can tell apart
/// from a table. None of them is an error: a v2.4 board has no fan.
async fn cooling_levels(sysfs: &Path, dir: &Path, max_state: Option<u64>) -> Option<Vec<u32>> {
    let max_state = max_state?;
    let node = device_tree_node(sysfs, dir).await?;
    let levels = read_be_u32_property(&node.join("cooling-levels")).await?;

    if levels.len() as u64 != max_state.saturating_add(1) {
        tracing::warn!(
            "{}: cooling-levels has {} entries but max_state is {}; not reporting them",
            dir.display(),
            levels.len(),
            max_state
        );
        return None;
    }

    Some(levels)
}

/// The device-tree node of the driver that registered this cooling device, by
/// way of the platform bus. `None` unless exactly one device is bound to that
/// driver and carries an `of_node`.
async fn device_tree_node(sysfs: &Path, dir: &Path) -> Option<PathBuf> {
    let driver = read_attribute_string(dir, "type").await?;
    // The `type` is a kernel string, but it is about to become a path
    // component, and a path component is the one place a surprise in it would
    // matter.
    if driver.is_empty() || driver.starts_with('.') || driver.contains('/') {
        return None;
    }

    let bound = bound_devices(&sysfs.join(PLATFORM_DRIVERS).join(&driver)).await;
    match bound.as_slice() {
        [device] => Some(device.join("of_node")),
        [] => None,
        devices => {
            tracing::warn!(
                "{} devices bound to the `{}` driver; cannot say which one {} is",
                devices.len(),
                driver,
                dir.display()
            );
            None
        }
    }
}

/// The devices a platform driver has bound, which are the entries of its
/// directory that have an `of_node` -- that filter is also what leaves the
/// driver's own `bind`, `unbind` and `uevent` files out.
async fn bound_devices(driver_dir: &Path) -> Vec<PathBuf> {
    let mut devices = Vec::new();

    let Ok(mut dir) = tokio::fs::read_dir(driver_dir).await else {
        return devices;
    };

    while let Some(entry) = dir.next_entry().await.unwrap_or(None) {
        let path = entry.path();
        if tokio::fs::metadata(path.join("of_node")).await.is_ok() {
            devices.push(path);
        }
    }

    devices.sort();
    devices
}

/// A device-tree property as the array of big-endian `u32`s it is on disk.
///
/// The tree under `/sys/firmware/devicetree/base` is the flattened blob laid
/// out as files, not text: `cooling-levels` on this board is 28 bytes, and the
/// second cell reads `00 00 00 10`. Reading that little-endian gives
/// `268435456`, which is not obviously wrong at a glance and is exactly the
/// kind of number that gets shipped. `None` for a property that is not a whole
/// number of cells, or that is not there at all.
async fn read_be_u32_property(path: &Path) -> Option<Vec<u32>> {
    let bytes = tokio::fs::read(path).await.ok()?;
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }

    Some(
        bytes
            .chunks_exact(4)
            .map(|cell| u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]))
            .collect(),
    )
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
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    /// The `cooling-levels` this board's device tree carries, as
    /// `/proc/device-tree/system-fan/cooling-levels` reads them.
    const BOARD_LEVELS: [u32; 7] = [0, 16, 32, 64, 102, 170, 254];

    /// Builds a `/sys` lookalike: the thermal class, and the platform bus that
    /// leads from a cooling device to its device-tree node. Every value here
    /// was copied off a running board -- one zone named by the device-tree
    /// node we added, reading 52539 millidegrees, and the fan the kernel now
    /// drives from it sitting on step 4 of 6 with a seven-entry table behind
    /// those steps.
    fn fake_sysfs() -> (tempdir::TempDir, PathBuf) {
        let dir = tempdir::TempDir::new("sysfs").expect("tempdir");
        let root = dir.path().to_path_buf();

        write_zone(&root, "thermal_zone0", "bmc-thermal", "52539");
        write_cooler(&root, "cooling_device0", "pwm-fan", "4", "6");
        let node = write_platform_device(&root, "pwm-fan", "system-fan");
        write_cooling_levels(&node, &BOARD_LEVELS);

        (dir, root)
    }

    fn thermal_class(root: &Path) -> PathBuf {
        root.join(THERMAL_CLASS)
    }

    fn write_zone(root: &Path, dir_name: &str, zone_type: &str, temp: &str) {
        let zone = thermal_class(root).join(dir_name);
        fs::create_dir_all(&zone).expect("create zone dir");
        fs::write(zone.join("type"), format!("{}\n", zone_type)).expect("type");
        fs::write(zone.join("temp"), format!("{}\n", temp)).expect("temp");
    }

    fn write_cooler(root: &Path, dir_name: &str, dev_type: &str, cur: &str, max: &str) {
        let device = thermal_class(root).join(dir_name);
        fs::create_dir_all(&device).expect("create cooling dir");
        fs::write(device.join("type"), format!("{}\n", dev_type)).expect("type");
        fs::write(device.join("cur_state"), format!("{}\n", cur)).expect("cur_state");
        fs::write(device.join("max_state"), format!("{}\n", max)).expect("max_state");
    }

    /// Wires a platform device up the way the kernel does: the driver's
    /// directory links to the device, and the device links to the device-tree
    /// node it was probed from. Returns the node, for a test to put properties
    /// in. The driver's own attribute files go in too, because they are what
    /// [`bound_devices`] has to leave out.
    fn write_platform_device(root: &Path, driver: &str, device: &str) -> PathBuf {
        let node = root.join("firmware/devicetree/base").join(device);
        fs::create_dir_all(&node).expect("create device-tree node");

        let device_dir = root.join("devices/platform").join(device);
        fs::create_dir_all(&device_dir).expect("create device dir");
        symlink(&node, device_dir.join("of_node")).expect("of_node link");

        let driver_dir = root.join(PLATFORM_DRIVERS).join(driver);
        fs::create_dir_all(&driver_dir).expect("create driver dir");
        symlink(&device_dir, driver_dir.join(device)).expect("driver link");
        for attribute in ["bind", "unbind", "uevent"] {
            fs::write(driver_dir.join(attribute), "").expect("driver attribute");
        }

        node
    }

    /// A device-tree property is the flattened blob's cells on disk: raw
    /// big-endian `u32`s, no text and no separators.
    fn write_cooling_levels(node: &Path, levels: &[u32]) {
        let mut bytes = Vec::with_capacity(levels.len() * 4);
        for level in levels {
            bytes.extend_from_slice(&level.to_be_bytes());
        }
        fs::write(node.join("cooling-levels"), bytes).expect("cooling-levels");
    }

    /// The board as it reads today. This is the exact `Thermal` value the
    /// serialization test in `api::legacy` asserts the bytes of.
    #[tokio::test]
    async fn reports_the_soc_sensor_and_the_fan_it_drives() {
        let (_guard, root) = fake_sysfs();

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
                    levels: Some(BOARD_LEVELS.to_vec()),
                    max_level: Some(254),
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

    /// The cells are big-endian, and a little-endian read of them is the whole
    /// reason this is a test: `00 00 00 10` is 16 that way round and
    /// 268435456 the other, and 268435456 looks enough like data to ship.
    /// These are the 28 bytes the board's own property holds.
    #[tokio::test]
    async fn a_property_is_read_as_big_endian_cells() {
        let dir = tempdir::TempDir::new("device_tree").expect("tempdir");
        let path = dir.path().join("cooling-levels");

        fs::write(
            &path,
            [
                0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0, 32, 0, 0, 0, 64, 0, 0, 0, 102, 0, 0, 0, 170, 0,
                0, 0, 254u8,
            ],
        )
        .expect("write");
        assert_eq!(
            read_be_u32_property(&path).await,
            Some(BOARD_LEVELS.to_vec())
        );

        // not a whole number of cells, empty, and not there at all
        fs::write(&path, [0u8, 0, 0]).expect("write");
        assert_eq!(read_be_u32_property(&path).await, None);
        fs::write(&path, b"").expect("write");
        assert_eq!(read_be_u32_property(&path).await, None);
        assert_eq!(read_be_u32_property(&dir.path().join("absent")).await, None);
    }

    /// A fan whose node says nothing about duty. Every image before the one
    /// that carries the property, and any board that describes its fan without
    /// a table. The steps are still reported; the duty is `null`, which a
    /// caller can tell from a table, rather than a fabricated one.
    #[tokio::test]
    async fn a_fan_with_no_table_in_the_device_tree_reports_no_levels() {
        let dir = tempdir::TempDir::new("sysfs").expect("tempdir");
        let root = dir.path();
        write_cooler(root, "cooling_device0", "pwm-fan", "4", "6");
        write_platform_device(root, "pwm-fan", "system-fan");

        let cooling = read_thermal(root).await.cooling;

        assert_eq!(cooling[0].cur_state, Some(4));
        assert_eq!(cooling[0].max_state, Some(6));
        assert!(cooling[0].present, "the steps are still there");
        assert_eq!(cooling[0].levels, None);
        assert_eq!(cooling[0].max_level, None);
    }

    /// A board with no fan at all -- v2.4 -- and a board whose fan is not a
    /// platform device the kernel can lead us back from. Neither invents a
    /// table, and neither is an error.
    #[tokio::test]
    async fn a_cooling_device_with_no_platform_driver_reports_no_levels() {
        let dir = tempdir::TempDir::new("sysfs").expect("tempdir");
        let root = dir.path();
        write_cooler(root, "cooling_device0", "pwm-fan", "4", "6");

        let cooling = read_thermal(root).await.cooling;

        assert_eq!(cooling.len(), 1);
        assert_eq!(cooling[0].levels, None);
    }

    /// The check that says we found the right node. `pwm_fan_of_get_cooling_data`
    /// sets `max_state` to one less than the number of levels it parsed, so a
    /// table of any other length belongs to something else -- and publishing
    /// it would be the exact failure this endpoint exists to avoid.
    #[tokio::test]
    async fn a_table_that_disagrees_with_max_state_is_not_reported() {
        let dir = tempdir::TempDir::new("sysfs").expect("tempdir");
        let root = dir.path();
        write_cooler(root, "cooling_device0", "pwm-fan", "1", "2");
        let node = write_platform_device(root, "pwm-fan", "system-fan");
        write_cooling_levels(&node, &BOARD_LEVELS);

        assert_eq!(read_thermal(root).await.cooling[0].levels, None);
    }

    /// Two fans on one driver. Nothing in `/sys/class/thermal` says which of
    /// them `cooling_device0` is, so neither table is reported rather than one
    /// of them being guessed at. `cooling_device.rs` takes the first and warns;
    /// that is a name for a control to write back to, and this is a number a
    /// reader would take for a measurement.
    #[tokio::test]
    async fn two_devices_on_one_driver_report_no_levels() {
        let dir = tempdir::TempDir::new("sysfs").expect("tempdir");
        let root = dir.path();
        write_cooler(root, "cooling_device0", "pwm-fan", "4", "6");
        let first = write_platform_device(root, "pwm-fan", "system-fan");
        let second = write_platform_device(root, "pwm-fan", "other-fan");
        write_cooling_levels(&first, &BOARD_LEVELS);
        write_cooling_levels(&second, &BOARD_LEVELS);

        assert_eq!(read_thermal(root).await.cooling[0].levels, None);
    }

    /// Every image before the one that added the sensor to the device tree,
    /// and every v2.4 board, which has no fan. Empty lists, and the caller can
    /// see that the board has nothing to measure rather than reading a zero.
    #[tokio::test]
    async fn a_board_with_no_thermal_zones_reports_empty_lists() {
        let empty = tempdir::TempDir::new("sysfs").expect("tempdir");

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
        let empty = tempdir::TempDir::new("sysfs").expect("tempdir");
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
        let dir = tempdir::TempDir::new("sysfs").expect("tempdir");
        let zone = thermal_class(dir.path()).join("thermal_zone0");
        fs::create_dir_all(&zone).expect("create zone dir");
        fs::write(zone.join("type"), "bmc-thermal\n").expect("type");

        let sensors = read_thermal(dir.path()).await.sensors;

        assert_eq!(sensors.len(), 1, "the zone is still reported");
        assert_eq!(sensors[0].name, "bmc-thermal");
        assert_eq!(sensors[0].temperature_c, None);
        assert!(!sensors[0].present);
    }

    /// The same for a cooling device, and for a driver that reports `-1`
    /// because nothing has set a state yet. A device with no step count has
    /// nothing to check a table against, so it reports no levels either --
    /// even though the node behind it has one.
    #[tokio::test]
    async fn a_cooler_without_states_is_listed_without_them() {
        let dir = tempdir::TempDir::new("sysfs").expect("tempdir");
        let root = dir.path();
        write_cooler(root, "cooling_device0", "pwm-fan", "-1", "6");
        let node = write_platform_device(root, "pwm-fan", "system-fan");
        write_cooling_levels(&node, &BOARD_LEVELS);
        let bare = thermal_class(root).join("cooling_device1");
        fs::create_dir_all(&bare).expect("create cooling dir");

        let cooling = read_thermal(root).await.cooling;

        assert_eq!(cooling.len(), 2);
        assert_eq!(cooling[0].cur_state, None, "-1 is not a step");
        assert_eq!(cooling[0].max_state, Some(6));
        assert!(!cooling[0].present);
        assert_eq!(
            cooling[0].levels,
            Some(BOARD_LEVELS.to_vec()),
            "an unknown current step does not hide the table"
        );
        // no `type` either, so it is named after its directory, and there is
        // no driver name to follow back to a node
        assert_eq!(cooling[1].name, "cooling_device1");
        assert!(!cooling[1].present);
        assert_eq!(cooling[1].levels, None);
    }

    /// readdir order is not index order, and text order puts `thermal_zone10`
    /// before `thermal_zone2`. Zones come back numerically so that the first
    /// entry is `thermal_zone0` on every read.
    #[tokio::test]
    async fn zones_and_coolers_come_back_in_index_order() {
        let dir = tempdir::TempDir::new("sysfs").expect("tempdir");
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
        let dir = tempdir::TempDir::new("sysfs").expect("tempdir");
        write_zone(dir.path(), "thermal_zone0", "bmc-thermal", "52539");
        fs::create_dir_all(thermal_class(dir.path()).join("thermal_zone")).expect("create dir");
        fs::create_dir_all(thermal_class(dir.path()).join("cooling_devices")).expect("create dir");
        fs::write(thermal_class(dir.path()).join("uevent"), "\n").expect("uevent");

        let thermal = read_thermal(dir.path()).await;

        assert_eq!(thermal.sensors.len(), 1);
        assert_eq!(thermal.sensors[0].name, "bmc-thermal");
        assert!(thermal.cooling.is_empty(), "this board has no fan");
    }
}
