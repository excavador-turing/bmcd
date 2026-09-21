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
//! Changing the board's address without being able to lock yourself out.
//!
//! The same shape as the switch's [`super::switch_change`], for the same
//! reason: the address is how you reach the page you are changing it from,
//! and a wrong one takes the board off the network with no API left to call.
//!
//! 1. **Apply** puts the new address on the bridge and keeps the old one.
//! 2. **Confirm** arrives on a *new connection*. After an apply the old
//!    address is gone, so any authenticated request that reaches the daemon
//!    came in at the new one. That is the whole proof.
//! 3. **No confirm in the window** and the board puts the old address back
//!    by itself, and records why.
//!
//! One difference from the switch: the window counts from the apply. There
//! is no spanning tree to wait for -- the bridge and its ports are not
//! touched, only the address on top of them -- so the moment the apply
//! returns is the moment a person could first reach the board.
//!
//! Only a confirmed document is ever written to `/etc/network/interfaces`, so
//! a reboot mid-window boots on the previous one.
use crate::app::address_document::AddressDocument;
use schemars::JsonSchema;
use serde::Serialize;
use std::time::{Duration, SystemTime};

pub const DEFAULT_WINDOW: Duration = Duration::from_secs(30);
pub const MIN_WINDOW: Duration = Duration::from_secs(10);
pub const MAX_WINDOW: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RevertReason {
    NotConfirmed,
    Requested,
    /// The apply itself failed part-way, and the previous address was put
    /// back at once rather than left for the window.
    ApplyFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RevertRecord {
    pub at: SystemTime,
    pub reason: RevertReason,
    pub document: AddressDocument,
}

/// A change that has been applied and is waiting to be proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Pending {
    /// Returned by apply and required by confirm, so a confirm cannot land on
    /// a different change than the one its sender saw.
    pub token: String,
    pub document: AddressDocument,
    /// What to put back. Held rather than re-read, because reading it back
    /// would mean asking the network we have just changed.
    #[serde(skip)]
    pub snapshot: AddressDocument,
    pub applied_at: SystemTime,
    pub window_s: u64,
}

impl Pending {
    pub fn deadline(&self) -> SystemTime {
        self.applied_at + Duration::from_secs(self.window_s)
    }

    fn expired_at(&self, now: SystemTime) -> bool {
        now >= self.deadline()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeError {
    WindowOutOfRange,
    AlreadyPending,
    NothingPending,
    WrongToken,
}

impl std::fmt::Display for ChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeError::WindowOutOfRange => write!(
                f,
                "the confirm window must be between {} and {} seconds",
                MIN_WINDOW.as_secs(),
                MAX_WINDOW.as_secs()
            ),
            ChangeError::AlreadyPending => write!(
                f,
                "an address change is already waiting to be confirmed; confirm or revert it first"
            ),
            ChangeError::NothingPending => write!(f, "there is no address change waiting"),
            ChangeError::WrongToken => write!(
                f,
                "that token is not the pending change's; it may have been reverted and \
                 another applied since"
            ),
        }
    }
}

impl std::error::Error for ChangeError {}

/// Returned rather than performed, so the state machine can be reasoned
/// about and tested without a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Apply(AddressDocument),
    Persist(AddressDocument),
    None,
}

#[derive(Debug, Clone, Default)]
pub struct AddressState {
    confirmed: Option<AddressDocument>,
    pending: Option<Pending>,
    last_revert: Option<RevertRecord>,
}

impl AddressState {
    pub fn confirmed(&self) -> Option<&AddressDocument> {
        self.confirmed.as_ref()
    }

    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    pub fn last_revert(&self) -> Option<&RevertRecord> {
        self.last_revert.as_ref()
    }

    /// Adopt what the file said at boot, without a window: it was confirmed
    /// once already, and the board is running on it.
    pub fn adopt_persisted(&mut self, document: AddressDocument) {
        self.confirmed = Some(document);
    }

    pub fn apply(
        &mut self,
        document: AddressDocument,
        running: AddressDocument,
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
        };
        self.pending = Some(pending.clone());
        Ok((pending, Effect::Apply(document)))
    }

    /// The apply did not complete: forget the pending change and put the
    /// snapshot back now. The window is not the recovery for a half-applied
    /// address; the caller's error message is.
    pub fn apply_failed(&mut self, now: SystemTime) -> Effect {
        let Some(pending) = self.pending.take() else {
            return Effect::None;
        };
        self.last_revert = Some(RevertRecord {
            at: now,
            reason: RevertReason::ApplyFailed,
            document: pending.document,
        });
        Effect::Apply(pending.snapshot)
    }

    pub fn confirm(&mut self, token: &str, now: SystemTime) -> Result<Effect, ChangeError> {
        let Some(pending) = self.pending.as_ref() else {
            return Err(ChangeError::NothingPending);
        };
        if pending.token != token {
            return Err(ChangeError::WrongToken);
        }
        if pending.expired_at(now) {
            // The board has already put the old address back; persisting the
            // pending one would write down something that is not running.
            return Err(ChangeError::NothingPending);
        }
        let document = pending.document.clone();
        self.confirmed = Some(document.clone());
        self.pending = None;
        Ok(Effect::Persist(document))
    }

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

    /// On a timer: put an unconfirmed change back once its window has passed.
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
    use crate::app::address_document::StaticAddress;

    fn fixed() -> AddressDocument {
        AddressDocument::Static(StaticAddress {
            address: "192.168.1.20".parse().unwrap(),
            prefix: 24,
            gateway: Some("192.168.1.1".parse().unwrap()),
            dns: vec![],
            search: None,
        })
    }

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_000_000)
    }

    fn applied(state: &mut AddressState) -> Pending {
        let (pending, effect) = state
            .apply(fixed(), AddressDocument::Dhcp, DEFAULT_WINDOW, t0(), "tok".into())
            .expect("a valid document applies");
        assert_eq!(effect, Effect::Apply(fixed()));
        pending
    }

    #[test]
    fn apply_then_confirm_persists_the_new_document() {
        let mut state = AddressState::default();
        let pending = applied(&mut state);
        assert_eq!(pending.deadline(), t0() + DEFAULT_WINDOW);
        let effect = state.confirm("tok", t0() + Duration::from_secs(5)).unwrap();
        assert_eq!(effect, Effect::Persist(fixed()));
        assert_eq!(state.confirmed(), Some(&fixed()));
        assert!(state.pending().is_none());
    }

    #[test]
    fn the_window_counts_from_the_apply_and_reverts_to_the_snapshot() {
        let mut state = AddressState::default();
        applied(&mut state);
        assert_eq!(state.tick(t0() + Duration::from_secs(29)), Effect::None);
        assert_eq!(
            state.tick(t0() + Duration::from_secs(30)),
            Effect::Apply(AddressDocument::Dhcp)
        );
        let revert = state.last_revert().unwrap();
        assert_eq!(revert.reason, RevertReason::NotConfirmed);
        assert_eq!(revert.document, fixed());
        assert!(state.confirmed().is_none(), "nothing was ever confirmed");
    }

    #[test]
    fn a_late_confirm_is_refused() {
        let mut state = AddressState::default();
        applied(&mut state);
        assert_eq!(
            state.confirm("tok", t0() + Duration::from_secs(31)),
            Err(ChangeError::NothingPending)
        );
    }

    #[test]
    fn the_wrong_token_is_refused_and_the_change_keeps_waiting() {
        let mut state = AddressState::default();
        applied(&mut state);
        assert_eq!(state.confirm("other", t0()), Err(ChangeError::WrongToken));
        assert!(state.pending().is_some());
    }

    #[test]
    fn a_second_apply_while_one_is_pending_is_refused() {
        let mut state = AddressState::default();
        applied(&mut state);
        assert_eq!(
            state
                .apply(AddressDocument::Dhcp, fixed(), DEFAULT_WINDOW, t0(), "t2".into())
                .err(),
            Some(ChangeError::AlreadyPending)
        );
    }

    #[test]
    fn a_failed_apply_puts_the_snapshot_back_at_once() {
        let mut state = AddressState::default();
        applied(&mut state);
        assert_eq!(state.apply_failed(t0()), Effect::Apply(AddressDocument::Dhcp));
        assert!(state.pending().is_none());
        assert_eq!(state.last_revert().unwrap().reason, RevertReason::ApplyFailed);
    }

    #[test]
    fn the_window_is_bounded() {
        let mut state = AddressState::default();
        for secs in [9, 301] {
            assert_eq!(
                state
                    .apply(fixed(), AddressDocument::Dhcp, Duration::from_secs(secs), t0(), "t".into())
                    .err(),
                Some(ChangeError::WindowOutOfRange),
                "{secs}s"
            );
        }
    }
}
