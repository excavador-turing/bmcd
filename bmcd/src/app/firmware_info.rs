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
//! The A/B firmware slots, as UBI knows them.
//!
//! A firmware upgrade writes the new image into the rootfs volume that is not
//! running, boots it once, and only then promotes it. Two volumes therefore
//! hold a firmware at any time and the daemon has never said which is which.
//! Everything here is read; nothing in this module writes, boots or promotes
//! anything.
use serde::Serialize;
use std::io::SeekFrom;
use std::path::Path;
use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Where the kernel exposes UBI devices and their volumes.
const UBI_CLASS: &str = "/sys/class/ubi";
/// Where the kernel exposes block devices, `ubiblock` among them.
const BLOCK_CLASS: &str = "/sys/class/block";
/// The one UBI device on this board. The NAND carries a single UBI device on
/// a single MTD partition; there is no second one to discover.
const UBI_DEVICE: &str = "ubi0";
/// Volumes whose name starts with this are firmware slots. The rest of the
/// device -- `uboot`, `uboot-env`, the overlay -- is not a slot and is not
/// reported here.
const SLOT_PREFIX: &str = "rootfs";
/// The name the promotion script gives the volume it is rolling back to.
const ROLLBACK_VOLUME: &str = "rootfs_prev";
/// The name a staged, not yet promoted image is written under. It is not a
/// rollback target: it is where the board is going, not where it came from.
const STAGING_VOLUME: &str = "rootfs_new";
/// The U-Boot variable that carries a staged update, read through the tool
/// that owns the environment.
const NEXTBOOT_VARIABLE: &str = "nextboot";
const FW_PRINTENV: &str = "fw_printenv";
/// Where the promotion script writes what it did.
const PROMOTION_LOG: &str = "/mnt/overlay/postupdate.log";
/// How much of the tail of that log to read. The file grows by a few lines
/// per upgrade and lives on the overlay, but this daemon runs on a board with
/// 116 MB of RAM in total, so it is read from the end with a bound rather
/// than slurped.
const PROMOTION_LOG_TAIL: u64 = 8192;
/// The marker the promotion script puts between its timestamp and its verdict.
const PROMOTION_MARKER: &str = "postupdate:";

/// One firmware slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Slot {
    /// UBI volume name, as the volume itself spells it.
    pub volume: String,
    /// Volume id within the UBI device, the `N` of `ubi0_N`.
    pub volume_id: u32,
    /// `data_bytes` of the volume: how much firmware is in it, not how much
    /// space it was given.
    pub size_bytes: Option<u64>,
    /// Firmware version, and only ever for the running slot. The rollback
    /// volume is not mounted -- nothing on this board reads a squashfs that
    /// is not the root filesystem -- so its `/etc/os-release` is not
    /// reachable, and a version is not guessed from a volume name.
    pub version: Option<String>,
}

/// What the promotion script last said it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Promotion {
    /// The date the script itself printed, passed through verbatim rather
    /// than reformatted: it is the board's own idea of the time, which on a
    /// BMC that has just come up may be well before the real one.
    pub timestamp: String,
    /// Everything the script said after its marker. Not classified into
    /// success or failure here, because the vocabulary of that log belongs to
    /// the firmware and not to this daemon.
    pub message: String,
}

/// The A/B slot state of the board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareSlots {
    /// Whether the UBI device is there at all. False on a board that boots
    /// from something else, and on any kernel without UBI: the rest of the
    /// fields are then empty because there is nothing to read, not because
    /// something failed.
    pub present: bool,
    /// The slot the board booted from, identified by which volume has an
    /// attached `ubiblock` device.
    pub running: Option<Slot>,
    /// The slot a rollback would land on, when one can be named without
    /// guessing.
    pub rollback: Option<Slot>,
    /// Whether an update is staged for the next boot. `null` means the
    /// U-Boot environment could not be read at all -- no `fw_printenv` on the
    /// board -- which is not the same as "no update is staged".
    pub update_staged: Option<bool>,
    /// The raw `nextboot` variable when it is set, so a caller can see what
    /// is staged and not merely that something is.
    pub nextboot: Option<String>,
    /// The last line the promotion script wrote, when there is a log.
    pub last_promotion: Option<Promotion>,
}

/// One UBI volume, before it is decided what part it plays.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Volume {
    id: u32,
    name: String,
    size_bytes: Option<u64>,
    /// Whether this volume has an attached `ubiblock` device, which on this
    /// board means it is the mounted root filesystem.
    running: bool,
}

/// Reads the slot state. Always answers: a board without UBI, without
/// `fw_printenv` and without a promotion log is `present: false` and three
/// nulls, which is an honest description of that board rather than a failure.
///
/// `running_version` is the firmware version of the running slot, read from
/// `/etc/os-release` by the caller -- the one place in this daemon that
/// already parses it.
pub async fn get_firmware_slots(running_version: Option<String>) -> FirmwareSlots {
    let volumes = read_volumes(
        Path::new(UBI_CLASS),
        Path::new(BLOCK_CLASS),
        UBI_DEVICE,
        SLOT_PREFIX,
    )
    .await;

    let (running, rollback) = volumes
        .as_deref()
        .map(|volumes| pick_slots(volumes, running_version))
        .unwrap_or((None, None));

    let (update_staged, nextboot) = read_nextboot().await;

    FirmwareSlots {
        present: volumes.is_some(),
        running,
        rollback,
        update_staged,
        nextboot,
        last_promotion: read_promotion(Path::new(PROMOTION_LOG)).await,
    }
}

/// Reads every volume of `device` whose name starts with `prefix`. `None`
/// when the UBI device is not there at all, which is the difference between
/// "this board has no UBI" and "this board has UBI and no firmware slots in
/// it".
async fn read_volumes(
    ubi_class: &Path,
    block_class: &Path,
    device: &str,
    prefix: &str,
) -> Option<Vec<Volume>> {
    let device_dir = ubi_class.join(device);
    let mut entries = tokio::fs::read_dir(&device_dir).await.ok()?;

    let mut volumes = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = entry_name
            .strip_prefix(&format!("{}_", device))
            .and_then(|id| id.parse::<u32>().ok())
        else {
            continue;
        };

        let volume_dir = device_dir.join(&entry_name);
        let Some(name) = read_attribute_string(&volume_dir, "name").await else {
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }

        volumes.push(Volume {
            id,
            name,
            size_bytes: read_attribute(&volume_dir, "data_bytes").await,
            running: is_attached(block_class, device, id).await,
        });
    }

    volumes.sort_by_key(|volume| volume.id);
    Some(volumes)
}

/// Whether a volume has a `ubiblock` device on it. The upgrade machinery
/// attaches one to the volume it is about to boot and the kernel keeps it for
/// the volume it booted from, so this -- not the volume's name -- is what
/// says which slot is running.
///
/// The kernel names that device `ubiblock<device>_<volume>`; an older one
/// names it `ubiblock<volume>`. Both are accepted, because the fork that
/// answers this question has to work on the firmware that is on the boards
/// today as well as the one being built.
async fn is_attached(block_class: &Path, device: &str, id: u32) -> bool {
    let number = device.trim_start_matches("ubi");
    for name in [
        format!("ubiblock{}_{}", number, id),
        format!("ubiblock{}", id),
    ] {
        if tokio::fs::metadata(block_class.join(name)).await.is_ok() {
            return true;
        }
    }
    false
}

/// Decides which of the slot volumes is running and which is the rollback
/// target.
///
/// The running one is the one with a `ubiblock` device. The rollback is the
/// volume the promotion script names for it, or -- when the running slot is
/// known -- the single other slot that is not the staging volume. Anything
/// more ambiguous than that is reported as no rollback at all: a wrong answer
/// here names the volume somebody would boot in a recovery, and no answer is
/// better than a guess.
fn pick_slots(volumes: &[Volume], running_version: Option<String>) -> (Option<Slot>, Option<Slot>) {
    let running = volumes.iter().find(|volume| volume.running);

    let others: Vec<&Volume> = volumes
        .iter()
        .filter(|volume| !volume.running && volume.name != STAGING_VOLUME)
        .collect();

    let rollback = others
        .iter()
        .find(|volume| volume.name == ROLLBACK_VOLUME)
        .or_else(|| (running.is_some() && others.len() == 1).then(|| &others[0]))
        .map(|volume| to_slot(volume, None));

    (
        running.map(|volume| to_slot(volume, running_version)),
        rollback,
    )
}

fn to_slot(volume: &Volume, version: Option<String>) -> Slot {
    Slot {
        volume: volume.name.clone(),
        volume_id: volume.id,
        size_bytes: volume.size_bytes,
        version,
    }
}

/// Reads the `nextboot` U-Boot variable through `fw_printenv`.
///
/// This is the one thing here that is not a file read. The U-Boot environment
/// is a redundant, CRC-covered pair of MTD regions, and `fw_printenv` --
/// which is on the board, and holds the lock that keeps a concurrent
/// `fw_setenv` from tearing the read -- is the tool that owns that format.
/// Re-implementing it to save a fork would be a second parser of a structure
/// this daemon does not otherwise touch.
///
/// Returns whether an update is staged and, when one is, what for. A board
/// with no `fw_printenv` answers `(None, None)`: not knowing is reported as
/// not knowing.
async fn read_nextboot() -> (Option<bool>, Option<String>) {
    let output = tokio::task::spawn_blocking(|| {
        Command::new(FW_PRINTENV)
            .args(["-n", NEXTBOOT_VARIABLE])
            .output()
    })
    .await;

    let Ok(Ok(output)) = output else {
        return (None, None);
    };

    if !output.status.success() {
        // `fw_printenv` exits non-zero for a variable that is not set, which
        // is the ordinary state of a board with nothing staged. Any other
        // failure -- an unreadable or corrupt environment -- is not evidence
        // that nothing is staged, so it is reported as not knowing.
        let stderr = String::from_utf8_lossy(&output.stderr);
        return (stderr.contains("not defined").then_some(false), None);
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        (Some(false), None)
    } else {
        (Some(true), Some(value))
    }
}

/// Reads the last thing the promotion script said, from the tail of its log.
async fn read_promotion(path: &Path) -> Option<Promotion> {
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let length = file.metadata().await.ok()?.len();
    let start = length.saturating_sub(PROMOTION_LOG_TAIL);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).await.ok()?;
    }

    let mut buffer = Vec::with_capacity(PROMOTION_LOG_TAIL as usize);
    file.take(PROMOTION_LOG_TAIL)
        .read_to_end(&mut buffer)
        .await
        .ok()?;

    // From the end: the last line that looks like the script's own is the
    // verdict. Reading from an offset can cut the first line in half, and
    // scanning backwards means that half is the last thing considered rather
    // than the first.
    String::from_utf8_lossy(&buffer)
        .lines()
        .rev()
        .find_map(parse_promotion)
}

/// Splits one log line into the date the script printed and what it said.
/// A line without the marker is not one of its lines and is skipped.
fn parse_promotion(line: &str) -> Option<Promotion> {
    let (timestamp, message) = line.split_once(PROMOTION_MARKER)?;
    let timestamp = timestamp.trim();
    let message = message.trim();

    (!timestamp.is_empty() && !message.is_empty()).then(|| Promotion {
        timestamp: timestamp.to_string(),
        message: message.to_string(),
    })
}

/// Reads one sysfs attribute as trimmed text. A missing file, an unreadable
/// one and an empty one are all `None`.
async fn read_attribute_string(dir: &Path, attribute: &str) -> Option<String> {
    let value = tokio::fs::read_to_string(dir.join(attribute)).await.ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

async fn read_attribute<T: std::str::FromStr>(dir: &Path, attribute: &str) -> Option<T> {
    read_attribute_string(dir, attribute).await?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A `/sys/class/ubi` and a `/sys/class/block` built from what the board
    /// reads today: `rootfs` at volume 1 with a ubiblock device on it, and
    /// `rootfs_prev` at volume 3 without one.
    fn fake_ubi() -> (tempdir::TempDir, PathBuf, PathBuf) {
        let dir = tempdir::TempDir::new("ubi").expect("tempdir");
        let ubi_class = dir.path().join("ubi");
        let block_class = dir.path().join("block");

        write_volume(&ubi_class, 0, "uboot", 1048576);
        write_volume(&ubi_class, 1, "rootfs", 37019648);
        write_volume(&ubi_class, 2, "overlay", 157286400);
        write_volume(&ubi_class, 3, "rootfs_prev", 37011456);
        attach(&block_class, "ubiblock0_1");

        (dir, ubi_class, block_class)
    }

    fn write_volume(ubi_class: &Path, id: u32, name: &str, data_bytes: u64) {
        let volume = ubi_class.join(UBI_DEVICE).join(format!("ubi0_{}", id));
        fs::create_dir_all(&volume).expect("create volume dir");
        fs::write(volume.join("name"), format!("{}\n", name)).expect("name");
        fs::write(volume.join("data_bytes"), format!("{}\n", data_bytes)).expect("data_bytes");
    }

    fn attach(block_class: &Path, name: &str) {
        fs::create_dir_all(block_class.join(name)).expect("create block dir");
    }

    async fn slots_of(ubi_class: &Path, block_class: &Path) -> (Option<Slot>, Option<Slot>) {
        let volumes = read_volumes(ubi_class, block_class, UBI_DEVICE, SLOT_PREFIX)
            .await
            .expect("ubi device");
        pick_slots(&volumes, Some("v2.2.0-unstable-hive.5".to_string()))
    }

    #[tokio::test]
    async fn only_the_rootfs_volumes_are_slots() {
        let (_guard, ubi_class, block_class) = fake_ubi();
        let volumes = read_volumes(&ubi_class, &block_class, UBI_DEVICE, SLOT_PREFIX)
            .await
            .expect("ubi device");

        assert_eq!(
            volumes
                .iter()
                .map(|volume| volume.name.as_str())
                .collect::<Vec<_>>(),
            vec!["rootfs", "rootfs_prev"],
            "uboot and overlay are not firmware slots"
        );
    }

    #[tokio::test]
    async fn the_attached_volume_is_the_running_slot() {
        let (_guard, ubi_class, block_class) = fake_ubi();
        let (running, rollback) = slots_of(&ubi_class, &block_class).await;

        let running = running.expect("a running slot");
        assert_eq!(running.volume, "rootfs");
        assert_eq!(running.volume_id, 1);
        assert_eq!(running.size_bytes, Some(37019648));
        assert_eq!(running.version.as_deref(), Some("v2.2.0-unstable-hive.5"));

        let rollback = rollback.expect("a rollback slot");
        assert_eq!(rollback.volume, "rootfs_prev");
        assert_eq!(rollback.volume_id, 3);
        assert_eq!(rollback.size_bytes, Some(37011456));
        assert_eq!(
            rollback.version, None,
            "the rollback volume is not mounted, so its version is not readable"
        );
    }

    /// The same board after an upgrade has been written but before it has
    /// been promoted: three rootfs volumes, and the new one is not where a
    /// rollback goes.
    #[tokio::test]
    async fn a_staged_volume_is_not_the_rollback_target() {
        let (_guard, ubi_class, block_class) = fake_ubi();
        write_volume(&ubi_class, 4, STAGING_VOLUME, 37020000);

        let (running, rollback) = slots_of(&ubi_class, &block_class).await;

        assert_eq!(running.expect("running").volume, "rootfs");
        assert_eq!(rollback.expect("rollback").volume, "rootfs_prev");
    }

    /// A board that has never been upgraded has one firmware and nowhere to
    /// roll back to. That is a fact about the board, not a missing reading.
    #[tokio::test]
    async fn a_board_with_one_slot_has_no_rollback() {
        let dir = tempdir::TempDir::new("ubi").expect("tempdir");
        let ubi_class = dir.path().join("ubi");
        let block_class = dir.path().join("block");
        write_volume(&ubi_class, 1, "rootfs", 37019648);
        attach(&block_class, "ubiblock0_1");

        let (running, rollback) = slots_of(&ubi_class, &block_class).await;

        assert_eq!(running.expect("running").volume, "rootfs");
        assert_eq!(rollback, None);
    }

    /// Two slots and no ubiblock device anywhere: the kernel cannot tell us
    /// which one it booted. `rootfs_prev` is still named by the promotion
    /// script, so it is still the rollback target, but nothing claims to know
    /// what is running.
    #[tokio::test]
    async fn without_a_ubiblock_device_nothing_is_called_running() {
        let (_guard, ubi_class, block_class) = fake_ubi();
        let nowhere = block_class.join("empty");
        let (running, rollback) = slots_of(&ubi_class, &nowhere).await;

        assert_eq!(running, None);
        assert_eq!(rollback.expect("rollback").volume, "rootfs_prev");
    }

    /// The older kernel naming, `ubiblock1` rather than `ubiblock0_1`.
    #[tokio::test]
    async fn the_short_ubiblock_name_is_accepted_too() {
        let dir = tempdir::TempDir::new("ubi").expect("tempdir");
        let ubi_class = dir.path().join("ubi");
        let block_class = dir.path().join("block");
        write_volume(&ubi_class, 1, "rootfs", 37019648);
        write_volume(&ubi_class, 3, "rootfs_prev", 37011456);
        attach(&block_class, "ubiblock1");

        let (running, _) = slots_of(&ubi_class, &block_class).await;
        assert_eq!(running.expect("running").volume, "rootfs");
    }

    /// No UBI at all. Not an error, and not an empty list of slots that could
    /// be read as "UBI is there and has nothing in it".
    #[tokio::test]
    async fn a_board_without_ubi_reads_as_absent() {
        let dir = tempdir::TempDir::new("ubi").expect("tempdir");
        let volumes = read_volumes(
            &dir.path().join("nothing"),
            &dir.path().join("block"),
            UBI_DEVICE,
            SLOT_PREFIX,
        )
        .await;

        assert_eq!(volumes, None);
    }

    /// A volume whose attributes the kernel will not answer for. The volume
    /// is skipped when it has no name -- there is nothing to call it -- but a
    /// missing size does not take the slot down with it.
    #[tokio::test]
    async fn a_volume_without_a_size_is_still_a_slot() {
        let dir = tempdir::TempDir::new("ubi").expect("tempdir");
        let ubi_class = dir.path().join("ubi");
        let volume = ubi_class.join(UBI_DEVICE).join("ubi0_1");
        fs::create_dir_all(&volume).expect("create volume dir");
        fs::write(volume.join("name"), "rootfs\n").expect("name");

        let volumes = read_volumes(
            &ubi_class,
            &dir.path().join("block"),
            UBI_DEVICE,
            SLOT_PREFIX,
        )
        .await
        .expect("ubi device");

        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].size_bytes, None);
    }

    /// The line the promotion script wrote on this board, verbatim.
    #[test]
    fn the_last_promotion_line_is_split_into_a_date_and_a_verdict() {
        let promotion = parse_promotion(
            "Mon Sep  7 19:30:22 UTC 2026 postupdate: switch ports present: node1 node2 node3 node4",
        )
        .expect("a promotion line");

        assert_eq!(promotion.timestamp, "Mon Sep  7 19:30:22 UTC 2026");
        assert_eq!(
            promotion.message, "switch ports present: node1 node2 node3 node4",
            "the colons inside the verdict are left alone"
        );
    }

    #[test]
    fn a_line_that_is_not_the_scripts_is_skipped() {
        assert_eq!(parse_promotion("some other daemon said something"), None);
        assert_eq!(parse_promotion("postupdate: no date"), None);
        assert_eq!(
            parse_promotion("Mon Sep  7 19:30:22 UTC 2026 postupdate:"),
            None
        );
    }

    #[tokio::test]
    async fn the_last_line_of_the_log_wins() {
        let dir = tempdir::TempDir::new("promotion").expect("tempdir");
        let log = dir.path().join("postupdate.log");
        fs::write(
            &log,
            concat!(
                "Sun Sep  6 10:00:00 UTC 2026 postupdate: rollback armed\n",
                "a line from something else\n",
                "Mon Sep  7 19:30:22 UTC 2026 postupdate: switch ports present: node1 node2 node3 node4\n",
            ),
        )
        .expect("write log");

        let promotion = read_promotion(&log).await.expect("a promotion");
        assert_eq!(promotion.timestamp, "Mon Sep  7 19:30:22 UTC 2026");
        assert_eq!(
            promotion.message,
            "switch ports present: node1 node2 node3 node4"
        );
    }

    /// A log longer than the tail we read. The last line still wins, and the
    /// half line the offset cuts through is not mistaken for one.
    #[tokio::test]
    async fn a_long_log_is_read_from_its_end() {
        let dir = tempdir::TempDir::new("promotion").expect("tempdir");
        let log = dir.path().join("postupdate.log");
        let mut content = "Sun Sep  6 10:00:00 UTC 2026 postupdate: an old one\n".repeat(1024);
        content.push_str("Mon Sep  7 19:30:22 UTC 2026 postupdate: the last one\n");
        fs::write(&log, &content).expect("write log");
        assert!(content.len() as u64 > PROMOTION_LOG_TAIL);

        let promotion = read_promotion(&log).await.expect("a promotion");
        assert_eq!(promotion.message, "the last one");
    }

    /// A board that has never been upgraded has no log.
    #[tokio::test]
    async fn no_log_is_no_promotion() {
        let dir = tempdir::TempDir::new("promotion").expect("tempdir");
        assert_eq!(read_promotion(&dir.path().join("absent.log")).await, None);
    }
}
