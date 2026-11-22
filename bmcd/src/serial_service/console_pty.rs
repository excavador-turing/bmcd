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
//! One shared console device per node, for tools running on the BMC itself.
//!
//! ## Why
//!
//! `microcom` or `picocom` on `/dev/ttyS1` and the daemon's own reader are two
//! readers of one UART. Each byte goes to exactly one of them, so a line is
//! split between the web console and the terminal and neither shows all of it.
//! The daemon also holds a shared `flock` on those ports, which makes
//! `picocom`'s exclusive lock fail. That lock stays: it now means "use the
//! node's console device instead".
//!
//! ## What
//!
//! For every node the daemon creates one pseudo-terminal and a stable symlink
//! to its slave side, `/dev/ttyNode1` to `/dev/ttyNode4`. The node's number
//! is the number on the board, whatever `ttyS` it sits behind.
//!
//! * Everything the node sends is also written to the master, so a tool on the
//!   slave sees what the web console sees.
//! * Whatever a tool writes to the slave is read off the master and sent to
//!   the node through the same channel the web console writes into, so typing
//!   from either side reaches the node.
//!
//! The speed of the real port is a node setting (`uart_baud`, see
//! `NodeInfo`), not something a terminal program can change: a baud rate set
//! on the slave is accepted by the kernel and does nothing at all, because
//! there is no line behind it.
//!
//! ## When nobody has the slave open
//!
//! Writing the node's output must never stall the serial reader or the web
//! clients, and must not make the daemon's memory grow. The daemon keeps its
//! own descriptor to the slave open, so the master never sees a hang-up and
//! the settings it made to the slave stay; that also means output with no
//! tool attached piles up in the kernel's buffer for the pty, which is
//! bounded. The master is non-blocking: when the kernel's buffer is full the
//! write is dropped, and the stale input of the slave is discarded so that a
//! tool opening the device later sees the recent output, not the oldest. A
//! tool that is attached but slower than the node loses output the same way.
//! Up to one buffer of recent output may be visible to a tool that opens the
//! device after the node was quiet; that is not a history, only whatever was
//! not yet dropped.

use bytes::Bytes;
use nix::{
    errno::Errno,
    fcntl::{fcntl, FcntlArg, OFlag},
    pty::{grantpt, posix_openpt, ptsname_r, unlockpt, PtyMaster},
    sys::termios::{cfmakeraw, tcflush, tcgetattr, tcsetattr, FlushArg, SetArg},
};
use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{symlink, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{io::unix::AsyncFd, sync::mpsc::WeakSender, task::JoinHandle};

/// Where the per-node console symlinks live.
pub const CONSOLE_LINK_DIR: &str = "/dev";

/// The name of the console device of a node, `node` being the number on the
/// board, 1 to 4.
pub fn console_link_name(node: usize) -> String {
    format!("ttyNode{node}")
}

/// A chunk of what a local tool typed. Bounded, so one `write(2)` of a large
/// buffer reaches the node as several messages, as it does from the web
/// console.
const INPUT_CHUNK: usize = 1024;

/// How long to wait before looking at the master again after a read error that
/// is not "nothing to read". Keeps a failing descriptor from spinning a core.
const INPUT_ERROR_BACKOFF: Duration = Duration::from_millis(250);

/// The pseudo-terminal of one node. See the module documentation.
#[derive(Debug)]
pub struct ConsolePty {
    master: AsyncFd<PtyMaster>,
    /// Kept open for the life of the daemon: it keeps the master from seeing
    /// a hang-up when the last tool closes the device, and the raw settings
    /// made below stay in force.
    slave: File,
    slave_path: PathBuf,
    link: PathBuf,
}

impl ConsolePty {
    /// Creates the pseudo-terminal and points `link` at its slave side,
    /// replacing a symlink left behind by an earlier run. Must be called from
    /// within the tokio runtime.
    ///
    /// Anything at `link` that is not a symlink is left alone and reported as
    /// an error: a regular file or device somebody put there is not ours to
    /// remove.
    pub fn create(link: &Path) -> io::Result<Self> {
        let master = posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY).map_err(io::Error::from)?;
        grantpt(&master).map_err(io::Error::from)?;
        unlockpt(&master).map_err(io::Error::from)?;
        let slave_path = PathBuf::from(ptsname_r(&master).map_err(io::Error::from)?);

        // The master is read and written from async code and must never block
        // it, whatever the slave's side is doing.
        fcntl(master.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).map_err(io::Error::from)?;

        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOCTTY)
            .open(&slave_path)?;

        // Raw: no echo, no CR/LF translation, no line editing and no signal
        // characters. The bridge carries bytes between the node and a tool;
        // a line discipline in between would show the node's own echo twice
        // and mangle anything that is not text. The line speed in this
        // termios is meaningless for a pty and is left as it is.
        let mut termios = tcgetattr(&slave).map_err(io::Error::from)?;
        cfmakeraw(&mut termios);
        tcsetattr(&slave, SetArg::TCSANOW, &termios).map_err(io::Error::from)?;

        remove_stale_link(link)?;
        symlink(&slave_path, link)?;

        // SAFETY: `PtyMaster` owns its descriptor, closes it only on drop, and
        // `as_raw_fd` always returns that same descriptor.
        let master = unsafe { AsyncFd::register(master) }?;

        Ok(ConsolePty {
            master,
            slave,
            slave_path,
            link: link.to_path_buf(),
        })
    }

    /// The device the tools open: the symlink, not the numbered slave behind
    /// it, which changes with every start of the daemon.
    pub fn link(&self) -> &Path {
        &self.link
    }

    /// The slave side the link points at.
    pub fn slave_path(&self) -> &Path {
        &self.slave_path
    }

    /// Writes what the node sent to the master, so it appears on the slave.
    /// Never blocks and never waits for a reader: what the kernel will not
    /// take is dropped, and the slave's stale input is discarded (see the
    /// module documentation).
    ///
    /// # Returns
    ///
    /// The number of bytes passed on. Less than `data.len()` means the rest
    /// was dropped.
    pub fn publish(&self, data: &[u8]) -> usize {
        let mut written = 0;
        while written < data.len() {
            match nix::unistd::write(self.master.get_ref(), &data[written..]) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => {
                    // Nobody is reading fast enough, or nobody is reading.
                    // Throw away what is queued for the slave so that the
                    // next tool to look sees recent output, and drop the rest
                    // of this chunk: the node's output is not to wait for it.
                    if let Err(e) = tcflush(&self.slave, FlushArg::TCIFLUSH) {
                        tracing::debug!("console pty {}: flush: {}", self.link.display(), e);
                    }
                    break;
                }
                Err(e) => {
                    tracing::debug!("console pty {}: write: {}", self.link.display(), e);
                    break;
                }
            }
        }
        written
    }

    /// Reads what tools write to the slave and sends it to the node through
    /// `writer`, the channel the web console writes into. Ends when the node's
    /// serial worker is gone. The task holds only a weak sender, so it does
    /// not keep that channel alive.
    pub fn spawn_input(self: &Arc<Self>, writer: WeakSender<Bytes>) -> JoinHandle<()> {
        let pty = self.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; INPUT_CHUNK];
            loop {
                let n = match pty.read_input(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::debug!("console pty {}: read: {}", pty.link.display(), e);
                        tokio::time::sleep(INPUT_ERROR_BACKOFF).await;
                        continue;
                    }
                };
                if n == 0 {
                    continue;
                }
                let Some(sender) = writer.upgrade() else {
                    break;
                };
                if sender
                    .send(Bytes::copy_from_slice(&buf[..n]))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    async fn read_input(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.master.readable().await?;
            match guard.try_io(|fd| {
                nix::unistd::read(fd.get_ref().as_raw_fd(), buf).map_err(io::Error::from)
            }) {
                Ok(res) => return res,
                Err(_would_block) => continue,
            }
        }
    }
}

impl Drop for ConsolePty {
    fn drop(&mut self) {
        // Leave a link that points somewhere else alone: a newer daemon may
        // already have replaced it.
        if matches!(std::fs::read_link(&self.link), Ok(t) if t == self.slave_path) {
            let _ = std::fs::remove_file(&self.link);
        }
    }
}

/// Removes a symlink left by an earlier run. Nothing there is fine. Anything
/// that is not a symlink is an error.
fn remove_stale_link(link: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(link),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a symlink", link.display()),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    fn open_slave(link: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK)
            .open(link)
            .unwrap()
    }

    fn read_available(file: &mut File) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("read: {e}"),
            }
        }
        out
    }

    #[tokio::test]
    async fn link_points_at_the_slave_and_is_removed_on_drop() {
        let dir = TempDir::with_prefix("console_pty").unwrap();
        let link = dir.path().join("ttyNode1");
        let pty = ConsolePty::create(&link).unwrap();

        assert_eq!(std::fs::read_link(&link).unwrap(), pty.slave_path());
        assert!(pty.slave_path().starts_with("/dev/pts"));
        drop(pty);
        assert!(std::fs::symlink_metadata(&link).is_err());
    }

    #[tokio::test]
    async fn a_stale_link_is_replaced_and_a_real_file_is_not() {
        let dir = TempDir::with_prefix("console_pty").unwrap();
        let link = dir.path().join("ttyNode2");
        symlink("/dev/pts/does-not-exist", &link).unwrap();
        let pty = ConsolePty::create(&link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), pty.slave_path());

        let file = dir.path().join("ttyNode3");
        std::fs::write(&file, b"mine").unwrap();
        assert!(ConsolePty::create(&file).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"mine");
    }

    #[tokio::test]
    async fn drop_leaves_a_link_that_was_taken_over() {
        let dir = TempDir::with_prefix("console_pty").unwrap();
        let link = dir.path().join("ttyNode1");
        let pty = ConsolePty::create(&link).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink("/dev/pts/elsewhere", &link).unwrap();
        drop(pty);
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("/dev/pts/elsewhere")
        );
    }

    #[tokio::test]
    async fn node_output_reaches_the_slave_untouched() {
        let dir = TempDir::with_prefix("console_pty").unwrap();
        let link = dir.path().join("ttyNode1");
        let pty = ConsolePty::create(&link).unwrap();
        let mut tool = open_slave(&link);

        // Raw: a line feed is not turned into CR LF, nothing is echoed back.
        let sent = b"login:\n\x1b[0m\xff\x00\x03 end";
        assert_eq!(pty.publish(sent), sent.len());
        // The tty layer hands bytes to the slave from a worker; give it a moment.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(read_available(&mut tool), sent);
    }

    #[tokio::test]
    async fn what_a_tool_types_goes_to_the_write_channel_without_echo() {
        let dir = TempDir::with_prefix("console_pty").unwrap();
        let link = dir.path().join("ttyNode1");
        let pty = Arc::new(ConsolePty::create(&link).unwrap());
        let (tx, mut rx) = mpsc::channel::<Bytes>(8);
        let _task = pty.spawn_input(tx.downgrade());
        let mut tool = open_slave(&link);

        tool.write_all(b"ls -l\r").unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&got[..], b"ls -l\r");

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(read_available(&mut tool).is_empty(), "the slave echoed");
    }

    #[tokio::test]
    async fn nobody_reading_never_blocks_and_stays_bounded() {
        let dir = TempDir::with_prefix("console_pty").unwrap();
        let link = dir.path().join("ttyNode1");
        let pty = ConsolePty::create(&link).unwrap();

        // Nobody has the slave open besides the daemon. Far more than the
        // kernel will hold; every call must return at once.
        let chunk = [b'x'; 4096];
        let started = std::time::Instant::now();
        let mut dropped = 0usize;
        for _ in 0..4096 {
            dropped += chunk.len() - pty.publish(&chunk);
        }
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(dropped > 0, "nothing was dropped, so something is queueing");

        // A tool that opens the device afterwards gets at most what the
        // kernel kept, not the 16 MiB sent.
        let mut tool = open_slave(&link);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(read_available(&mut tool).len() < 1 << 20);
    }

    #[tokio::test]
    async fn a_baud_rate_set_on_the_slave_is_accepted_and_harmless() {
        use nix::sys::termios::{cfsetspeed, BaudRate};
        let dir = TempDir::with_prefix("console_pty").unwrap();
        let link = dir.path().join("ttyNode1");
        let pty = ConsolePty::create(&link).unwrap();
        let tool = open_slave(&link);

        let mut t = tcgetattr(&tool).unwrap();
        cfsetspeed(&mut t, BaudRate::B9600).unwrap();
        tcsetattr(&tool, SetArg::TCSANOW, &t).unwrap();

        // Bytes still flow.
        assert_eq!(pty.publish(b"ok"), 2);
    }
}
