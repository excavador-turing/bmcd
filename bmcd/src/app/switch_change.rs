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

//! Changing the switch without being able to lock yourself out.
//!
//! Every rule here exists because the thing you would use to undo a mistake
//! is the thing the mistake breaks. A wrong VLAN on the BMC's own port takes
//! the board off the network, and then there is no API to call.
//!
//! So a change is applied on approval and kept on proof:
//!
//! 1. **Apply** puts the new document on the switch and keeps the old one.
//! 2. **Confirm** arrives on a *new connection*. That is the whole proof: the
//!    old path no longer exists, so any authenticated request that reaches the
//!    daemon came through the new configuration. Nothing else needs checking.
//! 3. **No confirm in the window** and the board puts the old document back by
//!    itself, and records why.
//!
//! ## The window starts when the uplink forwards, not when apply returns
//!
//! This is the detail that makes the difference between a feature and a
//! nuisance. Spanning tree holds a port in listening and learning for its own
//! forwarding delay -- 30 seconds by default -- before it passes traffic. A
//! window counted from the apply would therefore expire before a correct
//! Trunk change could possibly be confirmed, and would revert every single
//! one of them.
//!
//! So the countdown starts at the moment the uplink carrying the BMC's VLAN
//! reports forwarding. With no spanning tree in play there is nothing to wait
//! for and it starts at once.
//!
//! ## Only a confirmed document is ever persisted
//!
//! A reboot during the window comes back on the previous confirmed document,
//! because the pending one was never written. Together with the boot applier
//! skipping entirely when the safe-mode key is held, that means no
//! unconfirmed configuration can survive a power cycle -- which is what let
//! this work stop waiting on a hardware watchdog.
//!
//! This module is the state machine and its clock. It does not touch the
//! switch; [`SwitchApplier`] is how it asks something else to.

use crate::app::switch_document::SwitchDocument;
use schemars::JsonSchema;
use serde::Serialize;
use std::time::{Duration, SystemTime};

/// The default confirm window, and the range an operator may choose from.
///
/// Thirty seconds because that is spanning tree's own forwarding delay, so it
/// is the shortest window that is not simply a race against the protocol. The
/// floor is ten because a window shorter than a page load is a trap; the
/// ceiling is five minutes because a board left pending is a board somebody
/// has forgotten about.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(30);
pub const MIN_WINDOW: Duration = Duration::from_secs(10);
pub const MAX_WINDOW: Duration = Duration::from_secs(300);

/// Why a pending change went back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RevertReason {
    /// The window passed with no confirmation. The ordinary case, and the one
    /// the whole design is for.
    NotConfirmed,
    /// Somebody asked for it back.
    Requested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RevertRecord {
    pub at: SystemTime,
    pub reason: RevertReason,
    /// What was reverted, so the interface can say *your change at 12:03 was
    /// put back because it was not confirmed* rather than *something
    /// happened*.
    pub document: SwitchDocument,
}

/// A change that has been applied and is waiting to be proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Pending {
    /// Returned by apply and required by confirm. It is not a credential --
    /// the request is authenticated anyway -- it is there so a confirm cannot
    /// land on a *different* change than the one its sender saw.
    pub token: String,
    pub document: SwitchDocument,
    /// What to put back. Held rather than recomputed, because the thing that
    /// would recompute it is the switch we have just changed.
    #[serde(skip)]
    pub snapshot: SwitchDocument,
    pub applied_at: SystemTime,
    pub window_s: u64,
    /// `None` until the uplink carrying the BMC's VLAN forwards. While it is
    /// `None` the window is not running, and the interface says so instead of
    /// showing a countdown that has not started.
    pub counting_from: Option<SystemTime>,
}

impl Pending {
    /// When this change will be put back, if nobody confirms it.
    pub fn deadline(&self) -> Option<SystemTime> {
        self.counting_from
            .map(|from| from + Duration::from_secs(self.window_s))
    }

    fn expired_at(&self, now: SystemTime) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }
}

/// What the daemon knows about the switch's configuration.
#[derive(Debug, Clone, Default)]
pub struct SwitchState {
    /// The last document anybody confirmed. This is what is persisted and
    /// what a reboot comes back to. `None` means nothing has ever been
    /// confirmed, and the board is on whatever it boots as.
    confirmed: Option<SwitchDocument>,
    pending: Option<Pending>,
    last_revert: Option<RevertRecord>,
}

/// Why a transition was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeError {
    /// A second change while one is still waiting. Queueing them would mean
    /// reverting to a snapshot that was itself never confirmed.
    AlreadyPending,
    /// The token names a change that is not the one pending.
    WrongToken,
    /// Nothing is waiting.
    NothingPending,
    /// The window is outside what the board will accept.
    WindowOutOfRange,
}

impl std::fmt::Display for ChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeError::AlreadyPending => write!(
                f,
                "a change is already waiting to be confirmed. Confirm it or revert it first: \
                 applying a second one would mean the way back is a configuration nobody ever \
                 confirmed either."
            ),
            ChangeError::WrongToken => write!(
                f,
                "that token is not the change this board is waiting on. It is probably from an \
                 earlier attempt that has already been reverted."
            ),
            ChangeError::NothingPending => write!(f, "no change is waiting to be confirmed."),
            ChangeError::WindowOutOfRange => write!(
                f,
                "the confirm window must be between {} and {} seconds.",
                MIN_WINDOW.as_secs(),
                MAX_WINDOW.as_secs()
            ),
        }
    }
}

/// What a caller should do to the hardware as a result of a transition.
///
/// Returned rather than performed, so the state machine can be reasoned about
/// and tested without a switch, and so the one place that talks to the
/// hardware is the one place that has to be careful about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Put this document on the switch.
    Apply(SwitchDocument),
    /// Write this document to the overlay as the persisted configuration.
    Persist(SwitchDocument),
    /// Nothing to do.
    None,
}

impl SwitchState {
    pub fn confirmed(&self) -> Option<&SwitchDocument> {
        self.confirmed.as_ref()
    }

    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    pub fn last_revert(&self) -> Option<&RevertRecord> {
        self.last_revert.as_ref()
    }

    /// Adopt a document read from the overlay at boot, without a window.
    ///
    /// It was confirmed once already; making the board re-prove it on every
    /// boot would mean a board that reverts itself every time it starts.
    pub fn adopt_persisted(&mut self, document: SwitchDocument) {
        self.confirmed = Some(document);
    }

    /// Apply a document, keeping `running` as the way back.
    pub fn apply(
        &mut self,
        document: SwitchDocument,
        running: SwitchDocument,
        window: Duration,
        now: SystemTime,
        token: String,
    ) -> Result<(Pending, Effect), ChangeError> {
        if !(MIN_WINDOW..=MAX_WINDOW).contains(&window) {
            return Err(ChangeError::WindowOutOfRange);
        }
        if self.pending.is_some() {
            return Err(ChangeError::AlreadyPending);
        }

        let pending = Pending {
            token,
            document: document.clone(),
            snapshot: running,
            applied_at: now,
            window_s: window.as_secs(),
            counting_from: None,
        };
        self.pending = Some(pending.clone());
        Ok((pending, Effect::Apply(document)))
    }

    /// The uplink carrying the BMC's VLAN is forwarding: start counting.
    ///
    /// Idempotent. The link will flap while spanning tree settles, and the
    /// first time it forwards is when a person could first have reached the
    /// board -- so a later flap must not restart a window that is already
    /// running, or a board that never quite settles would never revert.
    pub fn uplink_forwarding(&mut self, now: SystemTime) {
        if let Some(pending) = self.pending.as_mut() {
            if pending.counting_from.is_none() {
                pending.counting_from = Some(now);
            }
        }
    }

    /// Keep the change. Only now is anything written down.
    pub fn confirm(&mut self, token: &str, now: SystemTime) -> Result<Effect, ChangeError> {
        let Some(pending) = self.pending.as_ref() else {
            return Err(ChangeError::NothingPending);
        };
        if pending.token != token {
            return Err(ChangeError::WrongToken);
        }
        // A confirm that arrives after the deadline is refused rather than
        // honoured: by then the board has already put the old document back,
        // and accepting it would persist a document that is not running.
        if pending.expired_at(now) {
            return Err(ChangeError::NothingPending);
        }

        let document = pending.document.clone();
        self.confirmed = Some(document.clone());
        self.pending = None;
        Ok(Effect::Persist(document))
    }

    /// Put the pending change back now.
    pub fn revert(&mut self, now: SystemTime) -> Result<Effect, ChangeError> {
        let Some(pending) = self.pending.take() else {
            return Err(ChangeError::NothingPending);
        };
        self.last_revert = Some(RevertRecord {
            at: now,
            reason: RevertReason::Requested,
            document: pending.document,
        });
        Ok(Effect::Apply(pending.snapshot))
    }

    /// Called on a timer. Puts an unconfirmed change back once its window has
    /// passed, and does nothing otherwise.
    pub fn tick(&mut self, now: SystemTime) -> Effect {
        let Some(pending) = self.pending.as_ref() else {
            return Effect::None;
        };
        if !pending.expired_at(now) {
            return Effect::None;
        }
        let pending = self.pending.take().expect("checked just above");
        self.last_revert = Some(RevertRecord {
            at: now,
            reason: RevertReason::NotConfirmed,
            document: pending.document,
        });
        Effect::Apply(pending.snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::switch_document::{Preset, SecondUplink};

    fn trunk() -> SwitchDocument {
        Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Redundant,
        }
        .expand()
    }

    fn flat() -> SwitchDocument {
        Preset::Flat.expand()
    }

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_000_000)
    }

    fn applied(state: &mut SwitchState, now: SystemTime) -> Pending {
        let (pending, effect) = state
            .apply(trunk(), flat(), DEFAULT_WINDOW, now, "tok".into())
            .expect("a valid document applies");
        assert_eq!(effect, Effect::Apply(trunk()));
        pending
    }

    /// THE RULE THE WHOLE MODULE IS FOR. Spanning tree holds a port for its
    /// forwarding delay before it passes traffic. A window counted from the
    /// apply would expire before a correct Trunk change could be confirmed,
    /// and would revert every one of them.
    #[test]
    fn the_window_does_not_run_until_the_uplink_forwards() {
        let mut state = SwitchState::default();
        let pending = applied(&mut state, t0());
        assert_eq!(pending.counting_from, None);
        assert_eq!(pending.deadline(), None);

        // Well past the window, measured from the apply.
        let long_after = t0() + Duration::from_secs(600);
        assert_eq!(
            state.tick(long_after),
            Effect::None,
            "a change was reverted while the uplink had not yet forwarded"
        );
        assert!(state.pending().is_some());

        // The link comes up, and only now does the clock start.
        state.uplink_forwarding(long_after);
        assert_eq!(state.tick(long_after), Effect::None);
        assert_eq!(
            state.tick(long_after + DEFAULT_WINDOW),
            Effect::Apply(flat()),
            "the window should run from forwarding"
        );
    }

    /// A link that flaps while spanning tree settles must not keep pushing the
    /// deadline out, or a board that never quite settles would never revert.
    #[test]
    fn a_later_link_event_does_not_restart_the_window() {
        let mut state = SwitchState::default();
        applied(&mut state, t0());
        state.uplink_forwarding(t0() + Duration::from_secs(5));
        let first = state.pending().unwrap().deadline();

        state.uplink_forwarding(t0() + Duration::from_secs(20));
        assert_eq!(state.pending().unwrap().deadline(), first);
    }

    #[test]
    fn confirming_keeps_the_change_and_is_the_only_thing_that_persists_it() {
        let mut state = SwitchState::default();
        applied(&mut state, t0());
        assert_eq!(
            state.confirmed(),
            None,
            "nothing is written before a confirm"
        );

        state.uplink_forwarding(t0());
        let effect = state.confirm("tok", t0() + Duration::from_secs(5)).unwrap();
        assert_eq!(effect, Effect::Persist(trunk()));
        assert_eq!(state.confirmed(), Some(&trunk()));
        assert!(state.pending().is_none());
        assert_eq!(state.tick(t0() + Duration::from_secs(600)), Effect::None);
    }

    #[test]
    fn a_change_nobody_confirms_goes_back_and_says_why() {
        let mut state = SwitchState::default();
        applied(&mut state, t0());
        state.uplink_forwarding(t0());

        assert_eq!(
            state.tick(t0() + DEFAULT_WINDOW),
            Effect::Apply(flat()),
            "the snapshot goes back on the switch"
        );
        let record = state.last_revert().expect("a revert is recorded");
        assert_eq!(record.reason, RevertReason::NotConfirmed);
        assert_eq!(
            record.document,
            trunk(),
            "the record names what was reverted"
        );
        assert_eq!(
            state.confirmed(),
            None,
            "an unconfirmed change is never kept"
        );
    }

    /// By the time the deadline has passed the old document is already back on
    /// the switch. Honouring a late confirm would persist a document that is
    /// not running.
    #[test]
    fn a_confirm_after_the_deadline_is_refused() {
        let mut state = SwitchState::default();
        applied(&mut state, t0());
        state.uplink_forwarding(t0());
        assert_eq!(
            state.confirm("tok", t0() + DEFAULT_WINDOW),
            Err(ChangeError::NothingPending)
        );
    }

    #[test]
    fn a_confirm_with_the_wrong_token_is_refused() {
        let mut state = SwitchState::default();
        applied(&mut state, t0());
        state.uplink_forwarding(t0());
        assert_eq!(
            state.confirm("someone-elses", t0()),
            Err(ChangeError::WrongToken)
        );
        assert!(state.pending().is_some(), "and the change is left alone");
    }

    /// Queueing a second change would mean the way back is a configuration
    /// nobody ever confirmed either.
    #[test]
    fn a_second_change_while_one_is_pending_is_refused() {
        let mut state = SwitchState::default();
        applied(&mut state, t0());
        let again = state.apply(flat(), trunk(), DEFAULT_WINDOW, t0(), "two".into());
        assert_eq!(again.unwrap_err(), ChangeError::AlreadyPending);
    }

    #[test]
    fn reverting_on_request_puts_the_snapshot_back_and_records_the_reason() {
        let mut state = SwitchState::default();
        applied(&mut state, t0());
        assert_eq!(state.revert(t0()).unwrap(), Effect::Apply(flat()));
        assert_eq!(state.last_revert().unwrap().reason, RevertReason::Requested);
        assert!(state.pending().is_none());
    }

    #[test]
    fn reverting_with_nothing_pending_is_refused() {
        let mut state = SwitchState::default();
        assert_eq!(state.revert(t0()).unwrap_err(), ChangeError::NothingPending);
    }

    #[test]
    fn a_window_outside_the_allowed_range_is_refused() {
        for window in [Duration::from_secs(1), Duration::from_secs(3600)] {
            let mut state = SwitchState::default();
            let result = state.apply(trunk(), flat(), window, t0(), "tok".into());
            assert_eq!(result.unwrap_err(), ChangeError::WindowOutOfRange);
            assert!(state.pending().is_none(), "and nothing was applied");
        }
    }

    #[test]
    fn the_edges_of_the_allowed_range_are_accepted() {
        for window in [MIN_WINDOW, MAX_WINDOW] {
            let mut state = SwitchState::default();
            assert!(state
                .apply(trunk(), flat(), window, t0(), "tok".into())
                .is_ok());
        }
    }

    /// A document read from the overlay was confirmed once already. Making the
    /// board re-prove it every boot would be a board that reverts itself every
    /// time it starts.
    #[test]
    fn a_persisted_document_is_adopted_without_a_window() {
        let mut state = SwitchState::default();
        state.adopt_persisted(trunk());
        assert_eq!(state.confirmed(), Some(&trunk()));
        assert!(state.pending().is_none());
        assert_eq!(state.tick(t0() + Duration::from_secs(10_000)), Effect::None);
    }

    /// A reboot during the window comes back on the previous confirmed
    /// document, because the pending one was never written. This is the test
    /// for that: after an apply and before a confirm, there is nothing to
    /// persist.
    #[test]
    fn a_reboot_during_the_window_has_nothing_to_come_back_to_but_the_old_one() {
        let mut state = SwitchState::default();
        state.adopt_persisted(flat());
        applied(&mut state, t0());
        state.uplink_forwarding(t0());
        // Whatever the board does next, the only document ever handed to
        // Effect::Persist is the confirmed one.
        assert_eq!(state.confirmed(), Some(&flat()));
    }
}
