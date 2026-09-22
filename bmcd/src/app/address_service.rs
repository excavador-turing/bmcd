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
//! The board's address, owned in one place.
//!
//! Holds the state machine, runs the live change it asks for, and writes the
//! confirmed document to `/etc/network/interfaces` -- the file the boot path
//! reads, so nothing new has to run at boot. That file lives on the overlay
//! (`/etc` is overlaid), which is also why `S00dsa` finds it there.
//!
//! ## What "running" means here
//!
//! The document the board is on, as this daemon understands it: what the
//! file said at start, then whatever was last applied. Not re-read from the
//! network, for the same reason the switch does not parse `bridge vlan show`
//! back: the one path where being wrong strands the board should not depend
//! on a parser. What the network actually has is reported beside it, as
//! `live`, so a hand change from a console is visible without being trusted.
use crate::app::address_applier::{self, ApplyError, LiveAddress};
use crate::app::address_change::{
    AddressState, ChangeError, Effect, Pending, RevertRecord, DEFAULT_WINDOW,
};
use crate::app::address_document::{parse, resolv_conf, AddressDocument};
use schemars::JsonSchema;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;

pub const INTERFACES_FILE: &str = "/etc/network/interfaces";
pub const TICK: Duration = Duration::from_secs(1);

/// Where the `interfaces` file came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileOrigin {
    /// This daemon wrote it.
    Bmcd,
    /// A person did, and this daemon could read it.
    HandEdited,
    /// A person did, and this daemon could not make sense of it. A confirmed
    /// change replaces it whole; the interface says so first.
    Unreadable,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AddressView {
    /// The document the board is on, as this daemon understands it.
    pub running: AddressDocument,
    /// What the file says, and therefore what a reboot comes back to.
    pub configured: Option<AddressDocument>,
    pub file: FileOrigin,
    /// What the bridge actually has, read now.
    pub live: LiveAddress,
    pub pending: Option<Pending>,
    pub last_revert: Option<RevertRecord>,
    pub default_window_s: u64,
}

#[derive(Debug)]
pub enum AddressError {
    Change(ChangeError),
    Apply(ApplyError),
    Io(std::io::Error),
}

impl std::fmt::Display for AddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddressError::Change(e) => write!(f, "{e}"),
            AddressError::Apply(e) => write!(f, "{e}"),
            AddressError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AddressError {}

impl From<ChangeError> for AddressError {
    fn from(e: ChangeError) -> Self {
        AddressError::Change(e)
    }
}

pub struct AddressService {
    inner: Mutex<Inner>,
    path: PathBuf,
}

/// What `repair` did at start.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Repair {
    pub file_rewritten: bool,
    pub resolvers_written: bool,
}

struct Inner {
    state: AddressState,
    running: AddressDocument,
    file: FileOrigin,
}

impl AddressService {
    /// Read what the file says. Applies nothing: the boot path already did.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let (document, file) = match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let parsed = parse(&contents);
                let origin = match (&parsed.document, parsed.ours) {
                    (_, true) => FileOrigin::Bmcd,
                    (Some(_), false) => FileOrigin::HandEdited,
                    (None, false) => FileOrigin::Unreadable,
                };
                (parsed.document, origin)
            }
            // No file at all is the image's own default, which is DHCP.
            Err(_) => (Some(AddressDocument::Dhcp), FileOrigin::HandEdited),
        };
        let mut state = AddressState::default();
        if let Some(document) = document.clone() {
            state.adopt_persisted(document);
        }
        AddressService {
            inner: Mutex::new(Inner {
                state,
                running: document.unwrap_or(AddressDocument::Dhcp),
                file,
            }),
            path,
        }
    }

    /// Put right, at start, what an earlier version of this daemon got wrong.
    ///
    /// Two things, both only for a file THIS daemon wrote -- a hand-edited
    /// file is never touched:
    ///
    /// 1. If the file is not byte-for-byte what this version renders for the
    ///    same document, it is rewritten. The document is unchanged; only
    ///    the rendering moves. This is how a board that 2.38.0 left with a
    ///    hook containing a literal `#` (which ifupdown-ng reads as a
    ///    comment, so `ifup br0` failed on every boot) gets a working file
    ///    from the first start of the daemon that knows better, rather than
    ///    at the next time somebody changes the address.
    ///
    /// 2. If the board runs a static address with resolvers and
    ///    `/etc/resolv.conf` does not carry them, they are written now. This
    ///    daemon starts after the network does, so on a board where the boot
    ///    hook failed, this is the moment the resolvers arrive; on a board
    ///    where it worked, the file already matches and nothing is written.
    ///    resolv.conf on this image is a symlink into tmpfs, so this is
    ///    honest about what a reboot does: it starts empty every time.
    ///
    /// Returns what it did, for the log and for the tests.
    pub async fn repair(&self, resolv_path: &Path) -> Repair {
        let inner = self.inner.lock().await;
        let mut done = Repair::default();

        if inner.file == FileOrigin::Bmcd {
            let wanted = inner.running.render();
            let have = std::fs::read_to_string(&self.path).unwrap_or_default();
            if have != wanted {
                match write_atomically(&self.path, wanted.as_bytes()) {
                    Ok(()) => {
                        done.file_rewritten = true;
                        tracing::warn!(
                            "rewrote {}: it was written by an earlier version of this daemon \
                             and its boot hook could not have run",
                            self.path.display()
                        );
                    }
                    Err(e) => tracing::error!("could not rewrite {}: {e}", self.path.display()),
                }
            }
        }

        if let AddressDocument::Static(s) = &inner.running {
            if !s.dns.is_empty() || s.search.is_some() {
                let wanted = resolv_conf(&s.dns, s.search.as_deref());
                let have = std::fs::read_to_string(resolv_path).unwrap_or_default();
                let missing = wanted.lines().any(|l| !have.lines().any(|h| h == l));
                if missing {
                    match std::fs::write(resolv_path, &wanted) {
                        Ok(()) => {
                            done.resolvers_written = true;
                            tracing::warn!(
                                "wrote the static address's resolvers to {}: the boot path had \
                                 left it without them",
                                resolv_path.display()
                            );
                        }
                        Err(e) => {
                            tracing::error!("could not write {}: {e}", resolv_path.display())
                        }
                    }
                }
            }
        }
        done
    }

    pub async fn view(&self) -> AddressView {
        let live = address_applier::live().await;
        let inner = self.inner.lock().await;
        AddressView {
            running: inner.running.clone(),
            configured: inner.state.confirmed().cloned(),
            file: inner.file,
            live,
            pending: inner.state.pending().cloned(),
            last_revert: inner.state.last_revert().cloned(),
            default_window_s: DEFAULT_WINDOW.as_secs(),
        }
    }

    /// Put a document on the bridge and start waiting to be proved right.
    pub async fn apply(
        &self,
        document: AddressDocument,
        window: Duration,
    ) -> Result<Pending, AddressError> {
        let mut inner = self.inner.lock().await;
        let running = inner.running.clone();
        let token = new_token();
        let (pending, effect) =
            inner
                .state
                .apply(document, running, window, SystemTime::now(), token)?;
        if let Effect::Apply(document) = effect {
            if let Err(e) = address_applier::apply(&document).await {
                // Half an address is worse than the old one. Put it back now
                // and report the failure; the window is not for this.
                let back = inner.state.apply_failed(SystemTime::now());
                if let Effect::Apply(previous) = back {
                    if let Err(e2) = address_applier::apply(&previous).await {
                        tracing::error!("could not put the previous address back either: {e2}");
                    }
                }
                return Err(AddressError::Apply(e));
            }
            inner.running = document;
        }
        Ok(pending)
    }

    pub async fn confirm(&self, token: &str) -> Result<(), AddressError> {
        let mut inner = self.inner.lock().await;
        let effect = inner.state.confirm(token, SystemTime::now())?;
        if let Effect::Persist(document) = effect {
            write_atomically(&self.path, document.render().as_bytes()).map_err(AddressError::Io)?;
            inner.file = FileOrigin::Bmcd;
            tracing::info!(
                "address confirmed and written to {}: {}",
                self.path.display(),
                describe(&document)
            );
        }
        Ok(())
    }

    pub async fn revert(&self) -> Result<(), AddressError> {
        let mut inner = self.inner.lock().await;
        let effect = inner.state.revert(SystemTime::now())?;
        self.carry_out(&mut inner, effect).await
    }

    pub async fn tick(&self) {
        let mut inner = self.inner.lock().await;
        let effect = inner.state.tick(SystemTime::now());
        if matches!(effect, Effect::None) {
            return;
        }
        tracing::warn!(
            "the address change was not confirmed in its window; putting the previous address back"
        );
        if let Err(e) = self.carry_out(&mut inner, effect).await {
            tracing::error!("could not put the previous address back: {e}");
        }
    }

    async fn carry_out(&self, inner: &mut Inner, effect: Effect) -> Result<(), AddressError> {
        if let Effect::Apply(document) = effect {
            address_applier::apply(&document)
                .await
                .map_err(AddressError::Apply)?;
            inner.running = document;
        }
        Ok(())
    }
}

fn describe(document: &AddressDocument) -> String {
    match document {
        AddressDocument::Dhcp => "dhcp".to_string(),
        AddressDocument::Static(s) => format!(
            "static {} via {}",
            s.cidr(),
            s.gateway
                .map(|g| g.to_string())
                .unwrap_or_else(|| "no gateway".into())
        ),
    }
}

fn new_token() -> String {
    use rand::Rng;
    let bytes: [u8; 16] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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

    #[test]
    fn a_missing_file_is_the_images_dhcp_default() {
        let dir = tempdir::TempDir::new("address-missing").expect("temp dir");
        let service = AddressService::load(dir.path().join("interfaces"));
        let inner = service.inner.try_lock().unwrap();
        assert_eq!(inner.running, AddressDocument::Dhcp);
        assert_eq!(inner.state.confirmed(), Some(&AddressDocument::Dhcp));
        assert_eq!(inner.file, FileOrigin::HandEdited);
    }

    #[test]
    fn a_file_this_daemon_wrote_is_recognised_as_its_own() {
        let dir = tempdir::TempDir::new("address-ours").expect("temp dir");
        let path = dir.path().join("interfaces");
        std::fs::write(&path, AddressDocument::Dhcp.render()).unwrap();
        let service = AddressService::load(&path);
        assert_eq!(service.inner.try_lock().unwrap().file, FileOrigin::Bmcd);
    }

    /// The hook 2.38.0 wrote carried a literal `#`, which ifupdown-ng reads
    /// as a comment; the board came up with `ifup br0` failed and no
    /// resolvers. The daemon that knows better fixes the file at its first
    /// start, without waiting for the address to be changed again.
    #[tokio::test]
    async fn a_file_this_daemon_wrote_with_the_old_hook_is_rewritten_at_start() {
        let dir = tempdir::TempDir::new("address-repair").expect("temp dir");
        let path = dir.path().join("interfaces");
        let resolv = dir.path().join("resolv.conf");
        let doc = AddressDocument::Static(crate::app::address_document::StaticAddress {
            address: "192.168.1.20".parse().unwrap(),
            prefix: 24,
            gateway: Some("192.168.1.1".parse().unwrap()),
            dns: vec!["192.168.1.1".parse().unwrap()],
            search: Some("home.lan".into()),
        });
        // As 2.38.0 rendered it: the same lines, the hook with a raw `#`.
        let old = doc.render().replace("\\043", "#");
        assert!(old.contains("# br0\\n"), "{old}");
        std::fs::write(&path, &old).unwrap();
        std::fs::write(&resolv, "").unwrap();

        let service = AddressService::load(&path);
        let done = service.repair(&resolv).await;
        assert_eq!(
            done,
            Repair {
                file_rewritten: true,
                resolvers_written: true
            }
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), doc.render());
        assert_eq!(
            std::fs::read_to_string(&resolv).unwrap(),
            "search home.lan # br0\nnameserver 192.168.1.1 # br0\n"
        );

        // A second start finds nothing to do.
        let again = AddressService::load(&path).repair(&resolv).await;
        assert_eq!(again, Repair::default());
    }

    #[tokio::test]
    async fn a_hand_edited_file_is_never_rewritten_and_dhcp_touches_no_resolvers() {
        let dir = tempdir::TempDir::new("address-repair-hand").expect("temp dir");
        let path = dir.path().join("interfaces");
        let resolv = dir.path().join("resolv.conf");
        let hand = "auto br0\niface br0 inet static\n  address 10.1.2.3/16\n  gateway 10.1.0.1\n  bridge-ports node1 node2 node3 node4 ge0 ge1\n";
        std::fs::write(&path, hand).unwrap();
        std::fs::write(&resolv, "").unwrap();
        let done = AddressService::load(&path).repair(&resolv).await;
        assert_eq!(done, Repair::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), hand);

        let dhcp = dir.path().join("interfaces-dhcp");
        std::fs::write(&dhcp, AddressDocument::Dhcp.render()).unwrap();
        let done = AddressService::load(&dhcp).repair(&resolv).await;
        assert_eq!(done, Repair::default());
        assert_eq!(std::fs::read_to_string(&resolv).unwrap(), "");
    }

    #[test]
    fn a_file_it_cannot_read_is_said_to_be_unreadable_and_runs_as_dhcp() {
        let dir = tempdir::TempDir::new("address-unreadable").expect("temp dir");
        let path = dir.path().join("interfaces");
        std::fs::write(
            &path,
            "auto eth0\niface eth0 inet static\n  address 10.0.0.2\n",
        )
        .unwrap();
        let service = AddressService::load(&path);
        let inner = service.inner.try_lock().unwrap();
        assert_eq!(inner.file, FileOrigin::Unreadable);
        assert_eq!(inner.state.confirmed(), None);
        assert_eq!(inner.running, AddressDocument::Dhcp);
    }
}
