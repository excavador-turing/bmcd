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
//! One stager at a time.
//!
//! Three things write the spare firmware volume: this daemon's upload path,
//! its `firmware_install`, and `tpi-selfupdate` run by hand. Nothing
//! serialised them, so two installs (two browser tabs, the page and `tpi`, a
//! script) both passed the "already staged" check while the first was still
//! writing and met in `osupdate`'s volume cleanup, where one removed
//! `rootfs_new` while the other wrote it (BMC-Firmware#127).
//!
//! They now share one `flock(2)` on [`STAGE_LOCK_PATH`], a file in `/run`
//! (a tmpfs, so a lock never outlives a boot). The shell scripts take it with
//! `flock -n`; this module is the daemon's half. A stager that already holds
//! it exports [`STAGE_LOCK_ENV`]`=1` to what it runs, and those scripts then
//! do not take it again -- they would otherwise find it held by their own
//! parent and refuse.
//!
//! The lock is never waited for. The second stager is told, at once, that
//! "an update is being staged"; that is a 409, and nothing is touched.

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

/// The lock file. The same path is in `osupdate`, `osunstage`,
/// `tpi-selfupdate` and `S99postupdate`; change them together.
pub const STAGE_LOCK_PATH: &str = "/run/firmware-stage.lock";

/// The environment variable a stager sets for what it runs once it holds the
/// lock, so that the script does not try to take it a second time.
pub const STAGE_LOCK_ENV: &str = "STAGE_LOCK_HELD";

/// What a second stager is told. No advice to power-cycle: waiting is the
/// whole remedy, and a power cycle hard-cuts all four compute modules.
pub const BUSY_MESSAGE: &str = "an update is being staged; try again when it has finished";

#[derive(Debug, Error)]
pub enum StageLockError {
    /// Somebody else holds the lock. The text is [`BUSY_MESSAGE`], spelled out
    /// because an attribute cannot name a constant; a test holds them equal.
    #[error("an update is being staged; try again when it has finished")]
    Busy,
    /// The lock file could not be opened or locked for another reason.
    #[error("cannot take the staging lock {}: {source}", .path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The staging lock, held. Released when this is dropped -- or when the
/// process dies, which is what makes a flock the right tool: a daemon that is
/// killed mid-install does not leave the board unable to stage until a reboot.
#[derive(Debug)]
pub struct StageLock {
    _lock: Flock<File>,
}

impl StageLock {
    /// Takes the lock, or says it is held. Never waits.
    pub fn try_acquire() -> Result<StageLock, StageLockError> {
        Self::try_acquire_at(Path::new(STAGE_LOCK_PATH))
    }

    /// [`StageLock::try_acquire`] on a file of the caller's choosing, for tests.
    pub fn try_acquire_at(path: &Path) -> Result<StageLock, StageLockError> {
        let io_error = |source: std::io::Error| StageLockError::Io {
            path: path.to_path_buf(),
            source,
        };
        // std opens with O_CLOEXEC: what the daemon runs does not inherit the
        // descriptor, and holds the lock only through `STAGE_LOCK_ENV`.
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(io_error)?;

        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => Ok(StageLock { _lock: lock }),
            // EWOULDBLOCK and EAGAIN are one value on Linux.
            Err((_, Errno::EAGAIN)) => Err(StageLockError::Busy),
            Err((_, errno)) => Err(io_error(std::io::Error::from(errno))),
        }
    }

    /// Tells a script this process spawns that the lock is already held, so
    /// it must not try to take it.
    pub fn mark_held(&self, command: &mut Command) {
        command.env(STAGE_LOCK_ENV, "1");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn lock_path() -> (TempDir, PathBuf) {
        let dir = TempDir::with_prefix("stage-lock").expect("tempdir");
        let path = dir.path().join("firmware-stage.lock");
        (dir, path)
    }

    #[test]
    fn a_second_stager_is_told_so_at_once() {
        let (_dir, path) = lock_path();
        let _first = StageLock::try_acquire_at(&path).expect("first");

        let second = StageLock::try_acquire_at(&path);
        assert!(matches!(second, Err(StageLockError::Busy)), "{second:?}");
        assert_eq!(StageLockError::Busy.to_string(), BUSY_MESSAGE);
    }

    #[test]
    fn the_busy_message_gives_no_power_cycle_advice() {
        // EBUSY used to be answered with "power-cycle the board", which
        // hard-cuts all four compute modules for a problem that waiting fixes.
        assert!(!BUSY_MESSAGE.to_lowercase().contains("power"));
        assert!(!BUSY_MESSAGE.to_lowercase().contains("cycle"));
    }

    #[test]
    fn dropping_the_lock_releases_it() {
        let (_dir, path) = lock_path();
        let first = StageLock::try_acquire_at(&path).expect("first");
        drop(first);
        StageLock::try_acquire_at(&path).expect("after the first one let go");
    }

    #[test]
    fn a_lock_that_cannot_be_opened_is_an_io_error_not_busy() {
        let (_dir, path) = lock_path();
        let missing = path.join("no-such-directory").join("lock");
        let result = StageLock::try_acquire_at(&missing);
        assert!(
            matches!(result, Err(StageLockError::Io { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_script_is_told_the_lock_is_held() {
        let (_dir, path) = lock_path();
        let lock = StageLock::try_acquire_at(&path).expect("lock");
        let mut command = Command::new("osupdate");
        lock.mark_held(&mut command);
        let envs: Vec<_> = command
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect();
        assert_eq!(
            envs,
            [(
                std::ffi::OsString::from(STAGE_LOCK_ENV),
                Some(std::ffi::OsString::from("1"))
            )]
        );
    }

    /// The scripts take the same file with `flock -n`; a held lock must stop
    /// them and a free one must not. Skipped where there is no `flock`.
    #[test]
    fn the_shell_scripts_and_the_daemon_exclude_each_other() {
        let (_dir, path) = lock_path();
        let flock = |path: &Path| {
            Command::new("flock")
                .arg("-n")
                .arg(path)
                .arg("true")
                .status()
        };
        let Ok(free) = flock(&path) else {
            eprintln!("no flock(1) here; skipping");
            return;
        };
        assert!(free.success(), "a free lock is taken by the script");

        let _held = StageLock::try_acquire_at(&path).expect("lock");
        assert!(
            !flock(&path).expect("flock").success(),
            "a lock the daemon holds is refused to the script"
        );
    }
}
