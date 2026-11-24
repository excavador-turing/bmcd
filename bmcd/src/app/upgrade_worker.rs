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
use crate::app::bmc_application::BmcApplication;
use crate::app::staging_lock::StageLock;
use crate::hal::{NodeId, UsbRoute};
use crate::streaming_data_service::data_transfer::DataTransfer;
use crate::utils::WriteMonitor;
use anyhow::{bail, Context};
use crc::{Crc, CRC_64_REDIS};
use humansize::{format_size, DECIMAL};
use nix::sys::statvfs::{statvfs, FsFlags};
use std::io::{Error, ErrorKind};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio::fs::OpenOptions;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::BufStream;
use tokio::io::{sink, AsyncRead};
use tokio::sync::watch;
use tokio::task::spawn_blocking;
use tokio::{
    fs,
    io::{self, AsyncWrite, AsyncWriteExt},
};
use tokio_util::sync::CancellationToken;

/// Directories an uploaded firmware image can be staged in, most preferred
/// first:
///
/// * `/mnt/sdcard` holds tens of gigabytes when a card is inserted and costs no
///   NAND wear.
/// * `/mnt/overlay` is the UBIFS overlay of about 150MB. It is always mounted,
///   and is what is left when there is no SD card.
/// * `/tmp` is a tmpfs, which is RAM. The BMC has 116MB of it in total and the
///   tmpfs is capped at 58MB, so staging a 38MiB image here got the daemon
///   killed halfway through the copy. It stays last in the list because it is
///   the only writable place left when nothing else is mounted.
const UPGRADE_STAGING_DIRS: [&str; 3] = ["/mnt/sdcard", "/mnt/overlay", "/tmp"];

/// Widens a `statvfs` count to `u64`.
///
/// These counts are `u64` on x86-64 and **`u32` on this board's 32-bit ARM**.
/// Multiplying two of them directly builds on a workstation, fails to
/// cross-compile for the board, and would overflow at 4 GB if it did.
///
/// Spelling the widening at the call site does not work either: `u64::from`
/// and `as u64` are both correct for the target and both trip clippy on the
/// host, where the types already match. A generic conversion is right on both
/// platforms and carries the reason with it, which a lint suppression would
/// not.
fn widen<T: Into<u64>>(value: T) -> u64 {
    value.into()
}

/// Whether a node is switched back on when its flash is over: only when it was
/// on before, and only when the flash succeeded.
fn switch_back_on(was_on: bool, flashed: bool) -> bool {
    was_on && flashed
}

/// Where a parked image goes, and where the catalogue's `local` source reads.
/// The two must agree: an image parked anywhere else is invisible.
const PARK_MOUNT: &str = "/mnt/sdcard";
const PARK_DIR: &str = "/mnt/sdcard/firmware";
/// Directory created under the chosen staging location, removed again when
/// `osupdate` returns.
pub(crate) const UPGRADE_DIR_NAME: &str = "os_upgrade";
/// The staged-version note, written into the staging directory and handed to
/// `osupdate`, which installs it before it arms `nextboot`. The name holds a
/// `~`, which an image name cannot (`firmware_image_name`), so the two can
/// never be the same file.
const STAGED_NOTE_FILE: &str = "~staged-firmware";
const BLOCK_WRITE_SIZE: usize = BLOCK_READ_SIZE; // 512Kib
const BLOCK_READ_SIZE: usize = 524288; // 512Kib

// Contains collection of functions that execute some business flow in relation
// to file transfers in the BMC. See `flash_node` and `os_update`.
pub struct UpgradeWorker {
    do_crc_validation: bool,
    data_transfer: DataTransfer,
    cancel: CancellationToken,
    written_sender: watch::Sender<u64>,
}

impl UpgradeWorker {
    pub fn new(
        do_crc_validation: bool,
        data_transfer: DataTransfer,
        cancel: CancellationToken,
        written_sender: watch::Sender<u64>,
    ) -> Self {
        Self {
            do_crc_validation,
            data_transfer,
            cancel,
            written_sender,
        }
    }

    /// Logic to program a given OS image to a node. Uses a [`DataTransfer`]
    /// abstraction as source of the image data. The transfer can be interrupted
    /// at any time when the `CancellationToken` is cancelled.
    ///
    /// The node is switched off to flash it. Whatever happens, the USB mode is
    /// put back as it was. A node that was on before the flash is switched back
    /// on after a successful one, so it boots the new image; a node that was off
    /// stays off. After a failed or cancelled flash the node is left off, so a
    /// half-written image does not boot.
    pub async fn flash_node(
        mut self,
        bmc: Arc<BmcApplication>,
        node: NodeId,
    ) -> anyhow::Result<()> {
        // Read before anything switches the node off: afterwards the stored
        // state says what the flash did to it, not what it was doing.
        let was_on = bmc
            .get_node_power(node)
            .await
            .with_context(|| format!("reading the power state of {node} before the flash"))?;

        let device = match bmc.node_in_flash(node, UsbRoute::Bmc, &self.cancel).await {
            Ok(device) => device,
            Err(e) => {
                // The node was powered and routed for flashing before the
                // device failed to show up: put it back, and never let a
                // failure of the cleanup hide why the flash failed.
                tracing::warn!("flashing {node} did not start ({e:#}), leaving it off");
                if let Err(c) = Self::restore(&bmc, node, false).await {
                    tracing::error!("restoring node {node} after a failed start: {c:#}");
                }
                return Err(e);
            }
        };

        let result = async move {
            let reader = self.data_transfer.reader().await?;
            let mut buf_stream =
                BufStream::with_capacity(BLOCK_READ_SIZE, BLOCK_WRITE_SIZE, device);
            let (bytes_written, written_crc) =
                self.try_write_node(node, reader, &mut buf_stream).await?;

            if self.do_crc_validation {
                buf_stream.seek(std::io::SeekFrom::Start(0)).await?;
                flush_file_caches().await?;
                self.try_validate_crc(node, written_crc, buf_stream.take(bytes_written))
                    .await?;
            } else {
                tracing::info!("user skipped crc check");
            }

            Ok::<(), anyhow::Error>(())
        }
        .await;

        let switch_on = switch_back_on(was_on, result.is_ok());
        match &result {
            Ok(()) => tracing::info!("Flashing {node} successful, restoring USB & power settings."),
            Err(e) => {
                tracing::warn!("flashing {node} failed ({e:#}), restoring USB, leaving it off")
            }
        }

        // disregarding the result, set the BMC in the finalized state.
        let switched_on = Self::restore(&bmc, node, switch_on).await?;
        match (switched_on, switch_on, result.is_ok()) {
            (true, _, _) => tracing::info!("{node} switched back on: it was on before the flash"),
            (false, true, _) => tracing::warn!(
                "{node} left off: its power was set while the flash was being wrapped up"
            ),
            (false, false, true) => tracing::info!("{node} left off: it was off before the flash"),
            (false, false, false) => tracing::warn!("{node} left off: the flash did not succeed"),
        }
        result
    }

    /// Switches the node off and puts USB back the way it was. When
    /// `switch_on`, the node is then held off for the settle time and switched
    /// on again with the USB boot pin released, so it boots from what it was
    /// flashed with; a power request for the node in the meantime is the last
    /// word.
    ///
    /// # Returns
    ///
    /// Whether the node was switched on.
    async fn restore(bmc: &BmcApplication, node: NodeId, switch_on: bool) -> anyhow::Result<bool> {
        let ticket = bmc.power_off_for_cycle(node).await?;
        bmc.usb_boot(node, false).await?;
        let (mode, _) = bmc.get_usb_mode().await?;
        bmc.configure_usb(mode).await?;
        if switch_on {
            return bmc.power_on_after_settle(ticket).await;
        }
        Ok(false)
    }

    async fn try_write_node(
        &mut self,
        node: NodeId,
        source_reader: impl AsyncRead + 'static + Unpin,
        mut node_writer: &mut (impl AsyncWrite + 'static + Unpin),
    ) -> anyhow::Result<(u64, u64)> {
        tracing::info!("started writing to {node}");

        let crc = Crc::<u64>::new(&CRC_64_REDIS);
        let mut write_watcher = WriteMonitor::new(&mut node_writer, &mut self.written_sender, &crc);

        let bytes_written = copy_or_cancel(source_reader, &mut write_watcher, &self.cancel).await?;
        let crc = write_watcher.crc();

        tracing::info!(
            "Wrote {}, crc: {}",
            format_size(bytes_written, DECIMAL),
            crc
        );

        Ok((bytes_written, crc))
    }

    async fn try_validate_crc(
        &mut self,
        node: NodeId,
        expected_crc: u64,
        node_reader: impl AsyncRead + 'static + Unpin,
    ) -> anyhow::Result<()> {
        tracing::info!("Verifying checksum of data on node {node}");

        let crc = Crc::<u64>::new(&CRC_64_REDIS);
        let mut sink = WriteMonitor::new(sink(), &mut self.written_sender, &crc);
        copy_or_cancel(node_reader, &mut sink, &self.cancel).await?;
        let dev_checksum = sink.crc();

        if expected_crc != dev_checksum {
            bail!(
                "crc error. expected {}, calculated {}",
                expected_crc,
                dev_checksum
            );
        }

        Ok(())
    }

    /// Writes an image to the SD card and stops.
    ///
    /// `os_update` stages into a scratch directory, runs `osupdate` on it at
    /// once, and deletes the directory -- so an uploaded image is never a
    /// thing you have, only a thing that happened. That made the browser's
    /// upload control a second way to install, bypassing the catalogue: an
    /// operator could upload one image and install another with the interface
    /// never showing which.
    ///
    /// Parking puts the file where the catalogue's `local` source reads, so
    /// an upload ends in the same list as every other candidate and installing
    /// is a separate, visible choice.
    pub async fn os_park(mut self) -> anyhow::Result<()> {
        let file_name = self.data_transfer.file_name()?.to_owned();
        require_image_name(&file_name)?;
        let image_size = self.data_transfer.size()?;
        let source = self.data_transfer.reader().await?;
        tracing::info!("parking firmware image {}", file_name.to_string_lossy());

        let dir = std::path::Path::new(PARK_DIR);
        // Only the card. The overlay is 138 MB and holds the settings, the
        // metrics token and the promotion log; a 37 MB image parked there
        // twice is how it fills, and the failure would land on the next
        // update rather than here.
        match is_mount_point(std::path::Path::new(PARK_MOUNT)) {
            Ok(true) => {}
            Ok(false) if card_inserted() => bail!(
                "the image was not saved: an SD card is inserted but not mounted (unsupported or damaged filesystem?). Install it straight away with `tpi firmware upload` without `--park` (the web interface's install option does the same)."
            ),
            Ok(false) => bail!(
                "the image was not saved: no SD card is mounted at {PARK_MOUNT}. Insert a card and try again, or install it straight away with `tpi firmware upload` without `--park` (the web interface's install option does the same)."
            ),
            Err(e) => bail!("the image was not saved: cannot tell whether {PARK_MOUNT} is mounted: {e}"),
        }
        match statvfs(std::path::Path::new(PARK_MOUNT)) {
            Ok(stat) => {
                let free = widen(stat.blocks_available()) * widen(stat.fragment_size());
                if free < image_size {
                    bail!(
                        "the card has {} free and the image is {}",
                        format_size(free, DECIMAL),
                        format_size(image_size, DECIMAL)
                    );
                }
            }
            Err(e) => bail!("cannot check the space on {PARK_MOUNT}: {e}"),
        }

        tokio::fs::create_dir_all(dir).await?;

        // Write beside the target and rename, so a transfer that is cancelled
        // or fails halfway cannot leave a truncated image in the directory the
        // catalogue lists. A partial file there would be offered for install.
        let final_path = dir.join(&file_name);
        let partial_path = dir.join(format!("{}.partial", file_name.to_string_lossy()));

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&partial_path)
            .await?;

        let crc = Crc::<u64>::new(&CRC_64_REDIS);
        let mut writer = WriteMonitor::new(&mut file, &mut self.written_sender, &crc);
        if let Err(e) = copy_or_cancel(source, &mut writer, &self.cancel).await {
            // A truncated image left in the directory the catalogue reads
            // would be offered for install.
            let _ = tokio::fs::remove_file(&partial_path).await;
            return Err(e.into());
        }
        file.sync_all().await?;
        drop(file);

        tokio::fs::rename(&partial_path, &final_path).await?;
        tracing::info!("parked {}", final_path.to_string_lossy());
        // The catalogue's `local` source was read before this file existed, and
        // would not read it again for up to half an hour.
        crate::app::firmware_catalog::invalidate().await;
        Ok(())
    }

    /// Writes the uploaded image into the spare volume and arms it.
    ///
    /// `lock` is the staging lock, taken by the request that started this
    /// transfer and held until this function returns: the upload, the write
    /// and the note are one staging, and nothing else may stage meanwhile
    /// (BMC-Firmware#127). It is moved in rather than taken here because the
    /// refusal has to reach the client that asked, as a 409, before any bytes
    /// are sent.
    pub async fn os_update(mut self, lock: StageLock) -> anyhow::Result<()> {
        let file_name = self.data_transfer.file_name()?.to_owned();
        require_image_name(&file_name)?;
        // Read the size before the reader is taken: on a URL transfer it is the
        // content-length of a response that `reader()` consumes.
        let image_size = self.data_transfer.size()?;
        let source = self.data_transfer.reader().await?;
        tracing::info!("start firmware upgrade {}", file_name.to_string_lossy());

        let staging_dir = select_staging_dir(image_size);
        let mut os_update_img = staging_dir.clone();
        os_update_img.push(&file_name);

        tokio::fs::create_dir_all(&staging_dir).await?;
        // Removed on every exit, including a cancelled copy. The delete used
        // to run only after `spawn_blocking` returned, so a failed copy left
        // the image in the staging directory.
        let _staging = RemoveStagingDir(staging_dir.clone());

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&os_update_img)
            .await?;

        let crc = Crc::<u64>::new(&CRC_64_REDIS);
        let mut writer = WriteMonitor::new(&mut file, &mut self.written_sender, &crc);
        copy_or_cancel(source, &mut writer, &self.cancel).await?;

        // Record WHICH image is being staged, so the web interface can name the
        // pending version instead of only reporting that something is pending.
        // `tpi-selfupdate` writes the same note for the path it owns; this is
        // the upload path.
        //
        // Written BEFORE `osupdate` runs and handed to it: `osupdate` installs
        // it, syncs it and only then arms `nextboot`. It used to be written
        // here after `osupdate` returned, and a reboot or a power cut between
        // the two armed an image whose note did not exist, which the promotion
        // gate then judged without its version check (BMC-Firmware#126). Never
        // fatal: without a note `osupdate` writes a minimal one of its own.
        let note = staging_dir.join(STAGED_NOTE_FILE);
        let note = match write_staged_note(&note, &file_name.to_string_lossy()).await {
            Ok(()) => Some(note),
            Err(e) => {
                tracing::warn!("could not record the staged version: {e}");
                None
            }
        };

        let mut command = osupdate_command(&os_update_img, note.as_deref());
        // This process holds the staging lock for the whole of the transfer;
        // `osupdate` must not try to take it from underneath its own parent.
        lock.mark_held(&mut command);
        let result = spawn_blocking(move || command.status()).await?;

        let success = result?;
        if !success.success() {
            bail!("failed firmware upgrade ({})", success);
        }

        // `lock` is a parameter, so it is dropped after every local, the
        // `RemoveStagingDir` guard included: the directory every upload shares
        // is gone before the next stager may create it again. The first
        // transfer to finish used to delete the second's image from under it.
        Ok(())
    }
}

/// Deletes the firmware staging directory when the update ends, however it ends.
struct RemoveStagingDir(PathBuf);

impl Drop for RemoveStagingDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("could not remove {}: {e}", self.0.display());
            }
        }
    }
}

/// The release filename shape this fork publishes, used to recover a tag from
/// an uploaded file.
const OTA_PREFIX: &str = "tp2-bmc-firmware-ota-";
const OTA_SUFFIX: &str = ".tpu";

/// A firmware image name is one path component of `[A-Za-z0-9._-]`.
///
/// `.` and `..` match that class and still leave the staging directory, so
/// they are refused. Anything else is refused before a file is created:
/// `Path::join` of an absolute name replaces the directory, and a shell
/// metacharacter must never reach `osupdate`.
pub(crate) fn firmware_image_name(name: &str) -> Result<&str, &'static str> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("firmware file name must be one component matching [A-Za-z0-9._-]");
    }
    Ok(name)
}

fn require_image_name(name: &std::ffi::OsStr) -> anyhow::Result<()> {
    let Some(name) = name.to_str() else {
        bail!("firmware file name must be one component matching [A-Za-z0-9._-]");
    };
    firmware_image_name(name).map_err(anyhow::Error::msg)?;
    Ok(())
}

/// `osupdate` takes the image path as an argument, and optionally the note it
/// is to install as the staged-version note. Neither is ever interpolated into
/// a shell command.
fn osupdate_command(image: &Path, note: Option<&Path>) -> Command {
    let mut command = Command::new("osupdate");
    command.arg(image);
    if let Some(note) = note {
        command.arg(note);
    }
    command
}

/// Recovers the release tag from a published OTA filename, when the name
/// follows the shape this fork publishes: `tp2-bmc-firmware-ota-v2.3.0.tpu`.
///
/// Returns `None` for anything else rather than guessing. A hand-built or
/// renamed image genuinely has no tag to report, and a wrong version on the
/// firmware page is worse than an absent one.
pub(crate) fn tag_from_ota_name(file_name: &str) -> Option<&str> {
    let tag = file_name
        .strip_prefix(OTA_PREFIX)?
        .strip_suffix(OTA_SUFFIX)?;
    (!tag.is_empty()).then_some(tag)
}

/// Writes the staged-image note to `path`. `KEY=VALUE`, the same shape the
/// firmware's `tpi-selfupdate` writes and `firmware_info` reads.
///
/// This is only the file `osupdate` is handed; `osupdate` installs it on the
/// overlay (where the gate and `firmware_info` read it) before it arms
/// `nextboot`, and adds the mark that says the gateway note came first.
async fn write_staged_note(path: &Path, file_name: &str) -> std::io::Result<()> {
    let running = crate::api::legacy::firmware_version().await;
    tokio::fs::write(path, staged_note(file_name, running.as_deref())).await
}

/// The body of the note. Split from the write so it can be tested without a
/// board.
fn staged_note(file_name: &str, running: Option<&str>) -> String {
    let mut note = String::new();
    if let Some(tag) = tag_from_ota_name(file_name) {
        note.push_str(&format!("VERSION={tag}\n"));
    }
    note.push_str(&format!("FILE={file_name}\n"));
    note.push_str(&format!(
        "STAGED_AT={}\n",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    ));
    note.push_str("SOURCE=upload\n");
    // What this image is about to replace.
    //
    // Only the stager knows it: by the time the gate runs it is the *new*
    // image reading `/etc/os-release`, and the volume the old one sits in is
    // not mounted, so its version cannot be read from a running board. That
    // is why the interface said "Rollback: version not readable" -- honest,
    // and useless to somebody deciding whether to press Reboot. Recording it
    // here lets the gate carry it forward on promotion.
    if let Some(running) = running {
        note.push_str(&format!("REPLACES={running}\n"));
    }
    note
}

/// Selects the directory to stage a firmware image in. Returns the first entry
/// of [`UPGRADE_STAGING_DIRS`] that is a mount point of its own, is mounted
/// read-write, and has room for `image_size` bytes. When none of them
/// qualifies it falls back to `/tmp`, which is where every image was staged
/// before this function existed.
fn select_staging_dir(image_size: u64) -> PathBuf {
    for candidate in UPGRADE_STAGING_DIRS {
        let path = Path::new(candidate);

        if !is_mounted_or_false(path) {
            tracing::debug!("staging: {} is not a mount point", candidate);
            continue;
        }

        let stat = match statvfs(path) {
            Ok(stat) => stat,
            Err(e) => {
                tracing::debug!("staging: cannot stat {}: {}", candidate, e);
                continue;
            }
        };

        if stat.flags().contains(FsFlags::ST_RDONLY) {
            tracing::debug!("staging: {} is mounted read-only", candidate);
            continue;
        }

        let free = stat.blocks_free() as u64 * stat.fragment_size() as u64;
        if free < image_size {
            tracing::debug!(
                "staging: {} has {} free, the image needs {}",
                candidate,
                format_size(free, DECIMAL),
                format_size(image_size, DECIMAL)
            );
            continue;
        }

        tracing::info!(
            "staging firmware image in {}/{}: first of {:?} that is a writable mount with room for it ({} free, image is {})",
            candidate,
            UPGRADE_DIR_NAME,
            UPGRADE_STAGING_DIRS,
            format_size(free, DECIMAL),
            format_size(image_size, DECIMAL)
        );
        return path.join(UPGRADE_DIR_NAME);
    }

    tracing::info!(
        "staging firmware image in /tmp/{}: none of {:?} is a writable mount with room for {}, falling back to the RAM disk",
        UPGRADE_DIR_NAME,
        UPGRADE_STAGING_DIRS,
        format_size(image_size, DECIMAL)
    );
    PathBuf::from("/tmp").join(UPGRADE_DIR_NAME)
}

/// True when `path` is the root of a mount, i.e. its device id differs from the
/// one of the directory it sits in. Both `/mnt/sdcard` and `/mnt/overlay` exist
/// as empty directories when nothing is mounted on them, and writing an image
/// there would land on the small root filesystem instead.
///
/// A path that is not there is not a mount point (`Ok(false)`). Anything else
/// that stops it being read is an `Err`, not a "no": a caller that treated an
/// I/O error as "no card" would tell the operator to insert one.
pub(crate) fn is_mount_point(path: &Path) -> std::io::Result<bool> {
    let Some(parent) = path.parent() else {
        // `/` has no parent and is always a mount point.
        return Ok(true);
    };

    match (path.metadata(), parent.metadata()) {
        (Ok(dir), Ok(parent)) => Ok(dir.dev() != parent.dev()),
        (Err(e), _) | (_, Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
}

/// [`is_mount_point`] for a caller that can only act on a yes or a no: an error
/// is logged and counts as "not a mount point".
pub(crate) fn is_mounted_or_false(path: &Path) -> bool {
    is_mount_point(path).unwrap_or_else(|e| {
        tracing::warn!(
            "cannot tell whether {} is a mount point: {e}",
            path.display()
        );
        false
    })
}

/// The SD slot's block device. The kernel creates it when it sees a card, so
/// it is there for a card that nothing has mounted -- an unsupported or damaged
/// filesystem -- and absent for an empty slot.
pub(crate) const CARD_DEVICE: &str = "/dev/mmcblk0";

pub(crate) fn card_inserted() -> bool {
    Path::new(CARD_DEVICE).exists()
}

/// Copies bytes from `reader` to `writer` until the reader is exhausted. This function
/// returns an `io::Error(Interrupted)` in case a cancel was issued.
async fn copy_or_cancel<L, W>(
    mut reader: L,
    mut writer: &mut W,
    cancel: &CancellationToken,
) -> std::io::Result<u64>
where
    L: AsyncRead + std::marker::Unpin,
    W: AsyncWrite + std::marker::Unpin,
{
    let copy_task = tokio::io::copy(&mut reader, &mut writer);
    let cancel = cancel.cancelled();

    let bytes_copied: u64;
    tokio::select! {
        res = copy_task =>  bytes_copied = res?,
        _ = cancel => return Err(Error::from(ErrorKind::Interrupted)),
    };

    tracing::debug!("copied {} bytes", bytes_copied);
    writer.flush().await?;
    Ok(bytes_copied)
}

async fn flush_file_caches() -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open("/proc/sys/vm/drop_caches")
        .await?;

    // Free reclaimable slab objects and page cache
    file.write_u8(b'3').await
}

#[cfg(test)]
mod test {

    use super::*;
    use rand::RngCore;
    use tempfile::TempDir;
    use tokio::io::BufWriter;

    fn random_array<const SIZE: usize>() -> Vec<u8> {
        let mut array = vec![0; SIZE];
        rand::rng().fill_bytes(&mut array);
        array
    }

    #[test]
    fn mount_point_detection() {
        assert!(is_mount_point(Path::new("/")).unwrap());
        // a directory inside a mount shares the device id of its parent.
        let dir = TempDir::with_prefix("staging_test").unwrap();
        assert!(!is_mount_point(dir.path()).unwrap());
        // a directory that is not there cannot be written to either.
        assert_eq!(is_mount_point(&dir.path().join("absent")).ok(), Some(false));
    }

    #[test]
    fn staging_dir_falls_back_to_tmp() {
        // no filesystem has room for this, so every candidate is skipped and
        // the historic location is returned.
        assert_eq!(
            select_staging_dir(u64::MAX),
            PathBuf::from("/tmp/os_upgrade")
        );
    }

    #[tokio::test]
    async fn crc_reader_test() {
        let crc = Crc::<u64>::new(&CRC_64_REDIS);
        let buffer = random_array::<{ 10024 * 1024 }>();
        let expected_crc = crc.checksum(&buffer);

        let mut buf_writer = BufWriter::new(Vec::new());
        let cursor = std::io::Cursor::new(&buffer);

        let (mut sender, mut receiver) = watch::channel(0u64);
        let mut write_watcher = WriteMonitor::new(&mut buf_writer, &mut sender, &crc);
        copy_or_cancel(cursor, &mut write_watcher, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(expected_crc, write_watcher.crc());
        assert_eq!(&buffer, buf_writer.get_ref());
        assert_eq!(*receiver.borrow_and_update(), buffer.len() as u64);
    }

    #[test]
    fn only_a_node_that_was_on_is_switched_back_on_and_only_after_a_good_flash() {
        assert!(switch_back_on(true, true));
        assert!(!switch_back_on(true, false), "a failed flash leaves it off");
        assert!(!switch_back_on(false, true), "an idle node stays off");
        assert!(!switch_back_on(false, false));
    }

    #[test]
    fn staging_dir_is_removed_when_the_guard_drops() {
        let parent = TempDir::with_prefix("staging_parent").unwrap();
        let dir = parent.path().join("image");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("a.tpu"), b"x").unwrap();
        {
            let _guard = RemoveStagingDir(dir.clone());
            assert!(dir.exists());
        }
        assert!(!dir.exists());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_image_name_is_one_safe_component() {
        assert_eq!(
            firmware_image_name("tp2-bmc-firmware-ota-v2.52.0.tpu"),
            Ok("tp2-bmc-firmware-ota-v2.52.0.tpu")
        );
        for bad in [
            "",
            ".",
            "..",
            "/tmp/bmcd-traversal-test",
            "a$(id)",
            "a;id",
            "dir/img.tpu",
            "a b",
        ] {
            assert!(firmware_image_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn osupdate_is_not_a_shell() {
        let image = PathBuf::from("/tmp/os_upgrade/firmware.tpu");
        let command = osupdate_command(&image, None);
        assert_eq!(command.get_program(), "osupdate");
        assert_eq!(command.get_args().collect::<Vec<_>>(), [image.as_os_str()]);
    }

    /// The note is the second argument, as `osupdate IMAGE [NOTE]` takes it.
    #[test]
    fn osupdate_is_handed_the_note_to_install() {
        let image = PathBuf::from("/tmp/os_upgrade/firmware.tpu");
        let note = PathBuf::from("/tmp/os_upgrade/~staged-firmware");
        let command = osupdate_command(&image, Some(&note));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [image.as_os_str(), note.as_os_str()]
        );
    }

    /// The note file shares a directory with the image, so its name must be one
    /// no image can have.
    #[test]
    fn the_note_file_name_cannot_be_an_image_name() {
        assert!(firmware_image_name(STAGED_NOTE_FILE).is_err());
    }

    #[test]
    fn the_staged_note_names_the_tag_the_file_and_what_it_replaces() {
        let note = staged_note("tp2-bmc-firmware-ota-v2.57.0.tpu", Some("v2.55.0"));
        let lines: Vec<&str> = note.lines().collect();
        assert!(lines.contains(&"VERSION=v2.57.0"), "{note}");
        assert!(
            lines.contains(&"FILE=tp2-bmc-firmware-ota-v2.57.0.tpu"),
            "{note}"
        );
        assert!(lines.contains(&"SOURCE=upload"), "{note}");
        assert!(lines.contains(&"REPLACES=v2.55.0"), "{note}");
        assert!(lines.iter().any(|l| l.starts_with("STAGED_AT=")), "{note}");
    }

    /// An image that is not named like a release has a file and no version,
    /// and saying so beats inventing one.
    #[test]
    fn a_hand_built_image_has_a_file_and_no_version() {
        let note = staged_note("my-build.tpu", None);
        assert!(!note.contains("VERSION="), "{note}");
        assert!(!note.contains("REPLACES="), "{note}");
        assert!(note.contains("FILE=my-build.tpu\n"), "{note}");
    }

    #[test]
    fn a_published_ota_name_yields_its_tag() {
        assert_eq!(
            tag_from_ota_name("tp2-bmc-firmware-ota-v2.3.0.tpu"),
            Some("v2.3.0")
        );
        assert_eq!(
            tag_from_ota_name("tp2-bmc-firmware-ota-v2.2.0-unstable-hive.12.tpu"),
            Some("v2.2.0-unstable-hive.12")
        );
    }

    /// A hand-built or renamed image has no tag to report, and inventing one
    /// would put a wrong version on the firmware page -- worse than none.
    #[test]
    fn anything_else_yields_no_tag() {
        assert_eq!(tag_from_ota_name("firmware.tpu"), None);
        assert_eq!(tag_from_ota_name("tp2-bmc-firmware-ota-v2.3.0.img"), None);
        assert_eq!(tag_from_ota_name("tp2-bmc-firmware-ota-.tpu"), None);
        assert_eq!(tag_from_ota_name(""), None);
    }
}

/// What the end of a flash does to the node, on the simulated board.
#[cfg(all(test, feature = "stubbed"))]
mod restore_tests {
    use super::*;
    use crate::app::bmc_application::UsbConfig;
    use std::time::Duration;
    use tempfile::TempDir;

    const SETTLE: Duration = Duration::from_millis(300);

    /// A board whose node 2 is in the state a flash leaves it in: powered, with
    /// the USB boot pin high, and the USB mode stored from before the flash.
    async fn flashed_board() -> (Arc<BmcApplication>, TempDir) {
        let dir = TempDir::with_prefix("bmcd-restore").unwrap();
        let bmc = Arc::new(BmcApplication::for_test(dir.path(), SETTLE).await);
        bmc.configure_usb(UsbConfig::UsbA(NodeId::Node1))
            .await
            .unwrap();
        bmc.activate_slot(0b1111, 0b1111).await.unwrap();
        bmc.usb_boot(NodeId::Node2, true).await.unwrap();
        (bmc, dir)
    }

    #[tokio::test]
    async fn a_node_that_was_on_is_switched_back_on() {
        let (bmc, _dir) = flashed_board().await;

        let on = UpgradeWorker::restore(&bmc, NodeId::Node2, true)
            .await
            .unwrap();

        assert!(on);
        assert_eq!(bmc.rails(), 0b1111);
        assert!(bmc.get_node_power(NodeId::Node2).await.unwrap());
        assert_eq!(
            bmc.pin_controller.simulated_usb_boot(),
            0,
            "boot pin released"
        );
        assert_eq!(
            bmc.get_usb_mode().await.unwrap().0,
            UsbConfig::UsbA(NodeId::Node1),
            "USB is back the way it was"
        );
    }

    #[tokio::test]
    async fn a_node_that_was_off_or_failed_stays_off_with_usb_restored() {
        let (bmc, _dir) = flashed_board().await;

        let on = UpgradeWorker::restore(&bmc, NodeId::Node2, false)
            .await
            .unwrap();

        assert!(!on);
        assert_eq!(bmc.rails(), 0b1101, "node 2 off, the others untouched");
        assert!(!bmc.get_node_power(NodeId::Node2).await.unwrap());
        assert_eq!(bmc.pin_controller.simulated_usb_boot(), 0);
        assert_eq!(
            bmc.get_usb_mode().await.unwrap().0,
            UsbConfig::UsbA(NodeId::Node1)
        );
    }

    #[tokio::test]
    async fn a_power_off_while_the_flash_is_wrapped_up_wins() {
        let (bmc, _dir) = flashed_board().await;

        let restore = tokio::spawn({
            let bmc = bmc.clone();
            async move { UpgradeWorker::restore(&bmc, NodeId::Node2, true).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        bmc.activate_slot(0b0000, 0b0010).await.unwrap();

        assert!(!restore.await.unwrap().unwrap());
        assert_eq!(bmc.rails(), 0b1101);
        assert!(!bmc.get_node_power(NodeId::Node2).await.unwrap());
    }
}
