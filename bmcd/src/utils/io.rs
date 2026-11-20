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
use crc::{Crc, Digest as CrcDigest};
use futures::Stream;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::{pin::Pin, task::Poll};
use tokio::{io::AsyncWrite, sync::watch};

/// Write `bytes` to a new or truncated file and flush them to stable storage.
///
/// For callers that keep a side copy under a name of their own before linking
/// it into place; [`write_durably`] uses [`write_new_synced`] instead.
pub fn write_synced(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    write_file_synced(path, bytes, mode, false)
}

/// Like [`write_synced`], but the file must not exist yet (`O_EXCL`): a name
/// somebody else holds is an error, never a silent truncation.
fn write_new_synced(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    write_file_synced(path, bytes, mode, true)
}

fn write_file_synced(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    exclusive: bool,
) -> io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true);
    if exclusive {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(true);
    }
    if let Some(mode) = mode {
        opts.mode(mode);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Replace `path` with `bytes` so a power cut cannot leave a truncated file.
///
/// Temp file in the same directory, `sync_all` on the data, rename, then
/// `sync_all` on the directory so the name itself reaches the media. A rename
/// alone is atomic as a name swap; without the syncs, UBIFS can still lose the
/// data behind a correctly named empty or truncated file.
pub fn write_durably(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_durably_with_mode(path, bytes, None)
}

/// Same as [`write_durably`], with an explicit Unix mode for the new file
/// (private keys, for example).
///
/// The temp file is `<name>.tmp` beside the target: one fixed name per target,
/// so a write cut short by a power loss leaves at most one stale file, and the
/// next write of the same target removes it. It is opened with `create_new`
/// after that removal, so it can never share bytes with another file. Writes
/// are serialised in this process, because two writers of one target would
/// otherwise remove each other's temp file.
pub fn write_durably_with_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    static WRITING: Mutex<()> = Mutex::new(());
    // The guard protects no data, so a poisoned lock is as good as a free one.
    let _one_at_a_time = WRITING.lock().unwrap_or_else(|e| e.into_inner());

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_os_string();
    name.push(".tmp");
    let tmp = dir.join(name);
    match std::fs::remove_file(&tmp) {
        Ok(()) => tracing::info!("removed {}, left by an interrupted write", tmp.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    write_new_synced(&tmp, bytes, mode).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    // The directory entry is what names the file; sync it too. The rename
    // already put the new bytes under `path`, so a sync failure must not be
    // reported as "write failed" -- callers would undo backups of a live file.
    // Logging it and returning Ok is deliberate, not an oversight.
    if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        tracing::warn!(
            "{} was replaced, but syncing {} failed: {e}",
            path.display(),
            dir.display()
        );
    }
    Ok(())
}

pub struct Sha256StreamValidator<T>
where
    T: Stream<Item = io::Result<bytes::Bytes>>,
{
    hasher: Option<Sha256>,
    expected_sha: bytes::Bytes,
    stream: T,
}

impl<T> Sha256StreamValidator<T>
where
    T: Stream<Item = io::Result<bytes::Bytes>>,
{
    pub fn new(stream: T, expected_sha: bytes::Bytes) -> Self {
        Self {
            stream,
            expected_sha,
            hasher: Some(Sha256::new()),
        }
    }

    pub fn verify_hash(&mut self) -> io::Result<()> {
        let sha256 = self
            .hasher
            .replace(Sha256::new())
            .expect("hasher cannot be None")
            .finalize()
            .to_vec();

        if sha256 != self.expected_sha {
            let got = hex::encode(sha256);
            let expected = hex::encode(&self.expected_sha);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "sha256 checksum failed. Expected: {}, got: {}",
                    expected, got
                ),
            ));
        }

        Ok(())
    }
}

impl<T> Stream for Sha256StreamValidator<T>
where
    T: Stream<Item = io::Result<bytes::Bytes>> + Unpin,
{
    type Item = T::Item;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let me = Pin::get_mut(self);
        let result = Pin::new(&mut me.stream).poll_next(cx);

        match result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(bytes))) if !bytes.is_empty() => {
                me.hasher
                    .as_mut()
                    .expect("hasher can never be None")
                    .update(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(_))) => result,
            _ => {
                // stream is exhausted. This means one of the following:
                // * `Poll::Ready(Some(bytes))` was empty
                // * `Poll::Ready(None)` end of stream signal.
                if let Err(e) = me.verify_hash() {
                    Poll::Ready(Some(Err(e)))
                } else {
                    Poll::Ready(None)
                }
            }
        }
    }
}

pub struct WriteMonitor<'a, W>
where
    W: AsyncWrite,
{
    written: u64,
    sender: &'a mut watch::Sender<u64>,
    digest: CrcDigest<'a, u64>,
    inner: W,
}

impl<'a, W> WriteMonitor<'a, W>
where
    W: AsyncWrite,
{
    pub fn new(writer: W, sender: &'a mut watch::Sender<u64>, crc: &'a Crc<u64>) -> Self {
        Self {
            written: 0,
            sender,
            digest: crc.digest(),
            inner: writer,
        }
    }

    pub fn crc(self) -> u64 {
        self.digest.finalize()
    }
}

impl<W> AsyncWrite for WriteMonitor<'_, W>
where
    W: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let me = Pin::get_mut(self);

        let result = Pin::new(&mut me.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = result {
            me.digest.update(&buf[..written]);
            me.written += written as u64;
            me.sender.send_replace(me.written);
        }
        result
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let me = Pin::get_mut(self);
        Pin::new(&mut me.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        let me = Pin::get_mut(self);
        Pin::new(&mut me.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crc::CRC_64_REDIS;
    use rand::RngCore;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn random_array<const SIZE: usize>() -> Vec<u8> {
        let mut array = vec![0; SIZE];
        rand::rng().fill_bytes(&mut array);
        array
    }

    #[test]
    fn write_durably_replaces_the_file_and_leaves_no_tmp() {
        let dir = tempdir::TempDir::new("write-durably").expect("temp dir");
        let path = dir.path().join("switch.json");
        std::fs::write(&path, b"old").unwrap();
        write_durably(&path, b"new confirmed").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new confirmed");
        assert_eq!(names_in(dir.path()), ["switch.json"]);
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// The TLS key is the reason the mode parameter exists.
    #[test]
    fn write_durably_with_mode_makes_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir::TempDir::new("write-durably-mode").expect("temp dir");
        let key = dir.path().join("server.key");
        write_durably_with_mode(&key, b"key", Some(0o600)).unwrap();
        assert_eq!(mode_of(&key), 0o600);

        // An existing, more open file is replaced by a private one.
        std::fs::write(&key, b"old").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_durably_with_mode(&key, b"newer key", Some(0o600)).unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), b"newer key");
        assert_eq!(mode_of(&key), 0o600);
        assert_eq!(names_in(dir.path()), ["server.key"]);
    }

    /// A write cut short by a power loss leaves `<name>.tmp`; the next write of
    /// the same file removes it, so nothing piles up.
    #[test]
    fn write_durably_removes_a_temp_left_by_an_interrupted_write() {
        let dir = tempdir::TempDir::new("write-durably-stale").expect("temp dir");
        let path = dir.path().join("switch.json");
        std::fs::write(dir.path().join("switch.json.tmp"), b"half a wri").unwrap();
        write_durably(&path, b"whole").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"whole");
        assert_eq!(names_in(dir.path()), ["switch.json"]);
    }

    /// A temp name that cannot be cleared is an error, and the target is
    /// untouched. A directory there fails even for root.
    #[test]
    fn write_durably_fails_cleanly_when_the_temp_name_is_taken() {
        let dir = tempdir::TempDir::new("write-durably-blocked").expect("temp dir");
        let path = dir.path().join("switch.json");
        std::fs::write(&path, b"old").unwrap();
        std::fs::create_dir(dir.path().join("switch.json.tmp")).unwrap();
        assert!(write_durably(&path, b"new").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
    }

    /// A rename that fails takes its temp file with it.
    #[test]
    fn write_durably_leaves_no_tmp_when_the_rename_fails() {
        let dir = tempdir::TempDir::new("write-durably-rename").expect("temp dir");
        let path = dir.path().join("switch.json");
        // A directory cannot be replaced by a file.
        std::fs::create_dir(&path).unwrap();
        assert!(write_durably(&path, b"new").is_err());
        assert_eq!(names_in(dir.path()), ["switch.json"]);
    }

    #[test]
    fn write_durably_creates_missing_parents() {
        let dir = tempdir::TempDir::new("write-durably-parents").expect("temp dir");
        let path = dir.path().join("etc").join("bmcd").join("switch.json");
        write_durably(&path, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    }

    #[tokio::test]
    async fn write_watcher_test() {
        let crc = Crc::<u64>::new(&CRC_64_REDIS);
        let mut reader = tokio::io::repeat(0b101).take(1044 * 1004);
        let (mut sender, receiver) = watch::channel(0u64);
        let mut writer = WriteMonitor::new(tokio::io::sink(), &mut sender, &crc);
        let copied = tokio::io::copy(&mut reader, &mut writer).await.unwrap();
        assert_eq!(copied, 1044 * 1004);
        assert_eq!(*receiver.borrow(), 1044 * 1004);
    }

    #[tokio::test]
    async fn crc_reader_test() {
        let read_buffer = random_array::<{ 1024 * 1024 + 23 }>();
        let expected_crc = Crc::<u64>::new(&CRC_64_REDIS).checksum(&read_buffer);

        let mut data = Vec::new();
        let crc = Crc::<u64>::new(&CRC_64_REDIS);
        let (mut sender, _) = watch::channel(0u64);
        let mut writer = WriteMonitor::new(&mut data, &mut sender, &crc);

        let mut total_read = 0;
        while total_read < read_buffer.len() {
            let range = (total_read + 1044).min(read_buffer.len());
            if let Ok(write) = writer.write(&read_buffer[total_read..range]).await {
                if write == 0 {
                    break;
                }
                total_read += write;
            }
        }

        assert_eq!(expected_crc, writer.crc());
    }

    //   #[tokio::test]
    //   async fn sha256_reader_test() {
    //       let mut buffer = random_array::<{ 1024 * 1024 + 23 }>();
    //       let mut hasher = Sha256::new();
    //       hasher.update(&buffer);
    //       let expected_sha = hasher.finalize().to_vec();
    //       let cursor = Cursor::new(&mut buffer);
    //       let mut reader = Sha256StreamValidator::new(ReaderStream::new(cursor), expected_sha.into());
    //       let mut data = Vec::new();
    //       let mut chunk = vec![0; 1044];
    //       while let Ok(read) = reader.read(&mut chunk).await {
    //           if read == 0 {
    //               break;
    //           }
    //           data.extend_from_slice(&chunk[0..read]);
    //       }

    //       assert_eq!(data, buffer);
    //   }
}
