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
//!
//! Whether the board is keeping its settings, and the one repair for when it
//! is not.
//!
//! ## What this answers
//!
//! `/sbin/preinit` mounts the UBI volume `overlay` at `/mnt/overlay` and
//! layers the root file system over it, so a password, a certificate or an
//! address survives a reboot. When that volume cannot be mounted it does not
//! stop: it boots on a writable layer in RAM (`/tmp/safemode`), and from then
//! on every reboot is a factory reset that nobody was told about. The same
//! layer is used on purpose when KEY1 is held at power-on. This module says
//! which of the two the board is in, why, and whether the board can repair
//! itself.
//!
//! ## `/proc/mounts` is the authority
//!
//! A file written by preinit says *why*, but only the mount table says what
//! is true now: a stale or missing file must not be able to make a board that
//! is losing its settings look healthy, or the other way round. So `state`
//! comes from the mount of `/`, and `/etc/safemode.reason` (written by
//! firmware 2.46.0 and later, in safe mode only) is consulted only to
//! explain a state that is already `temporary`.
//!
//! ## The repair
//!
//! A volume that cannot be mounted (a UBIFS journal that will not replay,
//! say) stays that way, and nothing in preinit repairs it. Removing the
//! volume does: `ubirmvol /dev/ubi0 -N overlay`, and on the next boot
//! preinit's `mount_overlay` finds no volume and recreates it with `ubimkvol`
//! and `mkfs.ubifs`. **What was on the volume is erased for good** -- network
//! configuration, certificates, node names, application data. It is not being
//! loaded now, but it is not recoverable afterwards, and a mount failure can
//! be transient: a plain reboot is the first thing to try.
//!
//! ## KEY1 always wins
//!
//! `requested` is decided by the kernel command line before anything else,
//! whatever the reason file says. Preinit decides safe mode by `grep -q
//! 'safemode' /proc/cmdline`, a substring match, and so does this: the
//! conservative direction is the one that blocks the repair.
//!
//! All the parsing is pure and takes the text of the file, so it is tested
//! with captured ones.
use crate::app::address_applier::{ApplyError, Exec, SystemExec};
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const PROC_MOUNTS: &str = "/proc/mounts";
const PROC_CMDLINE: &str = "/proc/cmdline";
/// Written by preinit in safe mode only. Line 1 is `reason=requested` or
/// `reason=mount_failed`; the rest is free text for a person.
const REASON_FILE: &str = "/etc/safemode.reason";
const SYS_CLASS_UBI: &str = "/sys/class/ubi";
const SYS_CLASS_BLOCK: &str = "/sys/class/block";
/// Where preinit mounts the settings volume.
const OVERLAY_MOUNT: &str = "/mnt/overlay";
/// The name of the UBI volume preinit mounts there and recreates when it is
/// missing.
const OVERLAY_VOLUME: &str = "overlay";
/// The UBI device the settings volume lives on.
const UBI_DEVICE: &str = "/dev/ubi0";
/// Where preinit puts the writable layer when it cannot use the volume.
const SAFEMODE_UPPER: &str = "/tmp/safemode";
/// How long `ubirmvol` gets. It takes a second or two on a healthy volume; a
/// device that hangs must not hold the repair, and the guard, forever.
pub const REMOVE_TIMEOUT: Duration = Duration::from_secs(60);
/// What the free-text part of the reason file is cut to.
pub const MAX_DETAIL_BYTES: usize = 8 * 1024;

/// Whether the settings are kept across a reboot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Persistent,
    Temporary,
    Unknown,
}

/// Why the board is running on the temporary layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// KEY1 was held at power-on.
    Requested,
    /// The settings volume could not be mounted.
    MountFailed,
    /// The board is in safe mode and nothing says why (firmware older than
    /// 2.46.0 writes no reason).
    Unrecorded,
}

/// What the read-only root file system sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backing {
    Ubi,
    Mmc,
    Unknown,
}

/// What `GET /api/bmc/storage` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageStatus {
    pub state: State,
    pub reason: Option<Reason>,
    pub detail: Option<String>,
    pub backing: Backing,
    pub repairable: bool,
    pub repair_blocker: Option<String>,
}

// --- parsing -------------------------------------------------------------

/// One line of `/proc/mounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
    pub options: Vec<String>,
}

/// `\040` and friends: the kernel escapes space, tab, newline and backslash
/// in the first two fields of a mount line as three octal digits.
fn unescape_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..=i + 3]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value = (bytes[i + 1] - b'0') as u32 * 64
                + (bytes[i + 2] - b'0') as u32 * 8
                + (bytes[i + 3] - b'0') as u32;
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `/proc/mounts`, in order. A line that does not have the four fields is
/// skipped.
pub fn parse_mounts(contents: &str) -> Vec<MountEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = fields.next()?;
            let mountpoint = fields.next()?;
            let fstype = fields.next()?;
            let options = fields.next()?;
            Some(MountEntry {
                device: unescape_mount_field(device),
                mountpoint: unescape_mount_field(mountpoint),
                fstype: fstype.to_string(),
                options: options.split(',').map(str::to_string).collect(),
            })
        })
        .collect()
}

impl MountEntry {
    fn upperdir(&self) -> Option<&str> {
        self.options
            .iter()
            .find_map(|o| o.strip_prefix("upperdir="))
    }
}

/// Whether the root's writable layer survives a reboot, from the mount table.
///
/// The last overlay mounted on `/` is the one in effect. Its upper directory
/// is `/tmp/safemode/...` in safe mode and `/mnt/overlay/upper` otherwise; an
/// overlay with no upper directory is read-only and says nothing about
/// persistence, like any root that is not an overlay at all.
pub fn state_from_mounts(mounts: &[MountEntry]) -> State {
    let Some(root) = mounts
        .iter()
        .rev()
        .find(|m| m.mountpoint == "/" && m.fstype == "overlay")
    else {
        return State::Unknown;
    };
    match root.upperdir() {
        Some(dir) if is_safemode_upper(dir) => State::Temporary,
        Some(_) => State::Persistent,
        None => State::Unknown,
    }
}

fn is_safemode_upper(dir: &str) -> bool {
    dir == SAFEMODE_UPPER || dir.starts_with("/tmp/safemode/")
}

/// A block device the kernel knows, as `/sys/class/block/<name>/dev` says:
/// its name and its `MAJOR:MINOR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDevice {
    pub name: String,
    pub dev: String,
}

/// The kernel's `root=` argument, if the command line has one. The last wins,
/// as it does for the kernel.
pub fn cmdline_root(cmdline: &str) -> Option<&str> {
    cmdline
        .split_whitespace()
        .rev()
        .find_map(|word| word.strip_prefix("root="))
}

/// The device name a kernel `root=` value stands for, or `None` when it does
/// not say (or cannot be worked out here).
///
/// - `254:0` is `MAJOR:MINOR`, looked up among `devices`;
/// - `/dev/mmcblk0p2` (or the bare `mmcblk0p2`) is its own name;
/// - `PARTUUID=`, `PARTLABEL=`, `UUID=`, `LABEL=`, `ubi0:rootfs` and the like
///   are not resolved: they stay unknown, which blocks the repair.
pub fn root_device_name(root: &str, devices: &[BlockDevice]) -> Option<String> {
    let root = root.trim();
    if let Some((major, minor)) = root.split_once(':') {
        let numeric = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        if !(numeric(major) && numeric(minor)) {
            return None;
        }
        // 254:0 and 254:00 are the same device.
        let canonical = format!(
            "{}:{}",
            major.trim_start_matches('0').parse::<u32>().ok()?,
            minor.trim_start_matches('0').parse::<u32>().unwrap_or(0)
        );
        return devices
            .iter()
            .find(|d| d.dev == canonical)
            .map(|d| d.name.clone());
    }
    if root.contains('=') {
        return None;
    }
    let name = root.strip_prefix("/dev/").unwrap_or(root);
    if name.is_empty() || name == "root" || name.contains('/') {
        return None;
    }
    Some(name.to_string())
}

fn classify_device(name: &str) -> Backing {
    if name.starts_with("ubiblock") {
        Backing::Ubi
    } else if name.starts_with("mmcblk") {
        Backing::Mmc
    } else {
        Backing::Unknown
    }
}

/// What the read-only root file system is on: `/mnt/rom` when preinit mounted
/// the image there, otherwise `/`.
///
/// The mount source is the device when it names one (`/dev/ubiblock0_1`). On a
/// board whose kernel mounted the root itself it is the pseudo-name
/// `/dev/root`, which is not a node and says nothing; then the kernel's own
/// `root=` argument is resolved instead, a `MAJOR:MINOR` through the block
/// devices in sysfs. A root that is itself the overlay has no device behind it
/// and is treated the same way. Anything that cannot be resolved is
/// `unknown`, which does not offer the repair.
pub fn backing_from_mounts(
    mounts: &[MountEntry],
    cmdline: &str,
    devices: &[BlockDevice],
) -> Backing {
    let root = ["/mnt/rom", "/"]
        .iter()
        .find_map(|point| mounts.iter().rev().find(|m| m.mountpoint == *point));
    let Some(root) = root else {
        return Backing::Unknown;
    };
    let telling = root.device.starts_with("/dev/") && root.device != "/dev/root";
    if telling {
        return classify_device(root.device.strip_prefix("/dev/").unwrap_or(&root.device));
    }
    cmdline_root(cmdline)
        .and_then(|value| root_device_name(value, devices))
        .map(|name| classify_device(&name))
        .unwrap_or(Backing::Unknown)
}

/// Whether anything is mounted at `/mnt/overlay`.
pub fn overlay_mounted(mounts: &[MountEntry]) -> bool {
    mounts.iter().any(|m| m.mountpoint == OVERLAY_MOUNT)
}

/// The reason file: line 1 `reason=requested` or `reason=mount_failed`, and
/// whatever follows it for a person. `None` for the reason when line 1 is
/// anything else; the detail is the rest of the file, trimmed and capped at
/// [`MAX_DETAIL_BYTES`], or `None` when there is none.
pub fn parse_reason_file(contents: &str) -> (Option<Reason>, Option<String>) {
    let (first, rest) = contents.split_once('\n').unwrap_or((contents, ""));
    let reason = match first.trim().strip_prefix("reason=").map(str::trim) {
        Some("requested") => Some(Reason::Requested),
        Some("mount_failed") => Some(Reason::MountFailed),
        _ => None,
    };
    let rest = rest.trim();
    let detail = if rest.is_empty() {
        None
    } else {
        Some(cap(rest, MAX_DETAIL_BYTES).trim_end().to_string())
    };
    (reason, detail)
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn cap(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Whether the kernel command line mentions `safemode`, which is how preinit
/// learns that KEY1 was held.
///
/// A substring, exactly as preinit tests it (`grep -q 'safemode'
/// /proc/cmdline`), so `safemode=1` and `x=safemode` count too. Where the two
/// could disagree, this one errs towards "requested", which blocks the repair.
pub fn cmdline_requests_safemode(cmdline: &str) -> bool {
    cmdline.contains("safemode")
}

/// Whether one of the UBI volumes is named exactly `overlay`.
pub fn has_overlay_volume(names: &[String]) -> bool {
    names.iter().any(|n| n == OVERLAY_VOLUME)
}

/// The names of the volumes of the first UBI device, from
/// `<sys>/ubi0_*/name`. `ubi0` itself has no underscore and is not one.
pub fn read_ubi_volume_names(sys_class_ubi: &Path) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(sys_class_ubi) else {
        return Vec::new();
    };
    let mut names: Vec<String> = dir
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("ubi0_"))
        .filter_map(|e| std::fs::read_to_string(e.path().join("name")).ok())
        .map(|n| n.trim().to_string())
        .collect();
    names.sort();
    names
}

/// The block devices under `<sys>/class/block/<name>/dev`, which holds
/// `MAJOR:MINOR`. A device whose file cannot be read is left out.
pub fn read_block_devices(sys_class_block: &Path) -> Vec<BlockDevice> {
    let Ok(dir) = std::fs::read_dir(sys_class_block) else {
        return Vec::new();
    };
    let mut devices: Vec<BlockDevice> = dir
        .flatten()
        .filter_map(|e| {
            let dev = std::fs::read_to_string(e.path().join("dev")).ok()?;
            Some(BlockDevice {
                name: e.file_name().into_string().ok()?,
                dev: dev.trim().to_string(),
            })
        })
        .collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    devices
}

// --- the decision --------------------------------------------------------

/// What the board is, as read; [`assess`] turns it into an answer.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub mounts: Vec<MountEntry>,
    /// `None` when `/etc/safemode.reason` does not exist.
    pub reason_file: Option<String>,
    pub cmdline: String,
    pub ubi_volumes: Vec<String>,
    /// The block devices in sysfs, to resolve a `root=` given as
    /// `MAJOR:MINOR`.
    pub block_devices: Vec<BlockDevice>,
}

/// Why repair is not offered, or `None` when it is.
///
/// Every condition has to hold, and the first that does not is the one said:
/// the board is on the temporary layer; it is not there because somebody
/// asked (the saved settings are fine and a normal reboot uses them); the
/// settings are on the built-in flash, which is the only place the repair
/// knows how to fix; the volume exists to be removed; and nothing is using
/// it.
pub fn repair_blocker(
    state: State,
    reason: Option<Reason>,
    backing: Backing,
    volume_present: bool,
    overlay_is_mounted: bool,
) -> Option<String> {
    match state {
        State::Persistent => {
            return Some(
                "The board is running on its saved settings; there is nothing to repair."
                    .to_string(),
            )
        }
        State::Unknown => {
            return Some(
                "The board could not tell whether it is running on its saved settings, so \
                 repair is not offered."
                    .to_string(),
            )
        }
        State::Temporary => {}
    }
    if reason == Some(Reason::Requested) {
        return Some(
            "Safemode was requested at power-on (KEY1). Reboot without holding KEY1 to use \
             the saved settings."
                .to_string(),
        );
    }
    match backing {
        Backing::Ubi => {}
        Backing::Mmc => {
            return Some(
                "This board keeps its settings on the SD card; repair is only offered for the \
                 built-in flash."
                    .to_string(),
            )
        }
        Backing::Unknown => {
            return Some(
                "The board could not tell where its settings are kept; repair is only offered \
                 for the built-in flash."
                    .to_string(),
            )
        }
    }
    if !volume_present {
        return Some(
            "There is no settings volume to remove; reboot the BMC to recreate it.".to_string(),
        );
    }
    if overlay_is_mounted {
        return Some(
            "The settings volume is mounted at /mnt/overlay, so it is in use and will not be \
             removed."
                .to_string(),
        );
    }
    None
}

/// The answer, from what was read.
pub fn assess(facts: &Facts) -> StorageStatus {
    let state = state_from_mounts(&facts.mounts);
    let backing = backing_from_mounts(&facts.mounts, &facts.cmdline, &facts.block_devices);
    let (reason, detail) = if state == State::Temporary {
        let (from_file, detail) = facts
            .reason_file
            .as_deref()
            .map(parse_reason_file)
            .unwrap_or((None, None));
        // KEY1 wins over whatever the file says: the key is what preinit
        // acted on, and "requested" is the answer that blocks the repair.
        let reason = if cmdline_requests_safemode(&facts.cmdline) {
            Reason::Requested
        } else {
            from_file.unwrap_or(Reason::Unrecorded)
        };
        (Some(reason), detail)
    } else {
        (None, None)
    };
    let blocker = repair_blocker(
        state,
        reason,
        backing,
        has_overlay_volume(&facts.ubi_volumes),
        overlay_mounted(&facts.mounts),
    );
    StorageStatus {
        state,
        reason,
        detail,
        backing,
        repairable: blocker.is_none(),
        repair_blocker: blocker,
    }
}

// --- the board -----------------------------------------------------------

/// Where the facts are read from. [`StoragePaths::system`] is the board;
/// anything else is a test.
#[derive(Debug, Clone)]
pub struct StoragePaths {
    mounts: PathBuf,
    cmdline: PathBuf,
    reason: PathBuf,
    ubi: PathBuf,
    block: PathBuf,
}

impl StoragePaths {
    pub fn system() -> Self {
        StoragePaths {
            mounts: PROC_MOUNTS.into(),
            cmdline: PROC_CMDLINE.into(),
            reason: REASON_FILE.into(),
            ubi: SYS_CLASS_UBI.into(),
            block: SYS_CLASS_BLOCK.into(),
        }
    }

    #[cfg(test)]
    pub fn under(root: &Path) -> Self {
        StoragePaths {
            mounts: root.join("mounts"),
            cmdline: root.join("cmdline"),
            reason: root.join("safemode.reason"),
            ubi: root.join("ubi"),
            block: root.join("block"),
        }
    }

    pub fn read(&self) -> Facts {
        Facts {
            mounts: std::fs::read_to_string(&self.mounts)
                .map(|c| parse_mounts(&c))
                .unwrap_or_default(),
            reason_file: read_capped(&self.reason),
            cmdline: std::fs::read_to_string(&self.cmdline).unwrap_or_default(),
            ubi_volumes: read_ubi_volume_names(&self.ubi),
            block_devices: read_block_devices(&self.block),
        }
    }
}

/// A text file, or `None` when it cannot be read. Only the first few KiB are
/// taken: the file is written by a script, but nothing here should be able
/// to be made to hold a large one in memory.
fn read_capped(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_DETAIL_BYTES as u64 * 2)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Debug)]
pub enum RepairError {
    /// Not offered right now, and why.
    Blocked(String),
    /// `ubirmvol` failed.
    Command(ApplyError),
    /// `ubirmvol` did not finish in time.
    Timeout(Duration),
    Join(String),
}

impl std::fmt::Display for RepairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepairError::Blocked(why) => write!(f, "{why}"),
            RepairError::Command(e) => write!(f, "{e}"),
            RepairError::Timeout(after) => write!(
                f,
                "`ubirmvol` did not finish within {}s; the board was not rebooted",
                after.as_secs()
            ),
            RepairError::Join(e) => write!(f, "the repair did not finish: {e}"),
        }
    }
}

impl std::error::Error for RepairError {}

/// Reads the board's storage state and removes the settings volume.
#[derive(Clone)]
pub struct StorageService {
    exec: Arc<dyn Exec>,
    paths: StoragePaths,
    /// Set once the volume has been removed (or is being), so a second
    /// request cannot start a second repair before the board goes down.
    repairing: Arc<AtomicBool>,
    remove_timeout: Duration,
}

impl StorageService {
    pub fn system() -> Self {
        Self::with(Arc::new(SystemExec), StoragePaths::system())
    }

    pub fn with(exec: Arc<dyn Exec>, paths: StoragePaths) -> Self {
        StorageService {
            exec,
            paths,
            repairing: Arc::new(AtomicBool::new(false)),
            remove_timeout: REMOVE_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub fn with_timeout(mut self, remove_timeout: Duration) -> Self {
        self.remove_timeout = remove_timeout;
        self
    }

    pub fn status(&self) -> StorageStatus {
        assess(&self.paths.read())
    }

    /// [`status`](Self::status) off the async workers: it reads a handful of
    /// files.
    pub async fn status_async(&self) -> StorageStatus {
        let service = self.clone();
        tokio::task::spawn_blocking(move || service.status())
            .await
            .unwrap_or_else(|_| assess(&Facts::default()))
    }

    /// Remove the settings volume. Decided again from the board as it is now,
    /// never from an earlier answer.
    ///
    /// The blockers come first, so a request that is refused never touches
    /// the guard; only a request that is about to run `ubirmvol` takes it.
    /// After a success it stays taken, on purpose: the board is about to
    /// reboot and a second repair has nothing to remove. Every failure
    /// gives it back.
    pub fn repair(&self) -> Result<(), RepairError> {
        if let Some(why) = self.status().repair_blocker {
            return Err(RepairError::Blocked(why));
        }
        if self
            .repairing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(RepairError::Blocked(
                "A repair is already under way; the board is about to reboot.".to_string(),
            ));
        }
        self.exec
            .run("ubirmvol", &[UBI_DEVICE, "-N", OVERLAY_VOLUME])
            .map(|_| ())
            .map_err(|e| {
                self.repairing.store(false, Ordering::SeqCst);
                RepairError::Command(e)
            })
    }

    /// [`repair`](Self::repair) off the async workers, and not for longer
    /// than the removal timeout. A timeout gives the guard back (the command
    /// may still finish in the background; a repair asked for again is
    /// decided from the board as it is then).
    pub async fn repair_async(&self) -> Result<(), RepairError> {
        let service = self.clone();
        let task = tokio::task::spawn_blocking(move || service.repair());
        match tokio::time::timeout(self.remove_timeout, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                self.repairing.store(false, Ordering::SeqCst);
                Err(RepairError::Join(e.to_string()))
            }
            Err(_) => {
                self.repairing.store(false, Ordering::SeqCst);
                Err(RepairError::Timeout(self.remove_timeout))
            }
        }
    }

    /// One warning at startup when the board is not keeping its settings, so
    /// the log says so even if nobody opens the interface.
    pub async fn log_startup(&self) {
        let status = self.status_async().await;
        if status.state == State::Temporary {
            tracing::warn!(
                "settings are NOT being kept: running on a temporary layer (reason: {}), \
                 so every reboot is a factory reset",
                match status.reason {
                    Some(Reason::Requested) => "requested",
                    Some(Reason::MountFailed) => "mount_failed",
                    Some(Reason::Unrecorded) | None => "unrecorded",
                }
            );
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::app::address_applier::testing::FakeExec;

    /// The real line from a board in safe mode.
    pub const SAFEMODE_LINE: &str =
        "overlay / overlay rw,relatime,lowerdir=/,upperdir=/tmp/safemode/upper,workdir=/tmp/safemode/work 0 0";
    pub const PERSISTENT_LINE: &str =
        "overlay / overlay rw,relatime,lowerdir=/,upperdir=/mnt/overlay/upper,workdir=/mnt/overlay/work 0 0";

    /// `/proc/mounts` of a v2.4 board, with the root line swapped in.
    pub fn board_mounts(root: &str, with_overlay_volume: bool) -> String {
        let mut out = String::from(
            "rootfs / rootfs rw 0 0\n\
             /dev/ubiblock0_0 /mnt/rom squashfs ro,relatime 0 0\n\
             proc /proc proc rw,relatime 0 0\n\
             sysfs /sys sysfs rw,relatime 0 0\n\
             devtmpfs /dev devtmpfs rw,relatime,size=481536k,nr_inodes=120384,mode=755 0 0\n",
        );
        out.push_str(root);
        out.push('\n');
        out.push_str("tmpfs /tmp tmpfs rw,relatime 0 0\n");
        if with_overlay_volume {
            out.push_str("ubi0:overlay /mnt/overlay ubifs rw,sync,relatime,assert=read-only 0 0\n");
        }
        out
    }

    fn facts(root: &str) -> Facts {
        Facts {
            mounts: parse_mounts(&board_mounts(root, false)),
            reason_file: None,
            cmdline: "console=ttyS0,115200 root=/dev/ubiblock0_0 ro".to_string(),
            ubi_volumes: vec!["overlay".to_string(), "rootfs".to_string()],
            block_devices: Vec::new(),
        }
    }

    // --- /proc/mounts ----------------------------------------------------

    #[test]
    fn the_real_safemode_line_is_temporary() {
        let mounts = parse_mounts(SAFEMODE_LINE);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mountpoint, "/");
        assert_eq!(mounts[0].fstype, "overlay");
        assert_eq!(mounts[0].upperdir(), Some("/tmp/safemode/upper"));
        assert_eq!(state_from_mounts(&mounts), State::Temporary);
    }

    #[test]
    fn the_persistent_line_is_persistent() {
        assert_eq!(
            state_from_mounts(&parse_mounts(PERSISTENT_LINE)),
            State::Persistent
        );
    }

    #[test]
    fn anything_that_is_not_an_overlay_root_is_unknown() {
        for text in [
            "",
            "rootfs / rootfs rw 0 0\n",
            "/dev/mmcblk0p2 / ext4 rw,relatime 0 0\n",
            // an overlay somewhere else is not the root
            "overlay /var overlay rw,upperdir=/tmp/safemode/upper,workdir=/tmp/safemode/work 0 0\n",
            // a read-only overlay has no upper directory
            "overlay / overlay ro,relatime,lowerdir=/a:/b 0 0\n",
            "garbage\n",
        ] {
            assert_eq!(
                state_from_mounts(&parse_mounts(text)),
                State::Unknown,
                "{text:?}"
            );
        }
    }

    #[test]
    fn the_root_that_counts_is_the_last_overlay_on_slash() {
        let text = format!("{PERSISTENT_LINE}\n{SAFEMODE_LINE}\n");
        assert_eq!(state_from_mounts(&parse_mounts(&text)), State::Temporary);
        let text = format!("{SAFEMODE_LINE}\n{PERSISTENT_LINE}\n");
        assert_eq!(state_from_mounts(&parse_mounts(&text)), State::Persistent);
    }

    #[test]
    fn a_lookalike_upperdir_is_not_the_safemode_layer() {
        let line = "overlay / overlay rw,upperdir=/tmp/safemode-not/upper,workdir=/w 0 0";
        assert_eq!(state_from_mounts(&parse_mounts(line)), State::Persistent);
    }

    #[test]
    fn escaped_mountpoints_are_decoded() {
        let mounts = parse_mounts("/dev/sda1 /media/my\\040card vfat rw 0 0\n");
        assert_eq!(mounts[0].mountpoint, "/media/my card");
        assert_eq!(unescape_mount_field("a\\134b"), "a\\b");
        // not an escape: left as it is
        assert_eq!(unescape_mount_field("a\\9zb"), "a\\9zb");
        assert_eq!(unescape_mount_field("trailing\\04"), "trailing\\04");
        assert_eq!(unescape_mount_field("\\040"), " ");
    }

    #[test]
    fn backing_follows_the_read_only_root() {
        let ubi = parse_mounts(&board_mounts(SAFEMODE_LINE, false));
        assert_eq!(backing_from_mounts(&ubi, "", &[]), Backing::Ubi);

        let mmc = parse_mounts(
            "/dev/mmcblk0p2 /mnt/rom ext4 ro,relatime 0 0\n\
             overlay / overlay rw,upperdir=/tmp/safemode/upper,workdir=/w 0 0\n",
        );
        assert_eq!(backing_from_mounts(&mmc, "", &[]), Backing::Mmc);

        // No /mnt/rom: the root itself, when it is a block device.
        let direct = parse_mounts("/dev/ubiblock0_0 / squashfs ro 0 0\n");
        assert_eq!(backing_from_mounts(&direct, "", &[]), Backing::Ubi);
        let direct = parse_mounts("/dev/mmcblk1p1 / ext4 rw 0 0\n");
        assert_eq!(backing_from_mounts(&direct, "", &[]), Backing::Mmc);

        // /mnt/rom wins over /.
        let both = parse_mounts(
            "/dev/mmcblk0p1 / ext4 rw 0 0\n/dev/ubiblock0_0 /mnt/rom squashfs ro 0 0\n",
        );
        assert_eq!(backing_from_mounts(&both, "", &[]), Backing::Ubi);

        // An overlay root with no /mnt/rom has no device behind it.
        let overlay_only = parse_mounts(SAFEMODE_LINE);
        assert_eq!(
            backing_from_mounts(&overlay_only, "", &[]),
            Backing::Unknown
        );
        assert_eq!(backing_from_mounts(&[], "", &[]), Backing::Unknown);
        let other = parse_mounts("/dev/sda1 / ext4 rw 0 0\n");
        assert_eq!(backing_from_mounts(&other, "", &[]), Backing::Unknown);
    }

    #[test]
    fn a_mount_at_mnt_overlay_is_seen() {
        assert!(!overlay_mounted(&parse_mounts(&board_mounts(
            SAFEMODE_LINE,
            false
        ))));
        assert!(overlay_mounted(&parse_mounts(&board_mounts(
            PERSISTENT_LINE,
            true
        ))));
        // A mount below it is not the volume.
        assert!(!overlay_mounted(&parse_mounts(
            "tmpfs /mnt/overlay-old tmpfs rw 0 0\n"
        )));
    }

    // --- the reason file -------------------------------------------------

    #[test]
    fn the_reason_file_is_line_one_and_then_detail() {
        let (reason, detail) = parse_reason_file(
            "reason=mount_failed\nmount: mounting ubi0:overlay on /mnt/overlay failed: Structure needs cleaning\nUBIFS error: journal replay failed\n",
        );
        assert_eq!(reason, Some(Reason::MountFailed));
        assert_eq!(
            detail.as_deref(),
            Some(
                "mount: mounting ubi0:overlay on /mnt/overlay failed: Structure needs cleaning\n\
                 UBIFS error: journal replay failed"
            )
        );

        let (reason, detail) = parse_reason_file("reason=requested\n");
        assert_eq!(reason, Some(Reason::Requested));
        assert_eq!(detail, None);

        // no trailing newline, CRLF, padding
        assert_eq!(
            parse_reason_file("reason=requested").0,
            Some(Reason::Requested)
        );
        let (reason, detail) = parse_reason_file("  reason=mount_failed \r\n\r\n  why \n\n");
        assert_eq!(reason, Some(Reason::MountFailed));
        assert_eq!(detail.as_deref(), Some("why"));
    }

    #[test]
    fn an_unparsable_reason_file_gives_no_reason() {
        for text in [
            "",
            "\n",
            "hello\nworld",
            "reason=\n",
            "reason=other\n",
            "Reason=requested\n",
        ] {
            assert_eq!(parse_reason_file(text).0, None, "{text:?}");
        }
    }

    #[test]
    fn the_detail_is_capped_on_a_character_boundary() {
        let long = format!("reason=mount_failed\n{}", "é".repeat(MAX_DETAIL_BYTES));
        let (_, detail) = parse_reason_file(&long);
        let detail = detail.unwrap();
        assert!(detail.len() <= MAX_DETAIL_BYTES);
        assert!(detail.len() >= MAX_DETAIL_BYTES - 1);
        assert!(detail.chars().all(|c| c == 'é'));

        let ascii = format!("reason=mount_failed\n{}", "x".repeat(MAX_DETAIL_BYTES * 3));
        assert_eq!(parse_reason_file(&ascii).1.unwrap().len(), MAX_DETAIL_BYTES);
    }

    #[test]
    fn cmdline_safemode_is_a_substring_match_like_preinits_grep() {
        for yes in [
            "console=ttyS0 safemode ro",
            "safemode\n",
            "console=ttyS0 safemode=1 ro",
            "x=safemode",
            "nosafemode",
        ] {
            assert!(cmdline_requests_safemode(yes), "{yes:?}");
        }
        for no in ["console=ttyS0 root=/dev/ubiblock0_0 ro", "safe mode", ""] {
            assert!(!cmdline_requests_safemode(no), "{no:?}");
        }
    }

    // --- a real board: the root is /dev/root -----------------------------

    /// Board B, firmware v2.46.0, as it is: the root device is the kernel's
    /// pseudo-name `/dev/root`, which is not a node.
    const REAL_MOUNTS: &str = "\
/dev/root /mnt/rom erofs ro,relatime,user_xattr,acl,cache_strategy=readaround 0 0
/dev/ubi0_2 /mnt/overlay ubifs rw,relatime,assert=read-only,ubi=0,vol=2 0 0
overlay / overlay rw,relatime,lowerdir=/,upperdir=/mnt/overlay/upper,workdir=/mnt/overlay/work 0 0
/dev/mmcblk0p1 /mnt/sdcard ext4 rw,relatime 0 0
";
    const REAL_CMDLINE: &str = "loglevel=4 cma=8M init=/sbin/preinit rootwait=5 ubi.mtd=ubi ubi.block=0,rootfs_new root=254:0 postupdate";

    fn real_block_devices() -> Vec<BlockDevice> {
        [
            ("mmcblk0", "179:0"),
            ("mmcblk0p1", "179:1"),
            ("ubiblock0_1", "254:0"),
            ("loop0", "7:0"),
        ]
        .iter()
        .map(|(name, dev)| BlockDevice {
            name: name.to_string(),
            dev: dev.to_string(),
        })
        .collect()
    }

    fn real_facts() -> Facts {
        Facts {
            mounts: parse_mounts(REAL_MOUNTS),
            reason_file: None,
            cmdline: REAL_CMDLINE.to_string(),
            ubi_volumes: vec!["rootfs_new".to_string(), "overlay".to_string()],
            block_devices: real_block_devices(),
        }
    }

    #[test]
    fn the_kernels_root_argument_is_found() {
        assert_eq!(cmdline_root(REAL_CMDLINE), Some("254:0"));
        assert_eq!(cmdline_root("a root=/dev/x root=254:0 b"), Some("254:0"));
        assert_eq!(cmdline_root("rootwait=5 console=ttyS0"), None);
        assert_eq!(cmdline_root(""), None);
    }

    #[test]
    fn a_root_argument_is_resolved_to_a_device_name() {
        let devices = real_block_devices();
        assert_eq!(
            root_device_name("254:0", &devices).as_deref(),
            Some("ubiblock0_1")
        );
        assert_eq!(
            root_device_name("254:00", &devices).as_deref(),
            Some("ubiblock0_1")
        );
        assert_eq!(
            root_device_name("179:1", &devices).as_deref(),
            Some("mmcblk0p1")
        );
        assert_eq!(
            root_device_name("/dev/mmcblk0p2", &devices).as_deref(),
            Some("mmcblk0p2")
        );
        assert_eq!(
            root_device_name("mmcblk0p2", &devices).as_deref(),
            Some("mmcblk0p2")
        );
        // Not resolvable here: fail closed.
        for unresolvable in [
            "254:1",
            "8:abc",
            ":0",
            "PARTUUID=0a1b2c3d-02",
            "PARTLABEL=rootfs",
            "UUID=1234",
            "LABEL=root",
            "ubi0:rootfs",
            "/dev/root",
            "/dev/disk/by-label/x",
            "",
        ] {
            assert_eq!(
                root_device_name(unresolvable, &devices),
                None,
                "{unresolvable:?}"
            );
        }
        assert_eq!(root_device_name("254:0", &[]), None);
    }

    #[test]
    fn the_real_board_backs_on_ubi() {
        let mounts = parse_mounts(REAL_MOUNTS);
        assert_eq!(
            backing_from_mounts(&mounts, REAL_CMDLINE, &real_block_devices()),
            Backing::Ubi
        );
        // The same board with an SD-card root.
        let sd = REAL_CMDLINE.replace("root=254:0", "root=179:1");
        assert_eq!(
            backing_from_mounts(&mounts, &sd, &real_block_devices()),
            Backing::Mmc
        );
    }

    #[test]
    fn a_root_that_cannot_be_resolved_stays_unknown() {
        let mounts = parse_mounts(REAL_MOUNTS);
        let devices = real_block_devices();
        // sysfs unreadable, no root=, an unresolvable root=, a root= that is
        // some other device.
        assert_eq!(
            backing_from_mounts(&mounts, REAL_CMDLINE, &[]),
            Backing::Unknown
        );
        assert_eq!(
            backing_from_mounts(&mounts, "loglevel=4 postupdate", &devices),
            Backing::Unknown
        );
        for root in ["PARTUUID=0a1b2c3d-02", "254:9", "7:0", "/dev/sda1"] {
            let cmdline = format!("loglevel=4 root={root}");
            assert_eq!(
                backing_from_mounts(&mounts, &cmdline, &devices),
                Backing::Unknown,
                "{root}"
            );
        }
        // A mount source that is a real node that is neither is not
        // second-guessed by the command line.
        let sda = parse_mounts("/dev/sda1 /mnt/rom ext4 ro 0 0\n");
        assert_eq!(
            backing_from_mounts(&sda, REAL_CMDLINE, &devices),
            Backing::Unknown
        );
    }

    #[test]
    fn the_real_persistent_board_is_ubi_and_has_nothing_to_repair() {
        let status = assess(&real_facts());
        assert_eq!(status.state, State::Persistent);
        assert_eq!(status.backing, Backing::Ubi);
        assert!(!status.repairable);
        assert!(status.repair_blocker.unwrap().contains("nothing to repair"));
    }

    #[test]
    fn the_real_board_in_safemode_is_repairable() {
        let mut f = real_facts();
        // preinit could not mount the volume: the root is the tmpfs layer
        // and nothing is at /mnt/overlay.
        f.mounts = parse_mounts(
            "/dev/root /mnt/rom erofs ro,relatime,user_xattr,acl,cache_strategy=readaround 0 0\n\
             overlay / overlay rw,relatime,lowerdir=/,upperdir=/tmp/safemode/upper,workdir=/tmp/safemode/work 0 0\n\
             /dev/mmcblk0p1 /mnt/sdcard ext4 rw,relatime 0 0\n",
        );
        for reason_file in [None, Some("reason=mount_failed\nUBIFS error\n".to_string())] {
            f.reason_file = reason_file;
            let status = assess(&f);
            assert_eq!(status.state, State::Temporary);
            assert_eq!(status.backing, Backing::Ubi);
            assert!(status.repairable, "{status:?}");
            assert_eq!(status.repair_blocker, None);
        }
        assert_eq!(assess(&f).reason, Some(Reason::MountFailed));

        // ... but not with KEY1 held, nor with nothing resolvable.
        f.cmdline.push_str(" safemode");
        assert!(!assess(&f).repairable);
        f.cmdline = REAL_CMDLINE.to_string();
        f.block_devices.clear();
        let status = assess(&f);
        assert_eq!(status.backing, Backing::Unknown);
        assert!(!status.repairable);
    }

    #[test]
    fn the_service_resolves_the_root_through_sysfs_files() {
        let dir = tempdir::TempDir::new("real-board").unwrap();
        std::fs::write(dir.path().join("mounts"), REAL_MOUNTS).unwrap();
        std::fs::write(dir.path().join("cmdline"), format!("{REAL_CMDLINE}\n")).unwrap();
        for (name, dev) in [("ubiblock0_1", "254:0"), ("mmcblk0", "179:0")] {
            let path = dir.path().join("block").join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("dev"), format!("{dev}\n")).unwrap();
        }
        // A directory without a dev file is left out, not an error.
        std::fs::create_dir_all(dir.path().join("block/odd")).unwrap();
        let service =
            StorageService::with(FakeExec::board("", ""), StoragePaths::under(dir.path()));
        let status = service.status();
        assert_eq!(status.backing, Backing::Ubi);
        assert_eq!(status.state, State::Persistent);
        assert_eq!(read_block_devices(&dir.path().join("block")).len(), 2);
        assert!(read_block_devices(&dir.path().join("missing")).is_empty());
    }

    // --- ubi volumes -----------------------------------------------------

    #[test]
    fn ubi_volume_names_come_from_sysfs() {
        let dir = tempdir::TempDir::new("ubi-sysfs").unwrap();
        for (volume, name) in [
            ("ubi0_0", "rootfs\n"),
            ("ubi0_1", "overlay\n"),
            ("ubi0_2", "overlay_old\n"),
            ("ubi1_0", "other\n"),
        ] {
            let path = dir.path().join(volume);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("name"), name).unwrap();
        }
        // The device itself has no name file and no underscore.
        std::fs::create_dir_all(dir.path().join("ubi0")).unwrap();

        let names = read_ubi_volume_names(dir.path());
        assert_eq!(names, vec!["overlay", "overlay_old", "rootfs"]);
        assert!(has_overlay_volume(&names));
        assert!(!has_overlay_volume(&[
            "overlay_old".to_string(),
            "rootfs".to_string()
        ]));
        assert!(!has_overlay_volume(&[]));
        assert!(read_ubi_volume_names(&dir.path().join("missing")).is_empty());
    }

    // --- the answer ------------------------------------------------------

    #[test]
    fn a_healthy_board_is_persistent_and_ignores_the_reason_file() {
        let mut f = facts(PERSISTENT_LINE);
        f.reason_file = Some("reason=mount_failed\nleft over\n".to_string());
        f.cmdline.push_str(" safemode");
        let status = assess(&f);
        assert_eq!(status.state, State::Persistent);
        assert_eq!(status.reason, None);
        assert_eq!(status.detail, None);
        assert_eq!(status.backing, Backing::Ubi);
        assert!(!status.repairable);
        assert!(status.repair_blocker.is_some());
    }

    #[test]
    fn a_failed_mount_is_temporary_and_repairable() {
        let mut f = facts(SAFEMODE_LINE);
        f.reason_file = Some("reason=mount_failed\nUBIFS error\n".to_string());
        let status = assess(&f);
        assert_eq!(status.state, State::Temporary);
        assert_eq!(status.reason, Some(Reason::MountFailed));
        assert_eq!(status.detail.as_deref(), Some("UBIFS error"));
        assert_eq!(status.backing, Backing::Ubi);
        assert!(status.repairable);
        assert_eq!(status.repair_blocker, None);
    }

    #[test]
    fn older_firmware_writes_no_reason() {
        let status = assess(&facts(SAFEMODE_LINE));
        assert_eq!(status.state, State::Temporary);
        assert_eq!(status.reason, Some(Reason::Unrecorded));
        assert_eq!(status.detail, None);
        assert!(status.repairable);

        // ... unless the command line says the key was held.
        let mut f = facts(SAFEMODE_LINE);
        f.cmdline.push_str(" safemode");
        let status = assess(&f);
        assert_eq!(status.reason, Some(Reason::Requested));
        assert!(!status.repairable);
    }

    #[test]
    fn an_unparsable_reason_file_falls_back_to_the_command_line() {
        let mut f = facts(SAFEMODE_LINE);
        f.reason_file = Some("garbage\nmore\n".to_string());
        assert_eq!(assess(&f).reason, Some(Reason::Unrecorded));
        f.cmdline.push_str(" safemode");
        assert_eq!(assess(&f).reason, Some(Reason::Requested));
    }

    #[test]
    fn key1_wins_over_the_reason_file_but_the_file_still_gives_the_detail() {
        let mut f = facts(SAFEMODE_LINE);
        f.reason_file = Some("reason=mount_failed\nUBIFS error\n".to_string());
        f.cmdline.push_str(" safemode");
        let status = assess(&f);
        assert_eq!(status.reason, Some(Reason::Requested));
        assert_eq!(status.detail.as_deref(), Some("UBIFS error"));
        assert!(!status.repairable);
        assert!(status
            .repair_blocker
            .unwrap()
            .starts_with("Safemode was requested"));

        // Requested beats mount_failed, never the reverse: a file that says
        // requested with no key on the command line is still requested.
        let mut f = facts(SAFEMODE_LINE);
        f.reason_file = Some("reason=requested\n".to_string());
        assert_eq!(assess(&f).reason, Some(Reason::Requested));
    }

    #[test]
    fn a_safemode_assignment_on_the_command_line_blocks_the_repair() {
        let mut f = facts(SAFEMODE_LINE);
        f.reason_file = Some("reason=mount_failed\n".to_string());
        f.cmdline.push_str(" safemode=1");
        assert_eq!(assess(&f).reason, Some(Reason::Requested));
    }

    #[test]
    fn a_board_that_does_not_look_like_either_is_unknown() {
        let status = assess(&Facts::default());
        assert_eq!(status.state, State::Unknown);
        assert_eq!(status.reason, None);
        assert_eq!(status.backing, Backing::Unknown);
        assert!(!status.repairable);
    }

    // --- the repair decision table ---------------------------------------

    #[test]
    fn each_blocker_is_said_and_only_when_it_applies() {
        use Backing::{Mmc, Ubi};
        use Reason::{MountFailed, Requested, Unrecorded};
        use State::{Persistent, Temporary};
        let blocker = repair_blocker;

        // Everything holds.
        assert_eq!(
            blocker(Temporary, Some(MountFailed), Ubi, true, false),
            None
        );
        assert_eq!(blocker(Temporary, Some(Unrecorded), Ubi, true, false), None);

        // state
        assert!(blocker(Persistent, None, Ubi, true, false)
            .unwrap()
            .contains("nothing to repair"));
        assert!(blocker(State::Unknown, None, Ubi, true, false)
            .unwrap()
            .contains("could not tell whether"));

        // requested
        assert_eq!(
            blocker(Temporary, Some(Requested), Ubi, true, false).unwrap(),
            "Safemode was requested at power-on (KEY1). Reboot without holding KEY1 to use the \
             saved settings."
        );

        // backing
        assert_eq!(
            blocker(Temporary, Some(MountFailed), Mmc, true, false).unwrap(),
            "This board keeps its settings on the SD card; repair is only offered for the \
             built-in flash."
        );
        assert!(
            blocker(Temporary, Some(MountFailed), Backing::Unknown, true, false)
                .unwrap()
                .contains("could not tell where")
        );

        // no volume
        assert_eq!(
            blocker(Temporary, Some(MountFailed), Ubi, false, false).unwrap(),
            "There is no settings volume to remove; reboot the BMC to recreate it."
        );

        // mounted
        assert!(blocker(Temporary, Some(MountFailed), Ubi, true, true)
            .unwrap()
            .contains("/mnt/overlay"));

        // The first one that fails is the one said.
        assert!(blocker(Temporary, Some(Requested), Mmc, false, true)
            .unwrap()
            .starts_with("Safemode was requested"));
        assert!(blocker(Temporary, Some(MountFailed), Mmc, false, true)
            .unwrap()
            .starts_with("This board keeps"));
    }

    #[test]
    fn a_mounted_overlay_or_a_missing_volume_blocks_the_assessment() {
        let mut f = facts(SAFEMODE_LINE);
        f.ubi_volumes = vec!["rootfs".to_string()];
        let status = assess(&f);
        assert!(!status.repairable);
        assert!(status
            .repair_blocker
            .unwrap()
            .contains("no settings volume"));

        let mut f = facts(SAFEMODE_LINE);
        f.mounts = parse_mounts(&board_mounts(SAFEMODE_LINE, true));
        let status = assess(&f);
        assert!(!status.repairable);
        assert!(status.repair_blocker.unwrap().contains("in use"));
    }

    #[test]
    fn mmc_boards_are_not_repairable() {
        let mut f = facts(SAFEMODE_LINE);
        f.mounts = parse_mounts(
            "/dev/mmcblk0p2 /mnt/rom ext4 ro,relatime 0 0\n\
             overlay / overlay rw,upperdir=/tmp/safemode/upper,workdir=/tmp/safemode/work 0 0\n",
        );
        let status = assess(&f);
        assert_eq!(status.backing, Backing::Mmc);
        assert!(!status.repairable);
    }

    #[test]
    fn the_answer_has_exactly_the_published_fields() {
        let status = assess(&facts(SAFEMODE_LINE));
        let json = serde_json::to_value(&status).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "backing",
                "detail",
                "reason",
                "repair_blocker",
                "repairable",
                "state"
            ]
        );
        assert_eq!(json["state"], "temporary");
        assert_eq!(json["reason"], "unrecorded");
        assert_eq!(json["backing"], "ubi");
        assert!(json["detail"].is_null());
        assert!(json["repair_blocker"].is_null());

        let mut f = facts(SAFEMODE_LINE);
        f.reason_file = Some("reason=mount_failed\n".to_string());
        assert_eq!(
            serde_json::to_value(assess(&f)).unwrap()["reason"],
            "mount_failed"
        );
    }

    // --- the service, over files ----------------------------------------

    /// A board on disk: the files the service reads, and a recording `Exec`.
    pub struct Board {
        pub _dir: tempdir::TempDir,
        pub exec: Arc<FakeExec>,
        pub service: StorageService,
    }

    impl Board {
        pub fn new(root: &str, reason: Option<&str>, cmdline: &str, volumes: &[&str]) -> Board {
            let dir = tempdir::TempDir::new("storage-board").unwrap();
            std::fs::write(dir.path().join("mounts"), board_mounts(root, false)).unwrap();
            std::fs::write(dir.path().join("cmdline"), cmdline).unwrap();
            if let Some(reason) = reason {
                std::fs::write(dir.path().join("safemode.reason"), reason).unwrap();
            }
            for (i, name) in volumes.iter().enumerate() {
                let path = dir.path().join(format!("ubi/ubi0_{i}"));
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join("name"), format!("{name}\n")).unwrap();
            }
            let exec = FakeExec::board("", "");
            let service = StorageService::with(exec.clone(), StoragePaths::under(dir.path()));
            Board {
                _dir: dir,
                exec,
                service,
            }
        }

        /// The same files, run by another `Exec`.
        pub fn service_with(&self, exec: Arc<dyn Exec>) -> StorageService {
            StorageService::with(exec, StoragePaths::under(self._dir.path()))
        }

        /// A board whose settings volume would not mount.
        pub fn broken() -> Board {
            Board::new(
                SAFEMODE_LINE,
                Some("reason=mount_failed\nUBIFS error\n"),
                "console=ttyS0 ro",
                &["rootfs", "overlay"],
            )
        }
    }

    /// An `Exec` that takes its time, to stand in for a hung `ubirmvol`.
    pub struct SlowExec {
        pub delay: Duration,
        pub calls: std::sync::Mutex<Vec<String>>,
    }

    impl SlowExec {
        pub fn new(delay: Duration) -> Arc<Self> {
            Arc::new(SlowExec {
                delay,
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    impl Exec for SlowExec {
        fn run(&self, program: &str, args: &[&str]) -> Result<String, ApplyError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            std::thread::sleep(self.delay);
            Ok(String::new())
        }
    }

    fn guard_is_taken(service: &StorageService) -> bool {
        service.repairing.load(Ordering::SeqCst)
    }

    #[test]
    fn a_refused_repair_never_takes_the_guard() {
        // Healthy, KEY1, no volume: each refused before the flag is touched,
        // so a later repair of a board that does need one is not locked out.
        for board in [
            Board::new(PERSISTENT_LINE, None, "ro", &["rootfs", "overlay"]),
            Board::new(SAFEMODE_LINE, None, "ro safemode", &["rootfs", "overlay"]),
            Board::new(SAFEMODE_LINE, None, "ro", &["rootfs"]),
        ] {
            assert!(matches!(
                board.service.repair(),
                Err(RepairError::Blocked(_))
            ));
            assert!(!guard_is_taken(&board.service));
            assert!(board.exec.changes().is_empty());
        }
    }

    #[test]
    fn a_failed_removal_gives_the_guard_back_and_a_successful_one_keeps_it() {
        let board = Board::broken();
        *board.exec.fail_prefix.lock().unwrap() = Some("ubirmvol".to_string());
        assert!(board.service.repair().is_err());
        assert!(!guard_is_taken(&board.service));
        board.exec.heal();
        board.service.repair().unwrap();
        assert!(guard_is_taken(&board.service));
    }

    #[actix_web::test]
    async fn a_hung_ubirmvol_times_out_and_gives_the_guard_back() {
        let board = Board::broken();
        let exec = SlowExec::new(Duration::from_millis(400));
        let service = board
            .service_with(exec.clone())
            .with_timeout(Duration::from_millis(50));
        let Err(RepairError::Timeout(_)) = service.repair_async().await else {
            panic!("ubirmvol hung");
        };
        assert!(!guard_is_taken(&service));
        assert_eq!(exec.calls.lock().unwrap().len(), 1);
        let message = RepairError::Timeout(Duration::from_secs(60)).to_string();
        assert!(message.contains("within 60s"), "{message}");
    }

    #[actix_web::test]
    async fn a_successful_async_repair_keeps_the_guard() {
        let board = Board::broken();
        board.service.repair_async().await.unwrap();
        assert!(guard_is_taken(&board.service));
    }

    #[test]
    fn the_service_reads_the_files() {
        let board = Board::broken();
        let status = board.service.status();
        assert_eq!(status.reason, Some(Reason::MountFailed));
        assert_eq!(status.detail.as_deref(), Some("UBIFS error"));
        assert!(status.repairable);
        // Reading runs nothing.
        assert!(board.exec.changes().is_empty());
    }

    #[test]
    fn a_service_with_no_files_does_not_panic() {
        let dir = tempdir::TempDir::new("storage-none").unwrap();
        let service = StorageService::with(
            FakeExec::board("", ""),
            StoragePaths::under(&dir.path().join("nothing")),
        );
        let status = service.status();
        assert_eq!(status.state, State::Unknown);
    }

    #[test]
    fn repair_removes_the_volume_with_an_argv_and_no_shell() {
        let board = Board::broken();
        board.service.repair().expect("repairable");
        assert_eq!(
            board.exec.changes(),
            vec!["ubirmvol /dev/ubi0 -N overlay".to_string()]
        );
    }

    #[test]
    fn repair_is_decided_again_and_refused_when_not_repairable() {
        let board = Board::new(
            SAFEMODE_LINE,
            Some("reason=requested\n"),
            "console=ttyS0 safemode",
            &["rootfs", "overlay"],
        );
        let Err(RepairError::Blocked(why)) = board.service.repair() else {
            panic!("requested safemode is not repairable");
        };
        assert!(why.starts_with("Safemode was requested"), "{why}");
        assert!(board.exec.changes().is_empty());

        let healthy = Board::new(PERSISTENT_LINE, None, "ro", &["rootfs", "overlay"]);
        assert!(matches!(
            healthy.service.repair(),
            Err(RepairError::Blocked(_))
        ));
        assert!(healthy.exec.changes().is_empty());
    }

    #[test]
    fn a_failed_ubirmvol_is_reported_and_can_be_tried_again() {
        let board = Board::broken();
        *board.exec.fail_prefix.lock().unwrap() = Some("ubirmvol".to_string());
        let Err(RepairError::Command(e)) = board.service.repair() else {
            panic!("ubirmvol failed");
        };
        assert!(
            e.to_string()
                .starts_with("`ubirmvol /dev/ubi0 -N overlay` failed"),
            "{e}"
        );
        board.exec.heal();
        board.service.repair().expect("a second try is allowed");
    }

    #[test]
    fn a_second_repair_after_a_successful_one_is_refused() {
        let board = Board::broken();
        board.service.repair().unwrap();
        let Err(RepairError::Blocked(why)) = board.service.repair() else {
            panic!("already repairing");
        };
        assert!(why.contains("already under way"), "{why}");
        assert_eq!(board.exec.changes().len(), 1);
    }
}
