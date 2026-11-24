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

use crate::app::address_applier::{ApplyError, Exec, SystemExec};
use crate::app::address_document::parse;
use crate::app::address_service::{keep_a_copy_for_switch, INTERFACES_FILE};
use crate::app::switch_applier::{
    creates_bond, document_from_kernel, expected_vlans, interface_name, is_best_effort,
    parse_bridge_vlans, plan, undo_plan, Command, KernelVlans, BOND, BRIDGE, SHOW_VLANS,
};
use crate::app::switch_change::{
    ChangeError, Effect, Pending, RevertRecord, SwitchState, DEFAULT_WINDOW,
};
use crate::app::switch_document::{PortId, Preset, SecondUplink, SwitchDocument};
use crate::app::switch_interfaces::{
    address_for_rewrite, bond_executor_in, bond_executor_paths, boot_baseline, render_with,
    switch_from_header, with_interfaces_lock, write_interfaces_with, HandWrittenInterfaces,
};
use crate::utils::write_durably;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{Mutex, Notify};

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
    /// `br0`, the bridge over the switch ports, is not there. Nothing was
    /// changed.
    NoBridge(String),
    /// `/etc/network/interfaces` was written by hand and the request did not
    /// say to replace it. Nothing was persisted.
    HandWritten(String),
    /// LACP needs ifupdown-ng's bond executor so a reboot can bring `bond0` up.
    MissingBondExecutor(String),
    /// `bond0` is already there and this daemon did not make it. Nothing was
    /// changed.
    BondExists(String),
}

impl std::fmt::Display for SwitchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwitchError::Change(e) => write!(f, "{e}"),
            SwitchError::Command { command, detail } => {
                write!(f, "`{command}` failed: {detail}")
            }
            SwitchError::Io(e) => write!(f, "{e}"),
            SwitchError::NoBridge(m)
            | SwitchError::HandWritten(m)
            | SwitchError::MissingBondExecutor(m)
            | SwitchError::BondExists(m) => write!(f, "{m}"),
        }
    }
}

impl From<ChangeError> for SwitchError {
    fn from(e: ChangeError) -> Self {
        SwitchError::Change(e)
    }
}

pub struct SwitchService {
    /// Shared with the blocking thread that runs a change: it records what it
    /// did here itself, because the future that started it may be gone.
    inner: Arc<Mutex<Inner>>,
    path: PathBuf,
    /// `/etc/network/interfaces` twin of the switch document.
    interfaces_path: PathBuf,
    /// Where to look for `br0`; the kernel's, unless a test says otherwise.
    net_class: PathBuf,
    /// Where ifupdown-ng's bond executor may be installed on this image.
    bond_executors: Vec<PathBuf>,
    /// Runs `ip` and `bridge`. The real thing, unless a test says otherwise.
    exec: Arc<dyn Exec>,
    /// One switch change at a time. The turn is held by the blocking thread
    /// that runs the commands, not by the future waiting for it: if the request
    /// is dropped (the client went away) the thread carries on, and the next
    /// change still waits for it to finish.
    busy: Arc<AtomicBool>,
    busy_notify: Arc<Notify>,
}

struct Inner {
    state: SwitchState,
    /// What this daemon last put on the switch.
    running: SwitchDocument,
    /// Confirm must keep a copy of a hand-written interfaces file first.
    replaces_hand_written: bool,
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
        let persisted = std::fs::read(&path)
            .ok()
            .and_then(|raw| SwitchDocument::from_json_slice(&raw));

        let mut state = SwitchState::default();
        if let Some(document) = persisted.clone() {
            state.adopt_persisted(document);
        }

        SwitchService {
            inner: Arc::new(Mutex::new(Inner {
                state,
                // Until something is applied, the board is on whatever it
                // boots as, which is Flat.
                running: Preset::flat().expand(),
                replaces_hand_written: false,
            })),
            path,
            interfaces_path: PathBuf::from(INTERFACES_FILE),
            net_class: NET_CLASS.into(),
            bond_executors: bond_executor_paths(),
            exec: Arc::new(SystemExec),
            busy: Arc::new(AtomicBool::new(false)),
            busy_notify: Arc::new(Notify::new()),
        }
    }

    /// Wait until no other switch change is in progress, then hold that place
    /// until the guard drops.
    async fn switch_turn(&self) -> SwitchTurn {
        loop {
            // Subscribe before the check. The other order loses the wake when
            // the holder finishes between the check and the wait.
            let notified = self.busy_notify.notified();
            tokio::pin!(notified);
            if self
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return SwitchTurn {
                    busy: Arc::clone(&self.busy),
                    notify: Arc::clone(&self.busy_notify),
                };
            }
            notified.await;
        }
    }

    /// Run `work` on a blocking thread that owns `turn`.
    ///
    /// `ip` and `bridge` are processes, so they run off the async workers, and
    /// `inner` is not held while they do. The turn travels with the work and
    /// not with the future that awaits it: a request future is dropped when its
    /// client disconnects, while the thread keeps running its commands. The
    /// turn is released when the commands are done, and `work` records what it
    /// did in `inner` itself, because nothing else will.
    async fn run_owning_turn<T: Send + 'static>(
        turn: SwitchTurn,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, SwitchError> {
        tokio::task::spawn_blocking(move || {
            let _turn = turn;
            work()
        })
        .await
        .map_err(|e| {
            SwitchError::Io(std::io::Error::other(format!(
                "switch command task stopped: {e}"
            )))
        })
    }
}

struct SwitchTurn {
    busy: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Drop for SwitchTurn {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
        self.notify.notify_waiters();
    }
}

impl SwitchService {
    /// Tests run the plan against a recording stand-in instead of `ip`.
    #[cfg(test)]
    pub fn with_exec(mut self, exec: Arc<dyn Exec>) -> Self {
        self.exec = exec;
        self
    }

    /// Tests say where the bond executor is, or that it is nowhere.
    #[cfg(test)]
    pub fn with_bond_executors(mut self, paths: Vec<PathBuf>) -> Self {
        self.bond_executors = paths;
        self
    }

    fn bond_native(&self) -> bool {
        bond_executor_in(&self.bond_executors)
    }

    fn bond_present(&self) -> bool {
        bond_present_in(&self.net_class)
    }

    /// `br0`'s `vlan_filtering`, from the bridge itself.
    fn kernel_filtering(&self) -> Option<bool> {
        kernel_filtering_in(&self.net_class)
    }

    /// What the bridge ports are in, per `bridge -j vlan show`. `None` when
    /// that cannot be run or read, which is "unknown", never "nothing".
    fn kernel_vlans(&self) -> Option<KernelVlans> {
        kernel_vlans_from(self.exec.as_ref())
    }

    /// What is missing from the kernel that `document` says should be there:
    /// `bond0` for LACP, `vlan_filtering` on `br0`, and each front port's
    /// VLANs. `None` when it all is.
    ///
    /// The `interfaces` file matching the document only says `ifup` was given
    /// the right lines. If `ifup br0` stopped before the `post-up` lines ran
    /// (DHCP failing on an untagged, unfiltered bridge is enough), the VLANs
    /// are not there and the file is no evidence otherwise.
    fn kernel_drift(&self, document: &SwitchDocument) -> Option<String> {
        if document.second_uplink == SecondUplink::Lag && !self.bond_present() {
            return Some(format!("{BOND} does not exist"));
        }
        match self.kernel_filtering() {
            Some(held) if held == document.vlan_filtering => {}
            Some(held) => {
                return Some(format!(
                    "{BRIDGE} has vlan_filtering {}, the document wants {}",
                    u8::from(held),
                    u8::from(document.vlan_filtering)
                ))
            }
            None => return Some(format!("{BRIDGE}'s vlan_filtering cannot be read")),
        }
        let expected = expected_vlans(document);
        if expected.is_empty() {
            return None;
        }
        let Some(held) = self.kernel_vlans() else {
            return Some("`bridge vlan show` cannot be read".to_string());
        };
        for (name, vids) in expected {
            for vid in vids {
                if !held.get(name).is_some_and(|port| port.contains_key(&vid)) {
                    return Some(format!("{name} is not in VLAN {vid}"));
                }
            }
        }
        None
    }

    /// Look for `br0` somewhere other than the kernel's `/sys/class/net`.
    #[cfg(test)]
    pub fn with_net_class(mut self, net_class: impl Into<PathBuf>) -> Self {
        self.net_class = net_class.into();
        self
    }

    /// Tests point confirm/heal at a temp interfaces file.
    #[cfg(test)]
    pub fn with_interfaces_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.interfaces_path = path.into();
        self
    }

    /// The switch is configured through `br0`. On a board whose
    /// `/etc/network/interfaces` has no such bridge there is nothing to
    /// configure, and the commands would fail one by one with the kernel's
    /// "Cannot find device"; this says so once, with what the board has.
    pub fn bridge_check(&self) -> Result<(), SwitchError> {
        if crate::app::network_interfaces::managed_present(&self.net_class) {
            return Ok(());
        }
        Err(SwitchError::NoBridge(
            crate::app::network_interfaces::missing_message(
                &self.net_class,
                &crate::app::network_interfaces::snapshot(&crate::app::address_applier::SystemExec),
                "The switch settings apply to br0, the bridge over the switch ports; edit \
                 /etc/network/interfaces over SSH, or restore the default layout.",
            ),
        ))
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
                 default network. interfaces heal is skipped."
            );
            return Ok(());
        }
        let turn = self.switch_turn().await;
        let mut inner = self.inner.lock().await;
        let Some(document) = inner.state.confirmed().cloned() else {
            return Ok(());
        };

        let (baseline, skip_rewrite) = with_interfaces_lock(|| -> Result<_, SwitchError> {
            let disk = std::fs::read_to_string(&self.interfaces_path).unwrap_or_default();
            match address_for_rewrite(&disk) {
                Ok(address) => {
                    let expected = render_with(&address, &document, self.bond_native());
                    if disk == expected {
                        return Ok((None, true));
                    }
                    let baseline = match switch_from_header(&disk) {
                        Some(from_file) => from_file,
                        None => {
                            // Image default / no header: ifup brought up stock
                            // Flat with independent uplinks, not a bond.
                            tracing::warn!(
                                "{} has no readable bmcd-switch header; planning from image \
                                 default flat",
                                self.interfaces_path.display()
                            );
                            Preset::flat().expand()
                        }
                    };
                    write_durably(&self.interfaces_path, expected.as_bytes())
                        .map_err(SwitchError::Io)?;
                    tracing::info!(
                        "healed {} from confirmed switch document",
                        self.interfaces_path.display()
                    );
                    Ok((Some(baseline), false))
                }
                Err(HandWrittenInterfaces) => {
                    tracing::warn!(
                        "{} is hand-written; switch heal will not rewrite it (live apply only)",
                        self.interfaces_path.display()
                    );
                    // A bond0 that is already up (a hand-written bond) is not
                    // ours to create again: plan from a baseline that has it.
                    let baseline =
                        if document.second_uplink == SecondUplink::Lag && self.bond_present() {
                            boot_baseline(&document)
                        } else {
                            Preset::flat().expand()
                        };
                    Ok((Some(baseline), true))
                }
            }
        })?;

        if skip_rewrite && baseline.is_none() {
            // The file matches, which says `ifup` was given the right lines,
            // not that it ran them. Look at the kernel before believing it.
            let Some(why) = self.kernel_drift(&document) else {
                inner.running = document;
                tracing::info!(
                    "switch configuration matches {} and the kernel; heal skipped",
                    self.interfaces_path.display()
                );
                return Ok(());
            };
            tracing::warn!(
                "{} matches the confirmed switch document but the kernel does not ({why}); \
                 running its boot lines once",
                self.interfaces_path.display()
            );
            // With no bond the plan has to make it; otherwise only the
            // `post-up` part is missing, which is what the file's lines are.
            let baseline = if document.second_uplink == SecondUplink::Lag && !self.bond_present() {
                Preset::flat().expand()
            } else {
                boot_baseline(&document)
            };
            let commands = plan(&baseline, &document);
            drop(inner);
            self.run_all_owning_turn(turn, commands, document).await?;
            return Ok(());
        }

        let baseline = baseline.unwrap_or_else(|| Preset::flat().expand());
        let commands = plan(&baseline, &document);
        drop(inner);
        self.run_all_owning_turn(turn, commands, document).await?;
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
    ///
    /// A hand-written `interfaces` file is only ever in the way of LACP.
    /// Every other document is applied live and persisted in `switch.json`, and
    /// at boot the file is left alone and the document applied from it, as it
    /// always was. LACP needs `bond0` declared in the file, so that a reboot
    /// can bring the bond up before this daemon starts; replacing the file is
    /// then a decision, and `replace_hand_written` is how the request makes it.
    pub async fn apply(
        &self,
        document: SwitchDocument,
        window: Duration,
        replace_hand_written: bool,
    ) -> Result<Pending, SwitchError> {
        let turn = self.switch_turn().await;
        self.bridge_check()?;
        let lag = document.second_uplink == SecondUplink::Lag;
        if lag && !self.bond_native() {
            return Err(SwitchError::MissingBondExecutor(
                "LACP needs ifupdown-ng's bond executor on this image so a reboot can bring \
                 bond0 up; install it, or use independent uplinks."
                    .into(),
            ));
        }

        let (hand_written, address_readable) = with_interfaces_lock(|| {
            let disk = std::fs::read_to_string(&self.interfaces_path).unwrap_or_default();
            (
                matches!(address_for_rewrite(&disk), Err(HandWrittenInterfaces)),
                parse(&disk).document.is_some(),
            )
        });
        if lag && hand_written {
            if !replace_hand_written {
                return Err(SwitchError::HandWritten(self.hand_written_message()));
            }
            if !address_readable {
                return Err(SwitchError::HandWritten(self.unreadable_address_message()));
            }
        }

        let mut inner = self.inner.lock().await;
        // A bond this daemon did not make is not one it may create again, and
        // not one a failed attempt may take down.
        if lag && inner.running.second_uplink != SecondUplink::Lag && self.bond_present() {
            return Err(SwitchError::BondExists(
                "bond0 already exists on this board and this daemon did not create it (a \
                 hand-written interfaces file, or an earlier run). Creating it again would fail, \
                 and undoing a failed attempt could take your bond down with it. Remove bond0, \
                 or keep managing it by hand; nothing was changed."
                    .into(),
            ));
        }

        inner.replaces_hand_written = hand_written && lag;
        let running = inner.running.clone();
        let token = new_token();
        let (pending, effect) =
            inner
                .state
                .apply(document, running, window, SystemTime::now(), token)?;

        if let Effect::Apply(document) = effect {
            let previous = inner.running.clone();
            let commands = plan(&previous, &document);
            // A failure here leaves the switch part-way. Undo what it did,
            // best-effort (steps that never ran usually fail too), and then
            // drop the pending change. What is undone is only what this apply
            // did: see [`undo_plan`]. All of it runs on one blocking thread
            // that owns the turn and records the outcome in `inner` itself, so
            // a client that goes away half-way leaves neither a second change
            // interleaved with these commands nor a `running` that was never
            // written. `inner` is not held meanwhile: one of the two workers
            // would otherwise sit inside `ip` or `bridge`.
            drop(inner);
            let exec = Arc::clone(&self.exec);
            let net_class = self.net_class.clone();
            let shared = Arc::clone(&self.inner);
            Self::run_owning_turn(turn, move || -> Result<(), SwitchError> {
                let (done, outcome) = run_plan(exec.as_ref(), &commands);
                if let Err(e) = outcome {
                    let bond_created = commands.iter().take(done).any(creates_bond);
                    let undo = undo_plan(&previous, &document, bond_created);
                    let _ = run_best_effort(exec.as_ref(), &undo);
                    let running = running_from_kernel(exec.as_ref(), &net_class, &previous);
                    let mut inner = shared.blocking_lock();
                    inner.running = running;
                    let _ = inner.state.revert(SystemTime::now());
                    return Err(e);
                }
                shared.blocking_lock().running = document;
                Ok(())
            })
            .await??;
        }
        Ok(pending)
    }

    fn hand_written_message(&self) -> String {
        let path = self.interfaces_path.display();
        format!(
            "{path} was written by hand, and LACP needs bond0 declared in it so that a reboot can \
             bring the bond up. A confirmed LACP change replaces the whole file (a copy of the \
             current one is kept as {path}.hand-written.<time>). To go ahead, tick the option to \
             replace the hand-written file in the interface, pass `--replace-hand-written` to \
             `tpi`, or send `replace_hand_written: true` in the request. Or keep the file and use \
             independent uplinks. Nothing was changed."
        )
    }

    fn unreadable_address_message(&self) -> String {
        let path = self.interfaces_path.display();
        format!(
            "{path} was written by hand and the address in its br0 stanza cannot be read (it is \
             neither DHCP nor a static address with a netmask or prefix), so replacing the file \
             would change how this board gets its address. Set the address through the address \
             settings first, or edit the file over SSH. Nothing was changed."
        )
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
    fn start_window_if_reachable(&self, inner: &mut Inner, now: SystemTime) {
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
            inner.state.uplink_forwarding(now);
            return;
        };

        let forwarding = if document.second_uplink == SecondUplink::Lag {
            is_forwarding(&self.net_class, BOND)
        } else {
            document
                .members(management)
                .into_iter()
                .filter(|port| port.is_uplink())
                .filter_map(interface_name)
                .any(|name| is_forwarding(&self.net_class, name))
        };

        if forwarding {
            tracing::info!("the uplink is forwarding; the confirm window starts now");
            inner.state.uplink_forwarding(now);
        }
    }

    pub async fn confirm(&self, token: &str) -> Result<(), SwitchError> {
        self.confirm_at(token, SystemTime::now()).await
    }

    /// [`confirm`](Self::confirm) at a given time.
    ///
    /// One timestamp for the check and the commit. Writing the two files takes
    /// as long as flash takes, and a clock read again afterwards could find the
    /// window gone: the commit would then refuse after both files were written,
    /// the timer would put the old document back, and the next boot would come
    /// up on a configuration nobody confirmed.
    pub(crate) async fn confirm_at(&self, token: &str, now: SystemTime) -> Result<(), SwitchError> {
        let _turn = self.switch_turn().await;
        let mut inner = self.inner.lock().await;
        // Check first, write both files, and only then commit. A refusal or a
        // write that fails leaves the change pending: it can be confirmed
        // again, or the window puts the old configuration back. `interfaces`
        // goes first: if `switch.json` then fails, boot heal plans from the
        // header back to the confirmed document, whereas the other order would
        // leave a confirmed document nobody confirmed.
        let document = inner.state.check_confirm(token, now)?;
        let keep_copy = inner.replaces_hand_written;
        with_interfaces_lock(|| self.write_interfaces_twin(&document, keep_copy))?;
        write_durably(
            &self.path,
            &serde_json::to_vec_pretty(&document)
                .map_err(|e| SwitchError::Io(std::io::Error::other(e)))?,
        )
        .map_err(SwitchError::Io)?;
        inner.state.confirm(token, now)?;
        inner.replaces_hand_written = false;
        tracing::info!(
            "switch configuration confirmed and persisted ({}, {})",
            self.path.display(),
            self.interfaces_path.display()
        );
        Ok(())
    }

    /// Rewrite `interfaces` for `document`, keeping the address stanza. A
    /// hand-written file is replaced only for LACP, only with the opt-in from
    /// apply, and only after a copy is kept; a write that fails removes that
    /// copy again. For any other document a hand-written file is left alone.
    fn write_interfaces_twin(
        &self,
        document: &SwitchDocument,
        keep_copy: bool,
    ) -> Result<(), SwitchError> {
        let disk = std::fs::read_to_string(&self.interfaces_path).unwrap_or_default();
        let address = match address_for_rewrite(&disk) {
            Ok(address) => address,
            // Nothing in this document needs the file: it is applied live and
            // at boot from `switch.json`, exactly as before the file was ever
            // written by this daemon. Not ours to replace.
            Err(HandWrittenInterfaces) if document.second_uplink != SecondUplink::Lag => {
                return Ok(());
            }
            // Opt-in already happened at apply. Keep the address from the hand
            // file when it can be read; never turn one that cannot into DHCP.
            Err(HandWrittenInterfaces) if keep_copy => match parse(&disk).document {
                Some(address) => address,
                None => {
                    return Err(SwitchError::HandWritten(self.unreadable_address_message()));
                }
            },
            Err(HandWrittenInterfaces) => {
                return Err(SwitchError::HandWritten(self.hand_written_message()))
            }
        };
        let mut backup = None;
        if keep_copy && self.interfaces_path.exists() {
            let (kept, removed) =
                keep_a_copy_for_switch(&self.interfaces_path).map_err(SwitchError::Io)?;
            tracing::warn!(
                "{} was not written by this daemon; a copy is kept as {}",
                self.interfaces_path.display(),
                kept.display()
            );
            for old in removed {
                tracing::warn!("removed the older copy {}", old.display());
            }
            backup = Some(kept);
        }
        if let Err(e) = write_interfaces_with(
            &self.interfaces_path,
            &address,
            document,
            self.bond_native(),
        ) {
            if let Some(kept) = &backup {
                let _ = std::fs::remove_file(kept);
            }
            return Err(SwitchError::Io(e));
        }
        Ok(())
    }

    pub async fn revert(&self) -> Result<(), SwitchError> {
        let turn = self.switch_turn().await;
        let (effect, running) = {
            let mut inner = self.inner.lock().await;
            let effect = inner.state.revert(SystemTime::now())?;
            let running = inner.running.clone();
            (effect, running)
        };
        self.carry_out(turn, effect, running).await
    }

    /// Called on a timer. Starts a window whose uplink has come up, and puts
    /// an unconfirmed change back once its window has passed.
    pub async fn tick(&self) {
        self.tick_at(SystemTime::now()).await
    }

    /// [`tick`](Self::tick) at a given time.
    pub(crate) async fn tick_at(&self, now: SystemTime) {
        let turn = self.switch_turn().await;
        let (effect, running) = {
            let mut inner = self.inner.lock().await;
            self.start_window_if_reachable(&mut inner, now);
            let effect = inner.state.tick(now);
            if matches!(effect, Effect::None) {
                return;
            }
            tracing::warn!("switch change was not confirmed in its window; putting the previous configuration back");
            (effect, inner.running.clone())
        };
        if let Err(e) = self.carry_out(turn, effect, running).await {
            // There is nothing above this to tell. Saying so loudly is the
            // whole of the response.
            tracing::error!("could not put the previous switch configuration back: {e}");
        }
    }

    /// Put a document back on the switch, for a revert.
    ///
    /// Best-effort, unlike an apply: every command is tried even after one
    /// fails. A revert that stopped at the first error would leave ge0 and ge1
    /// half-moved between the bond and the bridge and the BMC off the network,
    /// with nothing above to try again. And `running` is then read back from
    /// the kernel rather than taken from the document, because after a failure
    /// the document is not what is running.
    ///
    /// The thread that runs the commands stores what is running afterwards,
    /// once they have finished, so the switch lock is not held while `ip` or
    /// `bridge` runs, and the turn is held until it has.
    async fn carry_out(
        &self,
        turn: SwitchTurn,
        effect: Effect,
        running: SwitchDocument,
    ) -> Result<(), SwitchError> {
        let Effect::Apply(document) = effect else {
            return Ok(());
        };
        let commands = plan(&running, &document);
        let exec = Arc::clone(&self.exec);
        let net_class = self.net_class.clone();
        let shared = Arc::clone(&self.inner);
        Self::run_owning_turn(turn, move || -> Result<(), SwitchError> {
            let outcome = run_best_effort(exec.as_ref(), &commands);
            let next = match &outcome {
                Ok(()) => document,
                Err(_) => running_from_kernel(exec.as_ref(), &net_class, &document),
            };
            shared.blocking_lock().running = next;
            outcome
        })
        .await?
    }

    /// Run a plan to the end, stopping at the first failure, and record
    /// `document` as running if it all went through. For the boot, which has
    /// nothing to undo.
    async fn run_all_owning_turn(
        &self,
        turn: SwitchTurn,
        commands: Vec<Command>,
        document: SwitchDocument,
    ) -> Result<(), SwitchError> {
        let exec = Arc::clone(&self.exec);
        let shared = Arc::clone(&self.inner);
        Self::run_owning_turn(turn, move || -> Result<(), SwitchError> {
            run_plan(exec.as_ref(), &commands).1?;
            shared.blocking_lock().running = document;
            Ok(())
        })
        .await?
    }
}

/// Where the kernel exposes network interfaces.
pub const NET_CLASS: &str = "/sys/class/net";

fn bond_present_in(net_class: &Path) -> bool {
    net_class.join(BOND).exists()
}

fn bridge_flag_in(net_class: &Path, name: &str) -> Option<u32> {
    std::fs::read_to_string(net_class.join(BRIDGE).join("bridge").join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn kernel_filtering_in(net_class: &Path) -> Option<bool> {
    bridge_flag_in(net_class, "vlan_filtering").map(|v| v != 0)
}

/// Whether `br0` runs spanning tree: the kernel's `stp_state` is 0 (off),
/// 1 (kernel STP) or 2 (user-space STP).
fn kernel_stp_in(net_class: &Path) -> Option<bool> {
    bridge_flag_in(net_class, "stp_state").map(|v| v != 0)
}

fn kernel_vlans_from(exec: &dyn Exec) -> Option<KernelVlans> {
    let (program, args) = SHOW_VLANS.split_first()?;
    match exec.run(program, args) {
        Ok(out) => parse_bridge_vlans(&out),
        Err(e) => {
            tracing::warn!("could not read the bridge VLANs: {e}");
            None
        }
    }
}

/// The switch as the kernel reports it, for after a plan that did not run
/// clean: what `fallback` says is running is then a guess, and the next plan
/// would be computed from it. Falls back to `fallback` when the kernel cannot
/// be read. `bridge -j vlan show` is a process: call this on a blocking thread.
fn running_from_kernel(
    exec: &dyn Exec,
    net_class: &Path,
    fallback: &SwitchDocument,
) -> SwitchDocument {
    let (Some(vlans), Some(filtering)) = (kernel_vlans_from(exec), kernel_filtering_in(net_class))
    else {
        tracing::warn!("could not read the switch back from the kernel; keeping the document");
        return fallback.clone();
    };
    let stp = kernel_stp_in(net_class).unwrap_or(fallback.stp);
    document_from_kernel(&vlans, filtering, stp, bond_present_in(net_class), fallback)
}

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

/// Run one command through `exec`.
fn run_command(exec: &dyn Exec, command: &Command) -> Result<(), SwitchError> {
    let (program, args) = command.split_first().expect("a command has a program");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    exec.run(program, &args).map(|_| ()).map_err(|e| match e {
        ApplyError::Command { detail, .. } => SwitchError::Command {
            command: command.join(" "),
            detail,
        },
        ApplyError::Io(e) => SwitchError::Io(e),
    })
}

/// Run the plan, stopping at the first failure. Returns how many commands were
/// done, and how it ended.
///
/// Each command is an ioctl with a command line in front of it. Callers run
/// the whole plan on a blocking thread, one task for the plan, not one task
/// per command.
fn run_plan(exec: &dyn Exec, commands: &[Command]) -> (usize, Result<(), SwitchError>) {
    for (done, command) in commands.iter().enumerate() {
        match run_command(exec, command) {
            Err(SwitchError::Command { detail, .. }) if is_best_effort(command) => {
                tracing::warn!("{} failed, carrying on: {detail}", command.join(" "));
            }
            Err(e) => return (done, Err(e)),
            Ok(()) => {}
        }
    }
    (commands.len(), Ok(()))
}

/// Run every command, going past errors so later steps still run. The first
/// error is returned, after the rest has been tried.
fn run_best_effort(exec: &dyn Exec, commands: &[Command]) -> Result<(), SwitchError> {
    let mut first = None;
    for command in commands {
        if let Err(e) = run_command(exec, command) {
            tracing::warn!("switch step failed, carrying on: {e}");
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::address_document::AddressDocument;
    use crate::app::switch_applier::show_vlans_json;
    use crate::app::switch_change::START_CEILING;
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
        assert_eq!(view.running, Preset::flat().expand());
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
            Preset::flat().expand(),
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

    /// A board with no `br0` is told so, once, and nothing is run.
    #[tokio::test]
    async fn without_br0_an_apply_is_refused_before_anything_is_run() {
        let dir = tempdir::TempDir::new("switch-nobridge").expect("temp dir");
        let net = dir.path().join("net");
        std::fs::create_dir_all(net.join("br-wan/bridge")).unwrap();
        let service = SwitchService::load(dir.path().join("switch.json")).with_net_class(&net);
        let error = service
            .apply(Preset::Split.expand(), DEFAULT_WINDOW, false)
            .await
            .expect_err("no br0");
        let SwitchError::NoBridge(message) = error else {
            panic!("expected NoBridge, got {error}");
        };
        assert!(
            message.starts_with("This board has no br0 bridge (it has:"),
            "{message}"
        );
        assert!(message.contains("br-wan"), "{message}");
        assert!(service.view().await.pending.is_none());
        assert!(service.view().await.last_revert.is_none());

        std::fs::create_dir_all(net.join("br0")).unwrap();
        assert!(service.bridge_check().is_ok());
    }

    #[test]
    fn a_token_is_unique_and_hex() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn confirm_dual_writes_interfaces_with_switch_header() {
        let dir = tempdir::TempDir::new("switch-confirm-ifaces").expect("temp dir");
        let interfaces = dir.path().join("interfaces");
        // Persist path shared by confirm: header + bond stanza round-trip.
        let document = lag_flat();
        write_interfaces_with(&interfaces, &AddressDocument::Dhcp, &document, true).unwrap();
        let text = std::fs::read_to_string(&interfaces).unwrap();
        assert!(text.contains(crate::app::switch_interfaces::SWITCH_HEADER_PREFIX));
        assert!(text.contains("auto bond0"));
        assert_eq!(switch_from_header(&text).expect("header"), document);
    }

    // ---- fixtures for what runs and what the kernel says ------------------

    fn lag_flat() -> SwitchDocument {
        Preset::Flat {
            second_uplink: SecondUplink::Lag,
        }
        .expand()
    }

    fn lag_trunk() -> SwitchDocument {
        Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Lag,
        }
        .expand()
    }

    /// Stands in for `ip` and `bridge`: records every command, fails the ones
    /// that match a fragment it was told to fail, and answers `bridge -j vlan
    /// show` with whatever it was given.
    #[derive(Default)]
    struct FakeExec {
        calls: std::sync::Mutex<Vec<String>>,
        failing: std::sync::Mutex<Vec<String>>,
        vlans: std::sync::Mutex<String>,
    }

    impl FakeExec {
        fn new() -> Arc<Self> {
            Arc::new(FakeExec::default())
        }
        fn fail(&self, fragment: &str) {
            self.failing.lock().unwrap().push(fragment.to_string());
        }
        fn set_vlans(&self, json: &str) {
            *self.vlans.lock().unwrap() = json.to_string();
        }
        /// Every command so far except the read of the VLAN listing.
        fn ran(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.as_str() != "bridge -j vlan show")
                .cloned()
                .collect()
        }
        fn forget(&self) {
            self.calls.lock().unwrap().clear();
        }
        fn ran_any(&self, fragment: &str) -> bool {
            self.ran().iter().any(|c| c.contains(fragment))
        }
    }

    impl Exec for FakeExec {
        fn run(&self, program: &str, args: &[&str]) -> Result<String, ApplyError> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.lock().unwrap().push(line.clone());
            if line == "bridge -j vlan show" {
                return Ok(self.vlans.lock().unwrap().clone());
            }
            if self
                .failing
                .lock()
                .unwrap()
                .iter()
                .any(|f| line.contains(f))
            {
                return Err(ApplyError::Command {
                    command: line,
                    detail: "refused".into(),
                });
            }
            Ok(String::new())
        }
    }

    /// A board's `/sys/class/net`, as far as this service reads it.
    fn net(dir: &Path, filtering: bool, bond: bool) -> PathBuf {
        let net = dir.join("net");
        std::fs::create_dir_all(net.join("br0").join("bridge")).unwrap();
        let bridge = net.join("br0").join("bridge");
        std::fs::write(
            bridge.join("vlan_filtering"),
            if filtering { "1\n" } else { "0\n" },
        )
        .unwrap();
        std::fs::write(bridge.join("stp_state"), "0\n").unwrap();
        if bond {
            std::fs::create_dir_all(net.join("bond0")).unwrap();
        }
        net
    }

    /// A file standing for ifupdown-ng's bond executor.
    fn executor(dir: &Path) -> Vec<PathBuf> {
        let path = dir.join("bond-executor");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        vec![path]
    }

    /// A service on a fake board: the kernel's view, the files, the commands.
    struct Rig {
        dir: tempdir::TempDir,
        service: SwitchService,
        exec: Arc<FakeExec>,
        interfaces: PathBuf,
        switch: PathBuf,
    }

    fn rig(name: &str, filtering: bool, bond: bool, interfaces: Option<&str>) -> Rig {
        let dir = tempdir::TempDir::new(name).expect("temp dir");
        let exec = FakeExec::new();
        let interfaces_path = dir.path().join("interfaces");
        if let Some(text) = interfaces {
            std::fs::write(&interfaces_path, text).unwrap();
        }
        let switch = dir.path().join("switch.json");
        let service = SwitchService::load(&switch)
            .with_interfaces_path(&interfaces_path)
            .with_net_class(net(dir.path(), filtering, bond))
            .with_bond_executors(executor(dir.path()))
            .with_exec(exec.clone());
        Rig {
            dir,
            service,
            exec,
            interfaces: interfaces_path,
            switch,
        }
    }

    /// A rig whose confirmed document is `document`, with its `interfaces`
    /// rendered for it, so that boot heal sees a file that matches.
    fn booted(name: &str, document: &SwitchDocument, filtering: bool, bond: bool) -> Rig {
        let dir = tempdir::TempDir::new(name).expect("temp dir");
        let exec = FakeExec::new();
        let interfaces_path = dir.path().join("interfaces");
        let executors = executor(dir.path());
        std::fs::write(
            &interfaces_path,
            render_with(&AddressDocument::Dhcp, document, true),
        )
        .unwrap();
        let switch = dir.path().join("switch.json");
        std::fs::write(&switch, serde_json::to_vec_pretty(document).unwrap()).unwrap();
        let service = SwitchService::load(&switch)
            .with_interfaces_path(&interfaces_path)
            .with_net_class(net(dir.path(), filtering, bond))
            .with_bond_executors(executors)
            .with_exec(exec.clone());
        Rig {
            dir,
            service,
            exec,
            interfaces: interfaces_path,
            switch,
        }
    }

    // ---- boot heal --------------------------------------------------------

    /// The file and the kernel both match: nothing is run but the read.
    #[tokio::test]
    async fn boot_heal_skips_when_interfaces_and_the_kernel_both_match() {
        let document = trunk();
        let rig = booted("switch-heal-match", &document, true, false);
        rig.exec.set_vlans(&show_vlans_json(&document));
        let text = std::fs::read_to_string(&rig.interfaces).unwrap();

        rig.service.boot_apply().await.expect("heal");
        assert_eq!(std::fs::read_to_string(&rig.interfaces).unwrap(), text);
        assert_eq!(rig.exec.ran(), Vec::<String>::new());
        assert_eq!(rig.service.view().await.running, document);
    }

    /// `ifup br0` stopped before its `post-up` lines ran (DHCP failing on an
    /// unfiltered bridge is enough): the file is right and the switch is not.
    #[tokio::test]
    async fn boot_heal_runs_the_boot_lines_when_the_file_matches_but_filtering_is_off() {
        let document = trunk();
        let rig = booted("switch-heal-nofilter", &document, false, false);
        rig.exec.set_vlans("[]");

        rig.service.boot_apply().await.expect("heal");
        assert!(
            rig.exec.ran_any("vlan_filtering 1"),
            "{:#?}",
            rig.exec.ran()
        );
        assert!(
            rig.exec
                .ran()
                .contains(&"bridge vlan add dev node1 vid 20 pvid untagged".to_string()),
            "{:#?}",
            rig.exec.ran()
        );
        assert_eq!(rig.service.view().await.running, document);
    }

    #[tokio::test]
    async fn boot_heal_runs_the_boot_lines_when_a_front_port_lacks_its_vlans() {
        let document = trunk();
        let rig = booted("switch-heal-missing", &document, true, false);
        // Everything is there but node1's VLAN.
        let mut partial = document.clone();
        partial.ports.insert(PortId::Node1, Default::default());
        rig.exec.set_vlans(&show_vlans_json(&partial));

        rig.service.boot_apply().await.expect("heal");
        assert!(
            rig.exec
                .ran()
                .contains(&"bridge vlan add dev node1 vid 20 pvid untagged".to_string()),
            "{:#?}",
            rig.exec.ran()
        );
    }

    /// A listing that cannot be read is not evidence that all is well.
    #[tokio::test]
    async fn boot_heal_does_not_take_an_unreadable_listing_for_a_match() {
        let document = trunk();
        let rig = booted("switch-heal-unreadable", &document, true, false);
        rig.exec.set_vlans("not json at all");
        rig.service.boot_apply().await.expect("heal");
        assert!(rig.exec.ran_any("bridge vlan add"), "{:#?}", rig.exec.ran());
    }

    /// LACP on a board where the bond never came up: heal makes it.
    #[tokio::test]
    async fn boot_heal_makes_the_bond_when_the_file_matches_but_bond0_is_missing() {
        let document = lag_flat();
        let rig = booted("switch-heal-nobond", &document, false, false);
        rig.service.boot_apply().await.expect("heal");
        assert!(
            rig.exec.ran_any("ip link add bond0"),
            "{:#?}",
            rig.exec.ran()
        );
        assert_eq!(rig.service.view().await.running, document);
    }

    #[tokio::test]
    async fn boot_heal_leaves_a_working_bond_alone() {
        let document = lag_flat();
        let rig = booted("switch-heal-bond", &document, false, true);
        rig.service.boot_apply().await.expect("heal");
        assert_eq!(rig.exec.ran(), Vec::<String>::new());
    }

    /// A hand-written file is left alone at boot. The confirmed document is
    /// Flat, so the plan from the image's Flat is empty.
    #[tokio::test]
    async fn boot_heal_does_not_rewrite_a_hand_written_file() {
        let document = Preset::flat().expand();
        let rig = rig("switch-heal-hand", false, false, Some(HAND_FILE));
        std::fs::write(&rig.switch, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        let service = SwitchService::load(&rig.switch)
            .with_interfaces_path(&rig.interfaces)
            .with_net_class(net(rig.dir.path(), false, false))
            .with_exec(rig.exec.clone());
        service.boot_apply().await.expect("nothing to run");
        assert_eq!(std::fs::read_to_string(&rig.interfaces).unwrap(), HAND_FILE);
        assert_eq!(service.view().await.running, document);
        assert_eq!(rig.exec.ran(), Vec::<String>::new());
    }

    /// A bond the hand-written file made is not made a second time.
    #[tokio::test]
    async fn boot_heal_does_not_remake_a_bond_a_hand_written_file_made() {
        let document = lag_flat();
        let rig = rig("switch-heal-hand-bond", false, true, Some(HAND_FILE));
        std::fs::write(&rig.switch, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        let service = SwitchService::load(&rig.switch)
            .with_interfaces_path(&rig.interfaces)
            .with_net_class(net(rig.dir.path(), false, true))
            .with_exec(rig.exec.clone());
        service.boot_apply().await.expect("heal");
        assert!(!rig.exec.ran_any("link add bond0"), "{:#?}", rig.exec.ran());
        assert_eq!(std::fs::read_to_string(&rig.interfaces).unwrap(), HAND_FILE);
    }

    const HAND_FILE: &str =
        "auto br0\niface br0 inet static\n  address 10.1.2.3\n  netmask 255.255.0.0\n";

    /// A hand-written file whose address this daemon cannot read.
    const HAND_FILE_UNREADABLE: &str = "auto br0\niface br0 inet manual\n  bridge-ports ge0\n";

    /// A pending change, as `apply` would leave it, without running the plan.
    async fn pending(service: &SwitchService, document: SwitchDocument) -> String {
        let mut inner = service.inner.lock().await;
        let running = inner.running.clone();
        let token = new_token();
        inner
            .state
            .apply(
                document,
                running,
                DEFAULT_WINDOW,
                SystemTime::now(),
                token.clone(),
            )
            .expect("a valid document");
        token
    }

    // ---- hand-written interfaces ------------------------------------------

    /// Only LACP needs the file, so only LACP is held up by a hand-written
    /// one: any other change is applied and confirmed, and the file is left as
    /// the person wrote it, which is what happened before this daemon wrote it
    /// at all.
    #[tokio::test]
    async fn a_hand_written_file_does_not_stand_in_the_way_of_a_change_that_is_not_lacp() {
        let rig = rig("switch-hand-trunk", false, false, Some(HAND_FILE));
        let pending = rig
            .service
            .apply(trunk(), DEFAULT_WINDOW, false)
            .await
            .expect("no opt-in is needed");
        rig.service.confirm(&pending.token).await.expect("confirm");

        assert_eq!(
            std::fs::read_to_string(&rig.interfaces).unwrap(),
            HAND_FILE,
            "the hand-written file is not replaced"
        );
        assert!(
            !rig.dir
                .path()
                .read_dir()
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains(".hand-written.")),
            "and so no copy of it is made"
        );
        assert_eq!(
            SwitchService::load(&rig.switch).view().await.confirmed,
            Some(trunk()),
            "the document is persisted, as it always was"
        );
    }

    /// LACP over a hand-written file is refused up front, before anything is
    /// run, and the message names what a client can do about it.
    #[tokio::test]
    async fn lacp_over_a_hand_written_file_is_refused_and_says_how_to_go_ahead() {
        let rig = rig("switch-hand-lag", false, false, Some(HAND_FILE));
        let error = rig
            .service
            .apply(lag_flat(), DEFAULT_WINDOW, false)
            .await
            .expect_err("hand-written");
        let SwitchError::HandWritten(message) = &error else {
            panic!("expected HandWritten, got {error}");
        };
        assert!(message.contains("--replace-hand-written"), "{message}");
        assert!(message.contains("replace_hand_written"), "{message}");
        assert!(message.contains("interface"), "{message}");
        assert!(rig.exec.ran().is_empty(), "nothing was run");
        assert!(rig.service.view().await.pending.is_none());
    }

    /// With the opt-in, LACP goes ahead; confirm keeps a copy, writes the
    /// bond into the file with the address of the hand-written one, then
    /// `switch.json`, then commits.
    #[tokio::test]
    async fn lacp_with_the_opt_in_keeps_a_copy_and_writes_both_files() {
        let rig = rig("switch-hand-lag-ok", false, false, Some(HAND_FILE));
        let pending = rig
            .service
            .apply(lag_flat(), DEFAULT_WINDOW, true)
            .await
            .expect("the opt-in is given");
        rig.service.confirm(&pending.token).await.expect("confirm");

        let view = rig.service.view().await;
        assert!(view.pending.is_none());
        assert_eq!(view.confirmed, Some(lag_flat()));
        let text = std::fs::read_to_string(&rig.interfaces).unwrap();
        assert_eq!(switch_from_header(&text), Some(lag_flat()));
        assert!(text.contains("address 10.1.2.3"), "{text}");
        assert!(text.contains("auto bond0"), "{text}");
        let copies: Vec<_> = std::fs::read_dir(rig.dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".hand-written."))
            .collect();
        assert_eq!(copies.len(), 1, "{copies:?}");
    }

    /// An address that cannot be read is not turned into DHCP: the board
    /// would come back from the next boot on a different address.
    #[tokio::test]
    async fn an_address_that_cannot_be_read_is_refused_not_turned_into_dhcp() {
        let rig = rig(
            "switch-hand-unreadable",
            false,
            false,
            Some(HAND_FILE_UNREADABLE),
        );
        let error = rig
            .service
            .apply(lag_flat(), DEFAULT_WINDOW, true)
            .await
            .expect_err("the address cannot be read");
        let SwitchError::HandWritten(message) = &error else {
            panic!("expected HandWritten, got {error}");
        };
        assert!(message.contains("cannot be read"), "{message}");
        assert!(rig.exec.ran().is_empty(), "nothing was run");
        assert_eq!(
            std::fs::read_to_string(&rig.interfaces).unwrap(),
            HAND_FILE_UNREADABLE
        );
    }

    /// The same, if the file only turned hand-written after the apply: confirm
    /// refuses and leaves everything as it was.
    #[tokio::test]
    async fn confirm_over_a_hand_written_file_without_the_opt_in_persists_nothing() {
        let rig = rig("switch-confirm-hand", false, false, Some(HAND_FILE));
        let token = pending(&rig.service, lag_trunk()).await;

        let error = rig.service.confirm(&token).await.expect_err("hand-written");
        assert!(matches!(error, SwitchError::HandWritten(_)), "{error}");
        assert!(rig.service.view().await.pending.is_some(), "still pending");
        assert!(!rig.switch.exists(), "switch.json not written");
        assert_eq!(std::fs::read_to_string(&rig.interfaces).unwrap(), HAND_FILE);
    }

    /// A failed `interfaces` write leaves the change pending, writes no
    /// `switch.json`, and does not leave the copy of the hand file behind.
    #[tokio::test]
    async fn a_failed_interfaces_write_leaves_the_change_pending() {
        let dir = tempdir::TempDir::new("switch-confirm-fail").expect("temp dir");
        let switch_path = dir.path().join("switch.json");
        let interfaces = dir.path().join("interfaces");
        std::fs::write(&interfaces, HAND_FILE).unwrap();
        // The write's temp file is `<name>.tmp`. A directory of that name
        // cannot be removed as a stale file, so the write fails, for root too,
        // after the copy of the hand file has been made.
        std::fs::create_dir(dir.path().join("interfaces.tmp")).unwrap();
        let service = SwitchService::load(&switch_path)
            .with_interfaces_path(&interfaces)
            .with_bond_executors(executor(dir.path()));
        let token = pending(&service, lag_trunk()).await;
        service.inner.lock().await.replaces_hand_written = true;

        let error = service.confirm(&token).await.expect_err("the write fails");
        assert!(matches!(error, SwitchError::Io(_)), "{error}");
        assert!(service.view().await.pending.is_some(), "still pending");
        assert!(!switch_path.exists(), "switch.json not written");
        assert_eq!(std::fs::read_to_string(&interfaces).unwrap(), HAND_FILE);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| !n.contains(".hand-written.")),
            "{names:?}"
        );
    }

    // ---- confirm: one clock -----------------------------------------------

    /// The check and the commit read one timestamp. A confirm at a moment
    /// inside the window is honoured however long the writes in between took;
    /// one past the ceiling is refused with nothing written.
    #[tokio::test]
    async fn confirm_judges_the_window_at_the_one_time_it_is_given() {
        let rig = rig("switch-confirm-clock", false, false, None);
        let pending = rig
            .service
            .apply(trunk(), DEFAULT_WINDOW, false)
            .await
            .expect("apply");

        let late = pending.applied_at + DEFAULT_WINDOW + START_CEILING + Duration::from_secs(1);
        let error = rig
            .service
            .confirm_at(&pending.token, late)
            .await
            .expect_err("past the ceiling");
        assert!(
            matches!(error, SwitchError::Change(ChangeError::NothingPending)),
            "{error}"
        );
        assert!(!rig.switch.exists(), "nothing written for a late confirm");

        let in_time = pending.applied_at + DEFAULT_WINDOW;
        rig.service
            .confirm_at(&pending.token, in_time)
            .await
            .expect("inside the window");
        assert!(rig.switch.exists());
    }

    // ---- LACP apply, failure and undo -------------------------------------

    /// A bond0 this daemon did not make is not created again, and nothing is
    /// run: a failed attempt could otherwise take it down.
    #[tokio::test]
    async fn lacp_is_refused_while_a_bond0_that_is_not_ours_exists() {
        let rig = rig("switch-bond-exists", false, true, None);
        let error = rig
            .service
            .apply(lag_flat(), DEFAULT_WINDOW, false)
            .await
            .expect_err("bond0 exists");
        let SwitchError::BondExists(message) = &error else {
            panic!("expected BondExists, got {error}");
        };
        assert!(message.contains("did not create it"), "{message}");
        assert!(rig.exec.ran().is_empty(), "{:#?}", rig.exec.ran());
        assert!(rig.service.view().await.pending.is_none());
    }

    /// ...but a bond0 it did make (the running document is LACP) can be
    /// reconfigured.
    #[tokio::test]
    async fn a_running_lacp_document_may_be_changed_with_its_bond_in_place() {
        let rig = booted("switch-bond-running", &lag_flat(), false, true);
        rig.service.boot_apply().await.expect("heal");
        rig.service
            .apply(lag_trunk(), DEFAULT_WINDOW, false)
            .await
            .expect("bond0 is ours: it is the running document's");
        assert!(!rig.exec.ran_any("link add bond0"), "{:#?}", rig.exec.ran());
    }

    /// The failed apply that never made the bond must not take one down.
    #[tokio::test]
    async fn a_failed_apply_that_never_made_the_bond_does_not_tear_one_down() {
        let rig = rig("switch-undo-nobond", false, false, None);
        rig.exec.fail("link add bond0");
        let error = rig
            .service
            .apply(lag_flat(), DEFAULT_WINDOW, false)
            .await
            .expect_err("the bond cannot be made");
        assert!(matches!(error, SwitchError::Command { .. }), "{error}");

        assert!(
            !rig.exec.ran_any("link delete bond0"),
            "{:#?}",
            rig.exec.ran()
        );
        assert!(!rig.exec.ran_any("nomaster"), "{:#?}", rig.exec.ran());
        assert!(
            rig.exec
                .ran()
                .iter()
                .any(|c| c == "bridge vlan add dev ge0 vid 1 pvid untagged"),
            "the VLANs the plan removed are put back: {:#?}",
            rig.exec.ran()
        );
        let view = rig.service.view().await;
        assert!(view.pending.is_none());
        assert_eq!(view.running, Preset::flat().expand());
    }

    /// The failed apply that did make it, and then failed, takes it down.
    #[tokio::test]
    async fn a_failed_apply_that_made_the_bond_takes_it_down_again() {
        let rig = rig("switch-undo-bond", false, false, None);
        rig.exec.fail("dev ge1 master bond0");
        let error = rig
            .service
            .apply(lag_flat(), DEFAULT_WINDOW, false)
            .await
            .expect_err("ge1 cannot join");
        assert!(matches!(error, SwitchError::Command { .. }), "{error}");
        assert!(
            rig.exec.ran_any("link delete bond0"),
            "{:#?}",
            rig.exec.ran()
        );
        assert!(
            rig.exec
                .ran()
                .contains(&"ip link set dev ge0 master br0".to_string()),
            "{:#?}",
            rig.exec.ran()
        );
    }

    // ---- the timer and the revert path ------------------------------------

    /// An uplink that never forwards is put back at the ceiling, through the
    /// same path the timer uses, and the window never having started is not
    /// a reason to wait for ever.
    #[tokio::test]
    async fn an_uplink_that_never_forwards_is_reverted_by_the_timer_at_the_ceiling() {
        let rig = rig("switch-timer", false, false, None);
        let pending = rig
            .service
            .apply(lag_trunk(), DEFAULT_WINDOW, false)
            .await
            .expect("apply");
        assert_eq!(rig.service.view().await.running, lag_trunk());

        // Inside the ceiling: waiting on an uplink that has not come up.
        rig.service
            .tick_at(pending.applied_at + DEFAULT_WINDOW + Duration::from_secs(59))
            .await;
        assert!(rig.service.view().await.pending.is_some());

        rig.exec.forget();
        rig.service
            .tick_at(pending.applied_at + DEFAULT_WINDOW + START_CEILING)
            .await;
        let view = rig.service.view().await;
        assert!(view.pending.is_none(), "put back");
        assert_eq!(
            view.last_revert.map(|r| r.reason),
            Some(crate::app::switch_change::RevertReason::NotConfirmed)
        );
        assert_eq!(view.running, Preset::flat().expand());
        assert!(
            rig.exec.ran_any("link delete bond0"),
            "{:#?}",
            rig.exec.ran()
        );
    }

    /// Flat to LACP and a revert back, and again: each trip does what the
    /// first did, and the document the board reports is the one it holds.
    #[tokio::test]
    async fn lag_then_flat_then_lag_again_goes_through_the_service() {
        let rig = rig("switch-roundtrip", false, false, None);
        for trip in 0..2 {
            rig.service
                .apply(lag_trunk(), DEFAULT_WINDOW, false)
                .await
                .unwrap_or_else(|e| panic!("trip {trip}: {e}"));
            assert_eq!(rig.service.view().await.running, lag_trunk());
            rig.service.revert().await.expect("revert");
            assert_eq!(rig.service.view().await.running, Preset::flat().expand());
        }
        let made = rig
            .exec
            .ran()
            .iter()
            .filter(|c| c.starts_with("ip link add bond0"))
            .count();
        assert_eq!(made, 2, "{:#?}", rig.exec.ran());
    }

    /// A revert that meets a failure goes on with the rest instead of leaving
    /// the ports half-moved, and what it reports as running is what the kernel
    /// says, not what it meant.
    #[tokio::test]
    async fn a_revert_carries_on_past_a_failure_and_reports_what_the_kernel_holds() {
        let rig = rig("switch-revert-best-effort", true, false, None);
        rig.service
            .apply(trunk(), DEFAULT_WINDOW, false)
            .await
            .expect("apply");

        rig.exec.fail("bridge vlan del dev ge0 vid 10");
        // After the revert the bridge is not filtering, and ge0 kept VLAN 10.
        std::fs::write(rig.dir.path().join("net/br0/bridge/vlan_filtering"), "0\n").unwrap();
        rig.exec.set_vlans(
            r#"[{"ifname":"ge0","vlans":[{"vlan":1,"flags":["PVID","Egress Untagged"]},{"vlan":10}]}]"#,
        );
        rig.exec.forget();

        let error = rig.service.revert().await.expect_err("one step fails");
        assert!(matches!(error, SwitchError::Command { .. }), "{error}");
        assert!(
            rig.exec
                .ran()
                .contains(&"bridge vlan add dev ge1 vid 1 pvid untagged".to_string()),
            "the steps after the failure still ran: {:#?}",
            rig.exec.ran()
        );
        let running = rig.service.view().await.running;
        assert!(!running.vlan_filtering);
        assert_eq!(
            running.ports[&PortId::Ge0],
            crate::app::switch_document::PortConfig {
                untagged: Some(1),
                tagged: [10].into_iter().collect()
            }
        );
    }

    // ---- confirm: a switch.json that cannot be written --------------------

    /// Stop `write_durably` from writing `path`: its temp file is `<name>.tmp`,
    /// and a directory of that name cannot be removed as a stale file, so the
    /// write fails for root too.
    fn block_writes_to(path: &Path) -> PathBuf {
        let mut name = path.file_name().expect("a file name").to_os_string();
        name.push(".tmp");
        let blocker = path.with_file_name(name);
        std::fs::create_dir(&blocker).unwrap();
        blocker
    }

    /// The write of `switch.json` fails: the change is not consumed. It stays
    /// pending, so the board is not left running a configuration with no
    /// window to put it back, and the same token confirms once the disk
    /// allows it.
    #[tokio::test]
    async fn a_failed_switch_json_write_keeps_the_change_pending() {
        let rig = rig("switch-confirm-json-fail", false, false, None);
        let pending = rig
            .service
            .apply(trunk(), DEFAULT_WINDOW, false)
            .await
            .expect("apply");
        let blocker = block_writes_to(&rig.switch);

        let error = rig
            .service
            .confirm(&pending.token)
            .await
            .expect_err("the write fails");
        assert!(matches!(error, SwitchError::Io(_)), "{error}");
        assert!(!rig.switch.exists(), "switch.json not written");
        let view = rig.service.view().await;
        assert_eq!(
            view.pending.map(|p| p.token),
            Some(pending.token.clone()),
            "still pending, same token"
        );
        assert_eq!(view.confirmed, None, "nothing was confirmed");
        assert_eq!(view.running, trunk(), "the board is still running it");

        std::fs::remove_dir(blocker).unwrap();
        rig.service
            .confirm(&pending.token)
            .await
            .expect("the retry goes through");
        let view = rig.service.view().await;
        assert!(view.pending.is_none());
        assert_eq!(view.confirmed, Some(trunk()));
        assert!(rig.switch.exists());
    }

    /// Without a retry the window still does its job: the change that could
    /// not be saved is put back when the time is up.
    #[tokio::test]
    async fn a_change_whose_confirm_could_not_write_is_still_reverted_by_the_timer() {
        let rig = rig("switch-confirm-json-timer", false, false, None);
        let pending = rig
            .service
            .apply(trunk(), DEFAULT_WINDOW, false)
            .await
            .expect("apply");
        let _blocker = block_writes_to(&rig.switch);
        rig.service
            .confirm(&pending.token)
            .await
            .expect_err("the write fails");

        rig.exec.forget();
        rig.service
            .tick_at(pending.applied_at + DEFAULT_WINDOW + START_CEILING)
            .await;
        let view = rig.service.view().await;
        assert!(view.pending.is_none(), "put back");
        assert_eq!(view.running, Preset::flat().expand());
        assert!(
            rig.exec
                .ran_any("bridge vlan add dev ge1 vid 1 pvid untagged"),
            "{:#?}",
            rig.exec.ran()
        );
    }

    // ---- an apply that fails part-way -------------------------------------

    /// The last step of a Flat to Trunk plan turns filtering on. When that is
    /// the one that fails, every VLAN has already been rewritten: the way back
    /// has to run, after the failure, and the change must not stay pending.
    #[tokio::test]
    async fn an_apply_that_fails_in_the_middle_runs_the_way_back() {
        let rig = rig("switch-apply-midway", false, false, None);
        rig.exec.fail("vlan_filtering 1");

        let error = rig
            .service
            .apply(trunk(), DEFAULT_WINDOW, false)
            .await
            .expect_err("filtering cannot be turned on");
        assert!(matches!(error, SwitchError::Command { .. }), "{error}");

        let ran = rig.exec.ran();
        let failed_at = ran
            .iter()
            .position(|c| c.contains("vlan_filtering 1"))
            .expect("the failing step ran");
        let back = &ran[failed_at + 1..];
        assert!(
            back.iter().any(|c| c.contains("vlan_filtering 0")),
            "the way back starts by turning filtering off: {ran:#?}"
        );
        assert!(
            back.contains(&"bridge vlan add dev ge1 vid 1 pvid untagged".to_string()),
            "the VLANs the plan removed are put back: {ran:#?}"
        );
        let view = rig.service.view().await;
        assert!(view.pending.is_none(), "no change is left pending");
        assert_eq!(view.running, Preset::flat().expand());
    }

    /// A step in the middle of the plan fails: the steps after it never ran,
    /// and the way back is best effort, so it goes past the errors its own
    /// steps meet and still reports the original failure.
    #[tokio::test]
    async fn the_way_back_goes_past_a_step_that_fails_and_reports_the_first_error() {
        let rig = rig("switch-apply-midway-undo", false, false, None);
        rig.exec.fail("bridge vlan add dev ge0 vid 10");
        // The way back also fails on its first VLAN step.
        rig.exec.fail("bridge vlan del dev ge0 vid 10");

        let error = rig
            .service
            .apply(trunk(), DEFAULT_WINDOW, false)
            .await
            .expect_err("a VLAN cannot be added");
        match error {
            SwitchError::Command { command, .. } => {
                assert!(
                    command.contains("bridge vlan add dev ge0 vid 10"),
                    "{command}"
                )
            }
            other => panic!("not the first failure: {other}"),
        }
        assert!(
            rig.exec
                .ran()
                .contains(&"bridge vlan add dev ge1 vid 1 pvid untagged".to_string()),
            "the way back carried on past its own failure: {:#?}",
            rig.exec.ran()
        );
        assert!(rig.service.view().await.pending.is_none());
    }

    // ---- the timer's revert is best effort too ----------------------------

    /// The timer reaches the same `carry_out` as a requested revert: it goes
    /// on past the step that fails, and reports what the kernel holds as
    /// running.
    #[tokio::test]
    async fn the_timer_revert_carries_on_past_a_failing_command() {
        let rig = rig("switch-timer-best-effort", true, false, None);
        let pending = rig
            .service
            .apply(trunk(), DEFAULT_WINDOW, false)
            .await
            .expect("apply");

        rig.exec.fail("bridge vlan del dev ge0 vid 10");
        std::fs::write(rig.dir.path().join("net/br0/bridge/vlan_filtering"), "0\n").unwrap();
        rig.exec.set_vlans(
            r#"[{"ifname":"ge0","vlans":[{"vlan":1,"flags":["PVID","Egress Untagged"]},{"vlan":10}]}]"#,
        );
        rig.exec.forget();

        rig.service
            .tick_at(pending.applied_at + DEFAULT_WINDOW + START_CEILING)
            .await;

        assert!(
            rig.exec
                .ran()
                .contains(&"bridge vlan add dev ge1 vid 1 pvid untagged".to_string()),
            "the steps after the failure still ran: {:#?}",
            rig.exec.ran()
        );
        let view = rig.service.view().await;
        assert!(view.pending.is_none());
        assert!(!view.running.vlan_filtering);
        assert_eq!(
            view.running.ports[&PortId::Ge0],
            crate::app::switch_document::PortConfig {
                untagged: Some(1),
                tagged: [10].into_iter().collect()
            }
        );
    }

    // ---- one change at a time, and none of them on an async worker ---------

    /// Holds the first command it is asked to run until the test lets it go,
    /// and otherwise behaves as [`FakeExec`].
    struct GateExec {
        inner: Arc<FakeExec>,
        taken: std::sync::atomic::AtomicBool,
        /// Whether a command is being held, and whether it may go on.
        state: std::sync::Mutex<(bool, bool)>,
        changed: std::sync::Condvar,
    }

    impl GateExec {
        fn new() -> Arc<Self> {
            Arc::new(GateExec {
                inner: FakeExec::new(),
                taken: std::sync::atomic::AtomicBool::new(false),
                state: std::sync::Mutex::new((false, false)),
                changed: std::sync::Condvar::new(),
            })
        }
        fn entered(&self) -> bool {
            self.state.lock().unwrap().0
        }
        fn release(&self) {
            self.state.lock().unwrap().1 = true;
            self.changed.notify_all();
        }
    }

    impl Exec for GateExec {
        fn run(&self, program: &str, args: &[&str]) -> Result<String, ApplyError> {
            let is_read = program == "bridge" && args.join(" ") == "-j vlan show";
            if !is_read && !self.taken.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let mut state = self.state.lock().unwrap();
                state.0 = true;
                while !state.1 {
                    state = self.changed.wait(state).unwrap();
                }
            }
            self.inner.run(program, args)
        }
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// The request that started a change can be dropped while `ip` is still
    /// running (the client went away). The change must still run to its end
    /// and record what it left running, the next change must wait until it
    /// has, and nothing but the commands' own thread may be held up meanwhile.
    ///
    /// On the single-threaded runtime of this test, a command run on the async
    /// worker would hang the test at the gate instead of failing it.
    #[tokio::test]
    async fn a_change_whose_request_is_dropped_still_finishes_before_the_next_one_starts() {
        let dir = tempdir::TempDir::new("switch-dropped-request").expect("temp dir");
        let gate = GateExec::new();
        let service = Arc::new(
            SwitchService::load(dir.path().join("switch.json"))
                .with_interfaces_path(dir.path().join("interfaces"))
                .with_net_class(net(dir.path(), false, false))
                .with_bond_executors(executor(dir.path()))
                .with_exec(gate.clone()),
        );

        let first = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.apply(trunk(), DEFAULT_WINDOW, false).await }
        });
        until("the first command to start", || gate.entered()).await;

        // The client goes away with the command still running.
        first.abort();
        assert!(first.await.is_err(), "the request was dropped");

        // No worker is stuck in `ip`: the service still answers.
        let view = tokio::time::timeout(Duration::from_secs(2), service.view())
            .await
            .expect("a running command does not hold up the service");
        assert!(view.pending.is_some(), "the change is waiting to be proved");

        // The next change waits for the one that is still running.
        let second = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.revert().await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !second.is_finished(),
            "the next change waits for the one still running"
        );
        assert!(
            gate.inner.ran().is_empty(),
            "nothing else ran while the first command was held: {:#?}",
            gate.inner.ran()
        );

        gate.release();
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("the second change finishes")
            .expect("the task ran")
            .expect("the revert");

        let first_plan: Vec<String> = plan(&Preset::flat().expand(), &trunk())
            .iter()
            .map(|command| command.join(" "))
            .collect();
        let ran = gate.inner.ran();
        assert!(ran.len() > first_plan.len(), "{ran:#?}");
        assert_eq!(
            ran[..first_plan.len()],
            first_plan[..],
            "the dropped change ran to its end before anything else"
        );
        // The revert was planned from what the first change left running: had
        // `running` never been written, it would have had nothing to undo.
        let view = service.view().await;
        assert!(view.pending.is_none());
        assert_eq!(view.running, Preset::flat().expand());
    }
}
