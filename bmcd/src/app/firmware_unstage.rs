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
//! Taking back a staged firmware update.
//!
//! Once an image is staged for the next boot the only way to undo it was a
//! shell on the board: `fw_setenv nextboot` and removing two notes
//! (BMC-Firmware#124). That is `osunstage`'s job now; this runs it, under the
//! staging lock, and turns what it says into a response.
//!
//! The firmware owns the procedure -- the order matters, and the same script
//! is what a person runs on the board -- and the daemon only decides whether
//! it may run now: it takes the lock first, so an install that is still
//! writing is refused with a 409 instead of being cancelled under its own
//! feet.

use crate::app::staging_lock::StageLock;
use actix_web::http::StatusCode;
use std::process::Command;

/// The script that does it.
pub const OSUNSTAGE: &str = "/sbin/osunstage";

/// `osunstage` exits with this while a tentative boot is being judged: the
/// new image is up and the gate is waiting on it.
const EXIT_TENTATIVE: i32 = 4;
/// ... and with this when a stager holds the lock. The daemon holds it
/// itself, so this cannot happen from here; it is mapped for completeness.
const EXIT_BUSY: i32 = 75;

/// Why it did not happen.
#[derive(Debug, PartialEq, Eq)]
pub struct UnstageRefusal {
    pub status: StatusCode,
    pub message: String,
}

/// Runs `osunstage` while holding `lock`.
pub async fn unstage(lock: &StageLock) -> Result<(), UnstageRefusal> {
    unstage_with(Command::new(OSUNSTAGE), lock).await
}

/// [`unstage`] with the command chosen by the caller, for tests.
pub async fn unstage_with(mut command: Command, lock: &StageLock) -> Result<(), UnstageRefusal> {
    let program = command.get_program().to_string_lossy().into_owned();
    lock.mark_held(&mut command);

    let output = tokio::task::spawn_blocking(move || command.output()).await;
    match output {
        Ok(Ok(output)) => interpret(
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        ),
        Ok(Err(e)) => Err(UnstageRefusal {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("cannot run {program}: {e}"),
        }),
        Err(e) => Err(UnstageRefusal {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("unstage task failed: {e}"),
        }),
    }
}

/// What an exit status and a stderr mean.
///
/// The script logs to stderr and its last line is the reason it stopped, which
/// is far more useful to a person than an exit code. 4 and 75 are refusals
/// (409): the request was fine and the board is busy. Anything else is a
/// failure of the board's own machinery (500), not the caller's mistake.
fn interpret(code: Option<i32>, stderr: &str) -> Result<(), UnstageRefusal> {
    let last_line = || {
        stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim().trim_start_matches("osunstage: ").to_string())
            .unwrap_or_else(|| "no output".to_string())
    };

    match code {
        Some(0) => Ok(()),
        Some(EXIT_TENTATIVE) => Err(UnstageRefusal {
            status: StatusCode::CONFLICT,
            message: "the new firmware is being checked; wait for the verdict before cancelling"
                .to_string(),
        }),
        Some(EXIT_BUSY) => Err(UnstageRefusal {
            status: StatusCode::CONFLICT,
            message: crate::app::staging_lock::BUSY_MESSAGE.to_string(),
        }),
        _ => Err(UnstageRefusal {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("could not cancel the staged update: {}", last_line()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn success_is_exit_zero() {
        assert_eq!(interpret(Some(0), "osunstage: nothing is staged\n"), Ok(()));
    }

    #[test]
    fn a_tentative_boot_and_a_busy_lock_are_409s() {
        let tentative = interpret(Some(4), "osunstage: the new firmware is being checked\n")
            .expect_err("refused");
        assert_eq!(tentative.status, StatusCode::CONFLICT);
        assert!(tentative.message.contains("being checked"));

        let busy = interpret(Some(75), "").expect_err("refused");
        assert_eq!(busy.status, StatusCode::CONFLICT);
        assert!(busy.message.contains("an update is being staged"));
    }

    #[test]
    fn a_failure_names_the_last_line_the_script_said() {
        let failed = interpret(
            Some(1),
            "Cannot write the environment\nosunstage: no flock on this board\n\n",
        )
        .expect_err("failed");
        assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            failed.message,
            "could not cancel the staged update: no flock on this board"
        );
    }

    #[test]
    fn a_script_killed_by_a_signal_is_a_failure() {
        let failed = interpret(None, "").expect_err("failed");
        assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(failed.message.ends_with("no output"));
    }

    /// A command that runs `body` under `sh`. The script is a file that is
    /// never made executable and is read by `sh`, not exec'd: a freshly
    /// written executable can be refused with ETXTBSY when another test
    /// thread forks while the file is still open for writing.
    fn script(dir: &TempDir, body: &str) -> Command {
        let path = dir.path().join("osunstage.sh");
        std::fs::write(&path, format!("{body}\n")).expect("write");
        let mut command = Command::new("sh");
        command.arg(path);
        command
    }

    /// The script is run, with the lock marked as held: without that it would
    /// try to take the lock its own parent holds, and refuse.
    #[tokio::test]
    async fn the_script_runs_and_is_told_the_lock_is_held() {
        let dir = TempDir::with_prefix("unstage").expect("tempdir");
        let lock = StageLock::try_acquire_at(&dir.path().join("lock")).expect("lock");

        let ok = script(&dir, r#"[ "$STAGE_LOCK_HELD" = 1 ] || exit 75; exit 0"#);
        assert_eq!(unstage_with(ok, &lock).await, Ok(()));

        let refused = script(&dir, "echo 'osunstage: still being checked' >&2; exit 4");
        let err = unstage_with(refused, &lock).await.expect_err("refused");
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_missing_program_is_a_500_that_names_it() {
        let dir = TempDir::with_prefix("unstage").expect("tempdir");
        let lock = StageLock::try_acquire_at(&dir.path().join("lock")).expect("lock");
        let err = unstage_with(Command::new("/no/such/osunstage"), &lock)
            .await
            .expect_err("no program");
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            err.message.contains("/no/such/osunstage"),
            "{}",
            err.message
        );
    }
}
