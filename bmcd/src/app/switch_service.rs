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

//! The switch configuration, owned in one place.
//!
//! Holds the state machine, runs the commands it asks for, and writes the
//! confirmed document where a reboot will find it.
//!
//! ## The daemon owns this configuration
//!
//! The running document is the one this daemon last applied, held in memory,
//! rather than something parsed back out of `bridge vlan show`. That is a
//! deliberate trade. Parsing it back would notice somebody changing the
//! bridge by hand from a console; holding it means a hand edit and the
//! daemon's idea of the world quietly disagree.
//!
//! Holding it wins because the alternative costs a parser for a format that
//! exists to be read by people, on the one path where being wrong strands the
//! board. A document applied from a snapshot that was misparsed is worse than
//! one applied from a stale snapshot, because the stale one at least
//! describes something that was true.
//!
//! ## Where it is written, and when
//!
//! `/etc/bmcd/switch.json`, on the overlay, and **only on confirm**. A reboot
//! in the middle of an unconfirmed change finds the previous document there,
//! because the pending one was never written.

use crate::app::switch_applier::{interface_name, plan, Command};
use crate::app::switch_change::{
    ChangeError, Effect, Pending, RevertRecord, SwitchState, DEFAULT_WINDOW,
};
use crate::app::switch_document::{PortId, Preset, SwitchDocument};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;

/// Where the confirmed document lives. Beside `access.json`, on the overlay,
/// for the same reason: this daemon never rewrites `config.yaml`.
pub const SWITCH_CONFIG: &str = "/etc/bmcd/switch.json";

/// How often the state machine is asked whether a window has passed.
///
/// One second. The window is measured in tens of seconds, so this is precise
/// enough, and a revert is the kind of thing that should not wait on a slow
/// poll while somebody watches a page.
pub const TICK: Duration = Duration::from_secs(1);

/// What a client sees.
#[derive(Debug, Serialize)]
pub struct SwitchView {
    /// What the board is running now.
    pub running: SwitchDocument,
    /// The last document anybody confirmed, and therefore what a reboot comes
    /// back to. `null` before anything has been confirmed.
    pub confirmed: Option<SwitchDocument>,
    pub pending: Option<Pending>,
    /// So the interface can say *your change at 12:03 was put back because it
    /// was not confirmed* rather than *something happened*.
    pub last_revert: Option<RevertRecord>,
    pub default_window_s: u64,
}

/// Something went wrong between the state machine and the hardware.
#[derive(Debug)]
pub enum SwitchError {
    /// The state machine refused. Not a fault: the caller asked for something
    /// the board will not do.
    Change(ChangeError),
    /// A command failed. The board is now in whatever state that command left,
    /// which is why the message says which one.
    Command {
        command: String,
        detail: String,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for SwitchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwitchError::Change(e) => write!(f, "{e}"),
            SwitchError::Command { command, detail } => {
                write!(f, "`{command}` failed: {detail}")
            }
            SwitchError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<ChangeError> for SwitchError {
    fn from(e: ChangeError) -> Self {
        SwitchError::Change(e)
    }
}

pub struct SwitchService {
    inner: Mutex<Inner>,
    path: PathBuf,
}

struct Inner {
    state: SwitchState,
    /// What this daemon last put on the switch.
    running: SwitchDocument,
}

impl SwitchService {
    /// Read whatever was confirmed before, without applying it.
    ///
    /// Applying is [`boot_apply`](Self::boot_apply), which the caller skips
    /// when the safe-mode key is held. Splitting the two is what makes safe
    /// mode possible: the daemon still knows what the configuration is and can
    /// report it, it simply has not put it on the switch.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let persisted: Option<SwitchDocument> = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok());

        let mut state = SwitchState::default();
        if let Some(document) = persisted.clone() {
            state.adopt_persisted(document);
        }

        SwitchService {
            inner: Mutex::new(Inner {
                state,
                // Until something is applied, the board is on whatever it
                // boots as, which is Flat.
                running: Preset::Flat.expand(),
            }),
            path,
        }
    }

    /// Put the confirmed document on the switch at boot.
    ///
    /// Two ways this does nothing, and both matter.
    ///
    /// **Nothing was ever confirmed**, which is the common case and is how a
    /// board that has never been configured behaves exactly as it always did.
    ///
    /// **The board is in safe mode.** Holding KEY1 at power-on puts
    /// `safemode` on the kernel command line, and `preinit` reads it the same
    /// way to decide not to mount the overlay. Skipping here is the last exit
    /// from a configuration that is wrong at every boot: a document that was
    /// confirmed, and was therefore reachable at the time, but has since
    /// stopped being -- because the router changed, or a cable moved. Without
    /// it, a board could be configured into a state only a serial console
    /// could undo.
    pub async fn boot_apply(&self) -> Result<(), SwitchError> {
        if is_safemode(Path::new(CMDLINE)) {
            tracing::warn!(
                "safe mode: the switch configuration is NOT being applied. The board is on its \
                 default network."
            );
            return Ok(());
        }
        let mut inner = self.inner.lock().await;
        let Some(document) = inner.state.confirmed().cloned() else {
            return Ok(());
        };
        let commands = plan(&inner.running, &document);
        run_all(&commands).await?;
        inner.running = document;
        tracing::info!("switch configuration applied from {}", self.path.display());
        Ok(())
    }

    pub async fn view(&self) -> SwitchView {
        let inner = self.inner.lock().await;
        SwitchView {
            running: inner.running.clone(),
            confirmed: inner.state.confirmed().cloned(),
            pending: inner.state.pending().cloned(),
            last_revert: inner.state.last_revert().cloned(),
            default_window_s: DEFAULT_WINDOW.as_secs(),
        }
    }

    /// Apply a document and start waiting to be proved right.
    pub async fn apply(
        &self,
        document: SwitchDocument,
        window: Duration,
    ) -> Result<Pending, SwitchError> {
        let mut inner = self.inner.lock().await;
        let running = inner.running.clone();
        let token = new_token();
        let (pending, effect) =
            inner
                .state
                .apply(document, running, window, SystemTime::now(), token)?;

        if let Effect::Apply(document) = effect {
            let commands = plan(&inner.running, &document);
            // A failure here leaves the switch part-way, so the pending change
            // is dropped rather than left waiting for a confirmation that
            // would persist something that was never fully applied. The window
            // is not the recovery for this; the caller's error message is.
            if let Err(e) = run_all(&commands).await {
                let _ = inner.state.revert(SystemTime::now());
                return Err(e);
            }
            inner.running = document;
        }
        Ok(pending)
    }

    /// Start the window if the uplink carrying the BMC's VLAN is forwarding.
    ///
    /// This is what makes the window mean anything. Spanning tree holds a port
    /// in listening and then learning for its forwarding delay -- 30 seconds
    /// by default -- before it passes a frame. A window counted from the apply
    /// would therefore expire before anybody could possibly confirm a correct
    /// Trunk change, and would revert every one of them.
    ///
    /// Which uplink to watch comes from the document itself: the ones in the
    /// BMC's own VLAN, because those are the only paths a confirmation could
    /// arrive over. With `Trunk` redundant that is both, and either forwarding
    /// is enough -- spanning tree will have blocked the other on purpose.
    async fn start_window_if_reachable(&self, inner: &mut Inner) {
        let Some(pending) = inner.state.pending() else {
            return;
        };
        if pending.counting_from.is_some() {
            return;
        }
        let document = pending.document.clone();
        let Some(management) = document
            .ports
            .get(&PortId::Bmc)
            .and_then(|config| config.untagged)
        else {
            // Filtering off: there is no VLAN to be reachable on, and nothing
            // to wait for.
            inner.state.uplink_forwarding(SystemTime::now());
            return;
        };

        let forwarding = document
            .members(management)
            .into_iter()
            .filter(|port| port.is_uplink())
            .filter_map(interface_name)
            .any(|name| is_forwarding(Path::new(NET_CLASS), name));

        if forwarding {
            tracing::info!("the uplink is forwarding; the confirm window starts now");
            inner.state.uplink_forwarding(SystemTime::now());
        }
    }

    pub async fn confirm(&self, token: &str) -> Result<(), SwitchError> {
        let mut inner = self.inner.lock().await;
        let effect = inner.state.confirm(token, SystemTime::now())?;
        if let Effect::Persist(document) = effect {
            write_atomically(
                &self.path,
                &serde_json::to_vec_pretty(&document)
                    .map_err(|e| SwitchError::Io(std::io::Error::other(e)))?,
            )
            .map_err(SwitchError::Io)?;
            tracing::info!("switch configuration confirmed and persisted");
        }
        Ok(())
    }

    pub async fn revert(&self) -> Result<(), SwitchError> {
        let mut inner = self.inner.lock().await;
        let effect = inner.state.revert(SystemTime::now())?;
        self.carry_out(&mut inner, effect).await
    }

    /// Called on a timer. Starts a window whose uplink has come up, and puts
    /// an unconfirmed change back once its window has passed.
    pub async fn tick(&self) {
        let mut inner = self.inner.lock().await;
        self.start_window_if_reachable(&mut inner).await;
        let effect = inner.state.tick(SystemTime::now());
        if matches!(effect, Effect::None) {
            return;
        }
        tracing::warn!("switch change was not confirmed in its window; putting the previous configuration back");
        if let Err(e) = self.carry_out(&mut inner, effect).await {
            // There is nothing above this to tell. Saying so loudly is the
            // whole of the response.
            tracing::error!("could not put the previous switch configuration back: {e}");
        }
    }

    async fn carry_out(&self, inner: &mut Inner, effect: Effect) -> Result<(), SwitchError> {
        if let Effect::Apply(document) = effect {
            let commands = plan(&inner.running, &document);
            run_all(&commands).await?;
            inner.running = document;
        }
        Ok(())
    }
}

/// Where the kernel exposes network interfaces.
pub const NET_CLASS: &str = "/sys/class/net";

/// The bridge port state that means "this port passes frames".
///
/// The kernel's numbering, from `if_bridge.h`: disabled, listening, learning,
/// forwarding, blocking.
const BR_STATE_FORWARDING: &str = "3";

/// Whether a port is passing traffic.
///
/// Reads the bridge port's own state where there is one, because that is the
/// question -- a port with a cable in it and spanning tree still learning is
/// up and carrying nothing. Where there is no `brport` directory the port is
/// not in a bridge at all, and carrier is then the only thing to go on.
fn is_forwarding(net_class: &Path, name: &str) -> bool {
    let port = net_class.join(name);
    let state = port.join("brport").join("state");
    if let Ok(raw) = std::fs::read_to_string(&state) {
        return raw.trim() == BR_STATE_FORWARDING;
    }
    std::fs::read_to_string(port.join("carrier"))
        .map(|raw| raw.trim() == "1")
        .unwrap_or(false)
}

/// Where the kernel puts what it was booted with.
pub const CMDLINE: &str = "/proc/cmdline";

/// True when the board was booted into safe mode.
///
/// The same test `preinit` makes, spelled the same way, deliberately: two
/// different opinions about whether a board is in safe mode would be worse
/// than either answer on its own.
fn is_safemode(cmdline: &Path) -> bool {
    std::fs::read_to_string(cmdline)
        .map(|line| line.split_whitespace().any(|word| word == "safemode"))
        .unwrap_or(false)
}

/// A token that names one change. Not a credential: the request carrying it is
/// authenticated anyway. It exists so a confirm cannot land on a different
/// change than the one its sender saw.
fn new_token() -> String {
    use rand::Rng;
    let bytes: [u8; 16] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Run the plan, stopping at the first failure.
///
/// `std::process::Command`, like the rest of this daemon's few shell-outs.
/// Each of these returns in well under a millisecond -- they are ioctls with a
/// command line in front of them -- so handing the runtime a blocking task per
/// command would cost more than it saved.
async fn run_all(commands: &[Command]) -> Result<(), SwitchError> {
    for command in commands {
        let (program, args) = command.split_first().expect("a command has a program");
        let output = std::process::Command::new(program)
            .args(args)
            .output()
            .map_err(SwitchError::Io)?;
        if !output.status.success() {
            return Err(SwitchError::Command {
                command: command.join(" "),
                detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
    }
    Ok(())
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::switch_document::SecondUplink;

    fn trunk() -> SwitchDocument {
        Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Redundant,
        }
        .expand()
    }

    #[tokio::test]
    async fn a_board_with_no_persisted_document_reports_flat_and_applies_nothing() {
        let dir = tempdir::TempDir::new("switch-none").expect("temp dir");
        let service = SwitchService::load(dir.path().join("switch.json"));
        let view = service.view().await;
        assert_eq!(view.confirmed, None);
        assert_eq!(view.running, Preset::Flat.expand());
        // No confirmed document means nothing to put on the switch, so this
        // runs no commands and cannot fail on a machine with no bridge.
        service.boot_apply().await.expect("nothing to do");
    }

    /// The file is the whole of the boot behaviour, so it has to survive the
    /// round trip exactly.
    #[tokio::test]
    async fn a_persisted_document_is_read_back_as_confirmed() {
        let dir = tempdir::TempDir::new("switch-persist").expect("temp dir");
        let path = dir.path().join("switch.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&trunk()).unwrap()).unwrap();

        let service = SwitchService::load(&path);
        let view = service.view().await;
        assert_eq!(view.confirmed, Some(trunk()));
        assert_eq!(
            view.running,
            Preset::Flat.expand(),
            "loading is not applying: safe mode depends on those being separate"
        );
    }

    /// A corrupt file must not stop the daemon. A board that will not start
    /// because its switch sidecar is truncated is worse than a board on Flat.
    #[tokio::test]
    async fn a_corrupt_file_reads_as_nothing_confirmed() {
        let dir = tempdir::TempDir::new("switch-corrupt").expect("temp dir");
        let path = dir.path().join("switch.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        let service = SwitchService::load(&path);
        assert_eq!(service.view().await.confirmed, None);
    }

    #[tokio::test]
    async fn the_view_reports_the_default_window() {
        let dir = tempdir::TempDir::new("switch-window").expect("temp dir");
        let service = SwitchService::load(dir.path().join("switch.json"));
        assert_eq!(
            service.view().await.default_window_s,
            DEFAULT_WINDOW.as_secs()
        );
    }

    /// A port with a cable in it and spanning tree still learning is up and
    /// carrying nothing. Treating that as forwarding would start the window
    /// during the very delay the window exists to survive.
    #[test]
    fn a_port_is_forwarding_only_in_the_forwarding_state() {
        let dir = tempdir::TempDir::new("switch-brport").expect("temp dir");
        let net = dir.path();
        for (state, expected) in [
            ("0", false),
            ("1", false),
            ("2", false),
            ("3", true),
            ("4", false),
        ] {
            let port = net.join(format!("ge{state}"));
            std::fs::create_dir_all(port.join("brport")).unwrap();
            std::fs::write(port.join("brport").join("state"), format!("{state}\n")).unwrap();
            assert_eq!(
                is_forwarding(net, &format!("ge{state}")),
                expected,
                "bridge port state {state}"
            );
        }
    }

    /// A port that is not in a bridge has no state machine, so carrier is the
    /// only thing there is to read.
    #[test]
    fn a_port_outside_a_bridge_falls_back_to_carrier() {
        let dir = tempdir::TempDir::new("switch-carrier").expect("temp dir");
        let net = dir.path();
        for (carrier, expected) in [("1", true), ("0", false)] {
            let name = format!("ge{carrier}");
            std::fs::create_dir_all(net.join(&name)).unwrap();
            std::fs::write(net.join(&name).join("carrier"), format!("{carrier}\n")).unwrap();
            assert_eq!(is_forwarding(net, &name), expected);
        }
    }

    #[test]
    fn a_port_that_does_not_exist_is_not_forwarding() {
        let dir = tempdir::TempDir::new("switch-absent").expect("temp dir");
        assert!(!is_forwarding(dir.path(), "ge0"));
    }

    #[test]
    fn safe_mode_is_read_from_the_kernel_command_line() {
        let dir = tempdir::TempDir::new("switch-cmdline").expect("temp dir");

        let plain = dir.path().join("plain");
        std::fs::write(&plain, "loglevel=8 root=254:0 postupdate\n").unwrap();
        assert!(!is_safemode(&plain));

        let safe = dir.path().join("safe");
        std::fs::write(&safe, "loglevel=8 root=254:0 safemode\n").unwrap();
        assert!(is_safemode(&safe));
    }

    /// Whole words only. A board booted with `root=/dev/safemodest` is not in
    /// safe mode, and a substring test would say it was.
    #[test]
    fn safe_mode_matches_a_whole_word() {
        let dir = tempdir::TempDir::new("switch-cmdline-word").expect("temp dir");
        let path = dir.path().join("cmdline");
        std::fs::write(&path, "root=/dev/safemodest nosafemode\n").unwrap();
        assert!(!is_safemode(&path));
    }

    /// A board whose `/proc/cmdline` cannot be read is not in safe mode. The
    /// alternative -- treating an unreadable file as safe mode -- would mean a
    /// board that silently stops applying its own configuration.
    #[test]
    fn an_unreadable_command_line_is_not_safe_mode() {
        assert!(!is_safemode(Path::new("/nonexistent/cmdline")));
    }

    #[test]
    fn a_token_is_unique_and_hex() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
