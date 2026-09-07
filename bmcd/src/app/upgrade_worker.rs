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
use crate::hal::{NodeId, UsbRoute};
use crate::streaming_data_service::data_transfer::DataTransfer;
use crate::utils::WriteMonitor;
use anyhow::bail;
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
/// Directory created under the chosen staging location, removed again when
/// `osupdate` returns.
const UPGRADE_DIR_NAME: &str = "os_upgrade";
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
    /// at any time when the `CancellationToken` is cancelled. When a transfer
    /// is interrupted or failed, it will always powers off the Node and
    /// restores the USB mode equally to a successful flow would.
    pub async fn flash_node(
        mut self,
        bmc: Arc<BmcApplication>,
        node: NodeId,
    ) -> anyhow::Result<()> {
        let device = bmc.node_in_flash(node, UsbRoute::Bmc).await?;

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

        if let Ok(()) = result {
            tracing::info!("Flashing {node} successful, restoring USB & power settings.");
        }

        // disregarding the result, set the BMC in the finalized state.
        bmc.activate_slot(node.to_inverse_bitfield(), node.to_bitfield())
            .await?;
        bmc.usb_boot(node, false).await?;
        let (mode, _) = bmc.get_usb_mode().await;
        bmc.configure_usb(mode).await?;
        result
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

    pub async fn os_update(mut self) -> anyhow::Result<()> {
        let file_name = self.data_transfer.file_name()?.to_owned();
        // Read the size before the reader is taken: on a URL transfer it is the
        // content-length of a response that `reader()` consumes.
        let image_size = self.data_transfer.size()?;
        let source = self.data_transfer.reader().await?;
        tracing::info!("start firmware upgrade {}", file_name.to_string_lossy());

        let staging_dir = select_staging_dir(image_size);
        let mut os_update_img = staging_dir.clone();
        os_update_img.push(&file_name);

        tokio::fs::create_dir_all(&staging_dir).await?;

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&os_update_img)
            .await?;

        let crc = Crc::<u64>::new(&CRC_64_REDIS);
        let mut writer = WriteMonitor::new(&mut file, &mut self.written_sender, &crc);
        copy_or_cancel(source, &mut writer, &self.cancel).await?;

        let result = spawn_blocking(move || {
            Command::new("sh")
                .arg("-c")
                .arg(format!("osupdate {}", os_update_img.to_string_lossy()))
                .status()
        })
        .await?;

        tokio::fs::remove_dir_all(&staging_dir).await?;

        let success = result?;
        if !success.success() {
            bail!("failed firmware upgrade ({})", success);
        }

        Ok(())
    }
}

/// Selects the directory to stage a firmware image in. Returns the first entry
/// of [`UPGRADE_STAGING_DIRS`] that is a mount point of its own, is mounted
/// read-write, and has room for `image_size` bytes. When none of them
/// qualifies it falls back to `/tmp`, which is where every image was staged
/// before this function existed.
fn select_staging_dir(image_size: u64) -> PathBuf {
    for candidate in UPGRADE_STAGING_DIRS {
        let path = Path::new(candidate);

        if !is_mount_point(path) {
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
fn is_mount_point(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        // `/` has no parent and is always a mount point.
        return true;
    };

    match (path.metadata(), parent.metadata()) {
        (Ok(dir), Ok(parent)) => dir.dev() != parent.dev(),
        _ => false,
    }
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
    use tempdir::TempDir;
    use tokio::io::BufWriter;

    fn random_array<const SIZE: usize>() -> Vec<u8> {
        let mut array = vec![0; SIZE];
        rand::rng().fill_bytes(&mut array);
        array
    }

    #[test]
    fn mount_point_detection() {
        assert!(is_mount_point(Path::new("/")));
        // a directory inside a mount shares the device id of its parent.
        let dir = TempDir::new("staging_test").unwrap();
        assert!(!is_mount_point(dir.path()));
        // a directory that is not there cannot be written to either.
        assert!(!is_mount_point(&dir.path().join("absent")));
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
}
