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
use crate::app::address_applier::{Applier, ApplyError, LiveAddress};
use crate::app::address_change::{
    AddressState, ChangeError, Effect, Pending, RevertRecord, DEFAULT_WINDOW,
};
use crate::app::address_document::{parse, resolv_conf, AddressDocument};
use crate::app::network_interfaces::{InterfaceInfo, MANAGED};
use schemars::JsonSchema;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;

pub const INTERFACES_FILE: &str = "/etc/network/interfaces";
pub const TICK: Duration = Duration::from_secs(1);

/// What to do when `br0` is missing, said after "This board has no br0
/// bridge (it has: ...)."
const NO_MANAGED_BRIDGE: &str = "The address page manages br0 only; edit \
    /etc/network/interfaces over SSH, or restore the default layout.";

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
    /// Where the copy of a hand-written file was kept by the last confirmed
    /// change, if this daemon made one since it started.
    pub file_backup: Option<String>,
    /// What the primary interface actually has, read now.
    pub live: LiveAddress,
    /// Every up, non-loopback interface with a global IPv4 address, the one
    /// with the default route first.
    pub interfaces: Vec<InterfaceInfo>,
    /// The only interface this page can change.
    pub managed_interface: String,
    /// Whether it exists. When it does not, a change is refused.
    pub managed_interface_present: bool,
    pub pending: Option<Pending>,
    pub last_revert: Option<RevertRecord>,
    pub default_window_s: u64,
}

#[derive(Debug)]
pub enum AddressError {
    Change(ChangeError),
    /// `br0` is not there. Nothing was changed.
    NoManagedBridge(String),
    /// The file was written by hand and the request did not say to replace
    /// it. Nothing was changed.
    HandWritten(String),
    Apply(ApplyError),
    Io(std::io::Error),
}

impl std::fmt::Display for AddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddressError::Change(e) => write!(f, "{e}"),
            AddressError::NoManagedBridge(m) | AddressError::HandWritten(m) => write!(f, "{m}"),
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
    applier: Applier,
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
    /// Whether replacing the file needs the caller's say-so: it is not ours,
    /// and it is more than the image's own default.
    replaces_hand_written: bool,
    /// The file's modification time and length when it was last read. A
    /// different pair means somebody changed it since.
    stamp: Option<Stamp>,
    file_backup: Option<PathBuf>,
}

/// What the `interfaces` file says, as of a read.
struct FileRead {
    document: Option<AddressDocument>,
    origin: FileOrigin,
    replaces_hand_written: bool,
    stamp: Option<Stamp>,
}

type Stamp = (SystemTime, u64);

/// Modification time and length: two edits inside one clock tick still
/// differ in length more often than not.
fn modified(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The lines that mean something: no comments, no blanks.
fn significant_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

fn read_file(path: &Path) -> FileRead {
    let stamp = modified(path);
    let contents = match std::fs::read(path) {
        // No file at all is the image's own default, which is DHCP.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return FileRead {
                document: Some(AddressDocument::Dhcp),
                origin: FileOrigin::HandEdited,
                replaces_hand_written: false,
                stamp,
            };
        }
        Ok(bytes) => String::from_utf8(bytes).ok(),
        Err(e) => {
            tracing::warn!("could not read {}: {e}", path.display());
            None
        }
    };
    let Some(contents) = contents else {
        // There is a file and this daemon cannot make sense of it -- not
        // UTF-8, or not readable at all. It is somebody's, so it is treated
        // as one: a change needs the opt-in and keeps a copy.
        return FileRead {
            document: None,
            origin: FileOrigin::Unreadable,
            replaces_hand_written: true,
            stamp,
        };
    };
    let parsed = parse(&contents);
    let origin = match (&parsed.document, parsed.ours) {
        (_, true) => FileOrigin::Bmcd,
        (Some(_), false) => FileOrigin::HandEdited,
        (None, false) => FileOrigin::Unreadable,
    };
    // The image's own file is not somebody's work: replacing it needs no
    // ceremony and no copy. It is the file this daemon renders for DHCP,
    // minus the comments.
    let image_default =
        significant_lines(&contents) == significant_lines(&AddressDocument::Dhcp.render());
    FileRead {
        document: parsed.document,
        origin,
        replaces_hand_written: origin != FileOrigin::Bmcd && !image_default,
        stamp,
    }
}

impl AddressService {
    /// Read what the file says. Applies nothing: the boot path already did.
    pub fn load(path: impl AsRef<Path>) -> Self {
        Self::load_with(path, Applier::system())
    }

    /// [`load`](Self::load), on an applier of the caller's choosing.
    pub fn load_with(path: impl AsRef<Path>, applier: Applier) -> Self {
        let path = path.as_ref().to_path_buf();
        let read = read_file(&path);
        let mut state = AddressState::default();
        if let Some(document) = read.document.clone() {
            state.adopt_persisted(document);
        }
        AddressService {
            inner: Mutex::new(Inner {
                state,
                running: read.document.unwrap_or(AddressDocument::Dhcp),
                file: read.origin,
                replaces_hand_written: read.replaces_hand_written,
                stamp: read.stamp,
                file_backup: None,
            }),
            path,
            applier,
        }
    }

    /// Read the file again if it has changed since it was last read, so a
    /// file somebody restored by hand is noticed without a restart.
    ///
    /// What the board is *running* is not touched: that is what this daemon
    /// last applied, and the file says what a reboot comes back to.
    fn refresh(&self, inner: &mut Inner) {
        if modified(&self.path) == inner.stamp {
            return;
        }
        let read = read_file(&self.path);
        tracing::info!(
            "{} changed since it was last read; it now reads as {:?}",
            self.path.display(),
            read.origin
        );
        inner.state.set_persisted(read.document);
        inner.file = read.origin;
        inner.replaces_hand_written = read.replaces_hand_written;
        inner.stamp = read.stamp;
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
        let mut inner = self.inner.lock().await;
        let mut done = Repair::default();

        if inner.file == FileOrigin::Bmcd {
            let wanted = inner.running.render();
            let have = std::fs::read_to_string(&self.path).unwrap_or_default();
            if have != wanted {
                match write_atomically(&self.path, wanted.as_bytes()) {
                    Ok(()) => {
                        inner.stamp = modified(&self.path);
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
        // One reading of the network serves `live` and `interfaces` both.
        let snapshot = self.applier.snapshot_async().await;
        let live = self.applier.live(&snapshot);
        let interfaces = snapshot.interfaces;
        let mut inner = self.inner.lock().await;
        self.refresh(&mut inner);
        AddressView {
            running: inner.running.clone(),
            configured: inner.state.confirmed().cloned(),
            file: inner.file,
            file_backup: inner.file_backup.as_ref().map(|p| p.display().to_string()),
            live,
            interfaces,
            managed_interface: MANAGED.to_string(),
            managed_interface_present: self.applier.managed_present(),
            pending: inner.state.pending().cloned(),
            last_revert: inner.state.last_revert().cloned(),
            default_window_s: DEFAULT_WINDOW.as_secs(),
        }
    }

    /// Put a document on the bridge and start waiting to be proved right.
    ///
    /// Two refusals come before anything is touched, and neither leaves a
    /// trace in the state: no `br0` to put it on, and a file written by hand
    /// that the caller has not said may be replaced.
    pub async fn apply(
        &self,
        document: AddressDocument,
        window: Duration,
        replace_hand_written: bool,
    ) -> Result<Pending, AddressError> {
        let mut inner = self.inner.lock().await;
        self.refresh(&mut inner);
        if let Some(message) = self.applier.missing_message(NO_MANAGED_BRIDGE) {
            return Err(AddressError::NoManagedBridge(message));
        }
        if inner.replaces_hand_written && !replace_hand_written {
            return Err(AddressError::HandWritten(self.hand_written_message()));
        }
        let running = inner.running.clone();
        let token = new_token();
        let (pending, effect) =
            inner
                .state
                .apply(document, running, window, SystemTime::now(), token)?;
        if let Effect::Apply(document) = effect {
            if let Err(e) = self.applier.apply(&document).await {
                // Half an address is worse than the old one. Put it back now
                // and report the failure; the window is not for this.
                let back = inner.state.apply_failed(SystemTime::now(), e.to_string());
                if let Effect::Apply(previous) = back {
                    if let Err(e2) = self.applier.apply(&previous).await {
                        tracing::error!("could not put the previous address back either: {e2}");
                    }
                }
                return Err(AddressError::Apply(e));
            }
            inner.running = document;
            inner.state.clear_last_revert();
        }
        Ok(pending)
    }

    fn hand_written_message(&self) -> String {
        let path = self.path.display();
        format!(
            "{path} was written by hand. A confirmed change replaces the whole file (a copy of \
             the current one is kept as {path}.hand-written.<time>). Send replace_hand_written \
             to proceed."
        )
    }

    pub async fn confirm(&self, token: &str) -> Result<(), AddressError> {
        let mut inner = self.inner.lock().await;
        // Somebody may have edited the file since the apply.
        self.refresh(&mut inner);
        // Check first, write, and only then commit. A copy or a write that
        // fails (a full or read-only overlay) leaves the change pending: it
        // can be confirmed again, or the window puts the old address back.
        // Committing first left the new address live with nothing pending,
        // `configured` claiming a file that was never written, and a reboot
        // quietly undoing it.
        let document = inner.state.check_confirm(token, SystemTime::now())?;

        // A file this daemon did not write is replaced whole, so what was in
        // it is kept first. Never overwritten without the copy. The image's
        // own file is nobody's work and is not kept.
        let mut backup = None;
        if inner.replaces_hand_written && self.path.exists() {
            let (kept, removed) = keep_a_copy(&self.path).map_err(AddressError::Io)?;
            tracing::warn!(
                "{} was not written by this daemon; a copy is kept as {}",
                self.path.display(),
                kept.display()
            );
            for old in removed {
                tracing::warn!("removed the older copy {}", old.display());
            }
            backup = Some(kept);
        }
        if let Err(e) = write_atomically(&self.path, document.render().as_bytes()) {
            if let Some(kept) = &backup {
                let _ = std::fs::remove_file(kept);
            }
            return Err(AddressError::Io(e));
        }

        inner
            .state
            .confirm(token, SystemTime::now())
            .map_err(AddressError::Change)?;
        if backup.is_some() {
            inner.file_backup = backup;
        }
        inner.file = FileOrigin::Bmcd;
        inner.replaces_hand_written = false;
        inner.stamp = modified(&self.path);
        tracing::info!(
            "address confirmed and written to {}: {}",
            self.path.display(),
            describe(&document)
        );
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
            self.applier
                .apply(&document)
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

/// How many copies of hand-written files are kept.
const KEEP_COPIES: usize = 5;

/// Copy `path` to `<path>.hand-written.<unix seconds>`, byte for byte, in the
/// same directory. Returns where it went and which older copies were removed
/// to stay within [`KEEP_COPIES`].
fn keep_a_copy(path: &Path) -> std::io::Result<(PathBuf, Vec<PathBuf>)> {
    let seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    keep_a_copy_at(path, seconds)
}

fn keep_a_copy_at(path: &Path, seconds: u64) -> std::io::Result<(PathBuf, Vec<PathBuf>)> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!("{name}.hand-written.");
    let bytes = std::fs::read(path)?;

    // Written under a name that is not a copy's, then linked to the first
    // free name: `link` refuses to replace, so a copy made earlier -- on a
    // board with no clock, the same second comes round on every boot -- is
    // never overwritten.
    let tmp = path.with_file_name(format!("{prefix}tmp"));
    let result = (|| {
        std::fs::write(&tmp, &bytes)?;
        for attempt in 0u32.. {
            let candidate = if attempt == 0 {
                format!("{prefix}{seconds}")
            } else {
                format!("{prefix}{seconds}-{attempt}")
            };
            let target = path.with_file_name(candidate);
            match std::fs::hard_link(&tmp, &target) {
                Ok(()) => return Ok(target),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!("the attempts do not run out")
    })();
    let _ = std::fs::remove_file(&tmp);
    let kept = result?;

    let removed = prune_copies(path.parent().unwrap_or(Path::new(".")), &prefix, &kept);
    Ok((kept, removed))
}

/// Remove all but the newest [`KEEP_COPIES`] copies, never the one just made.
/// Newest is by the number in the name, then by its suffix.
fn prune_copies(dir: &Path, prefix: &str, just_made: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut copies: Vec<((u64, u32), PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let rest = name.strip_prefix(prefix)?;
            let (seconds, suffix) = match rest.split_once('-') {
                Some((s, n)) => (s, n.parse().ok()?),
                None => (rest, 0),
            };
            Some(((seconds.parse().ok()?, suffix), e.path()))
        })
        .collect();
    copies.sort();
    let excess = copies.len().saturating_sub(KEEP_COPIES);
    let mut removed = Vec::new();
    for (_, old) in copies.into_iter().take(excess) {
        if old != just_made && std::fs::remove_file(&old).is_ok() {
            removed.push(old);
        }
    }
    removed
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

    // --- any bridge layout -------------------------------------------------

    use crate::app::address_applier::testing::FakeExec;
    use crate::app::address_change::RevertReason;
    use crate::app::address_document::StaticAddress;
    use crate::app::network_interfaces::tests::{ADDR_SPLIT, ADDR_STOCK, ROUTE_SPLIT, ROUTE_STOCK};
    use std::sync::Arc;

    /// The user's file: two bridges, no `br0`, written by hand.
    const SPLIT_FILE: &str = "auto lo\niface lo inet loopback\n\nauto br-wan\niface br-wan inet dhcp\n  bridge-ports ge0\n\nauto br-lan\niface br-lan inet static\n  address 192.168.1.10/24\n  bridge-ports ge1\n";
    /// A hand-written file that does have `br0`, and says something of its own.
    const HAND_FILE: &str = "auto br0\niface br0 inet static\n  address 10.1.2.3/16\n  gateway 10.1.0.1\n  bridge-ports node1 node2 node3 node4 ge0 ge1\n  post-up /usr/local/bin/mine\n";

    fn lan() -> AddressDocument {
        AddressDocument::Static(StaticAddress {
            address: "192.168.1.20".parse().unwrap(),
            prefix: 24,
            gateway: Some("192.168.1.1".parse().unwrap()),
            dns: vec![],
            search: None,
        })
    }

    struct Rig {
        dir: tempdir::TempDir,
        file: PathBuf,
        exec: Arc<FakeExec>,
        service: AddressService,
    }

    /// A board in a temp dir: the file, `/sys/class/net` with or without
    /// `br0`, and an `ip` that prints `addr` and `route`.
    fn rig(file: Option<&str>, br0: bool, exec: Arc<FakeExec>) -> Rig {
        let dir = tempdir::TempDir::new("address-board").expect("temp dir");
        let net = dir.path().join("net");
        std::fs::create_dir_all(&net).unwrap();
        if br0 {
            std::fs::create_dir_all(net.join("br0/bridge")).unwrap();
        }
        let path = dir.path().join("etc/interfaces");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        if let Some(file) = file {
            std::fs::write(&path, file).unwrap();
        }
        let service = AddressService::load_with(&path, Applier::with(exec.clone(), dir.path()));
        Rig {
            dir,
            file: path,
            exec,
            service,
        }
    }

    fn board(file: Option<&str>, br0: bool) -> Rig {
        rig(file, br0, FakeExec::board(ADDR_STOCK, ROUTE_STOCK))
    }

    fn split_board() -> Rig {
        rig(
            Some(SPLIT_FILE),
            false,
            FakeExec::board(ADDR_SPLIT, ROUTE_SPLIT),
        )
    }

    fn set_mtime(path: &Path, secs: u64) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    #[tokio::test]
    async fn the_split_layout_is_read_as_unreadable_and_the_view_names_the_bridge_in_use() {
        let rig = split_board();
        let view = rig.service.view().await;
        assert_eq!(view.file, FileOrigin::Unreadable);
        assert_eq!(view.managed_interface, "br0");
        assert!(!view.managed_interface_present);
        assert_eq!(view.live.interface.as_deref(), Some("br-wan"));
        assert_eq!(view.live.address.as_deref(), Some("10.20.0.5/16"));
        assert_eq!(view.live.gateway, Some("10.20.0.1".parse().unwrap()));
        let names: Vec<&str> = view.interfaces.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["br-wan", "br-lan"]);

        // The JSON field names the interface is built against.
        let json = serde_json::to_value(&view).unwrap();
        for key in [
            "live",
            "interfaces",
            "managed_interface",
            "managed_interface_present",
            "file_backup",
            "last_revert",
            "pending",
        ] {
            assert!(json.get(key).is_some(), "missing {key}: {json}");
        }
        assert_eq!(json["live"]["interface"], "br-wan");
        assert_eq!(json["interfaces"][0]["default_route"], true);
        assert_eq!(json["managed_interface"], "br0");
        assert_eq!(json["managed_interface_present"], false);
    }

    /// The report: a `PUT` on a board with no `br0` used to run `ip addr add
    /// dev br0`, fail, and leave a `last_revert` that nothing cleared.
    #[tokio::test]
    async fn without_br0_a_change_is_refused_and_leaves_no_trace() {
        let rig = split_board();
        let before = std::fs::read(&rig.file).unwrap();
        for opt_in in [false, true] {
            let error = rig
                .service
                .apply(lan(), DEFAULT_WINDOW, opt_in)
                .await
                .expect_err("no br0");
            let AddressError::NoManagedBridge(message) = &error else {
                panic!("expected NoManagedBridge, got {error:?}");
            };
            assert_eq!(
                message,
                "This board has no br0 bridge (it has: br-wan, br-lan). The address page \
                 manages br0 only; edit /etc/network/interfaces over SSH, or restore the \
                 default layout."
            );
        }
        let view = rig.service.view().await;
        assert!(view.pending.is_none());
        assert!(view.last_revert.is_none());
        assert!(rig.exec.changes().is_empty(), "{:?}", rig.exec.changes());
        assert_eq!(std::fs::read(&rig.file).unwrap(), before);
    }

    #[tokio::test]
    async fn a_hand_written_file_needs_the_opt_in_and_the_refusal_changes_nothing() {
        let rig = board(Some(HAND_FILE), true);
        let view = rig.service.view().await;
        assert_eq!(view.file, FileOrigin::HandEdited);
        assert!(view.managed_interface_present);

        let error = rig
            .service
            .apply(lan(), DEFAULT_WINDOW, false)
            .await
            .expect_err("hand-written, no opt-in");
        let AddressError::HandWritten(message) = &error else {
            panic!("expected HandWritten, got {error:?}");
        };
        assert_eq!(
            message,
            &format!(
                "{p} was written by hand. A confirmed change replaces the whole file (a copy \
                 of the current one is kept as {p}.hand-written.<time>). Send \
                 replace_hand_written to proceed.",
                p = rig.file.display()
            )
        );
        let view = rig.service.view().await;
        assert!(view.pending.is_none());
        assert!(view.last_revert.is_none());
        assert!(rig.exec.changes().is_empty());
    }

    #[tokio::test]
    async fn with_the_opt_in_the_change_goes_through_and_confirm_keeps_a_copy() {
        let rig = board(Some(HAND_FILE), true);
        let pending = rig
            .service
            .apply(lan(), DEFAULT_WINDOW, true)
            .await
            .expect("opted in");
        assert!(rig
            .exec
            .changes()
            .iter()
            .any(|c| c.starts_with("ip -4 addr add 192.168.1.20/24")));
        // Nothing is written until the confirmation.
        assert_eq!(std::fs::read_to_string(&rig.file).unwrap(), HAND_FILE);

        rig.service.confirm(&pending.token).await.expect("confirm");

        let written = std::fs::read_to_string(&rig.file).unwrap();
        assert_eq!(written, lan().render());
        let view = rig.service.view().await;
        assert_eq!(view.file, FileOrigin::Bmcd);
        let backup = PathBuf::from(view.file_backup.expect("the copy is reported"));
        assert_eq!(backup.parent(), rig.file.parent());
        let name = backup.file_name().unwrap().to_string_lossy().into_owned();
        let seconds = name
            .strip_prefix("interfaces.hand-written.")
            .unwrap_or_else(|| panic!("{name}"));
        assert!(seconds.parse::<u64>().is_ok(), "{name}");
        assert_eq!(std::fs::read(&backup).unwrap(), HAND_FILE.as_bytes());
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(rig.file.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers.len(), 2, "{leftovers:?}");

        // The next change replaces our own file: no opt-in, no second copy.
        let again = rig
            .service
            .apply(AddressDocument::Dhcp, DEFAULT_WINDOW, false)
            .await
            .expect("our own file needs no opt-in");
        rig.service.confirm(&again.token).await.unwrap();
        let count = std::fs::read_dir(rig.file.parent().unwrap())
            .unwrap()
            .count();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn an_unreadable_file_needs_the_opt_in_too_when_br0_exists() {
        let rig = board(Some("auto eth0\niface eth0 inet dhcp\n"), true);
        let error = rig
            .service
            .apply(lan(), DEFAULT_WINDOW, false)
            .await
            .expect_err("unreadable, no opt-in");
        assert!(matches!(error, AddressError::HandWritten(_)));
        assert!(rig.service.apply(lan(), DEFAULT_WINDOW, true).await.is_ok());
    }

    #[tokio::test]
    async fn the_images_own_file_and_a_missing_file_are_replaced_without_ceremony() {
        let image = "# interface file auto-generated by buildroot\n\nauto lo\niface lo inet loopback\n\nauto br0\niface br0 inet dhcp\n  bridge-ports node1 node2 node3 node4 ge0 ge1\n  pre-up /etc/network/nfs_check\n  wait-delay 15\n  hostname $(hostname)\n";
        for file in [Some(image), None] {
            let rig = board(file, true);
            let pending = rig
                .service
                .apply(lan(), DEFAULT_WINDOW, false)
                .await
                .expect("the stock file is not somebody's work");
            rig.service.confirm(&pending.token).await.unwrap();
            assert_eq!(std::fs::read_to_string(&rig.file).unwrap(), lan().render());
            // Nobody's work was replaced, so there is nothing to keep.
            assert!(rig.service.view().await.file_backup.is_none());
            assert_eq!(
                std::fs::read_dir(rig.file.parent().unwrap())
                    .unwrap()
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn a_file_this_daemon_wrote_is_replaced_without_a_copy() {
        let rig = board(Some(&AddressDocument::Dhcp.render()), true);
        let pending = rig
            .service
            .apply(lan(), DEFAULT_WINDOW, false)
            .await
            .unwrap();
        rig.service.confirm(&pending.token).await.unwrap();
        assert!(rig.service.view().await.file_backup.is_none());
        assert_eq!(
            std::fs::read_dir(rig.file.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_failed_apply_records_why_and_a_later_success_clears_it() {
        let exec = FakeExec::failing(ADDR_STOCK, ROUTE_STOCK, "ip -4 addr add");
        let rig = rig(Some(&AddressDocument::Dhcp.render()), true, exec);
        let error = rig
            .service
            .apply(lan(), DEFAULT_WINDOW, false)
            .await
            .expect_err("ip fails");
        let AddressError::Apply(_) = &error else {
            panic!("{error:?}");
        };
        assert!(error.to_string().contains("ip -4 addr add"), "{error}");
        assert!(error.to_string().contains("Cannot find device"), "{error}");

        let view = rig.service.view().await;
        assert!(view.pending.is_none());
        let record = view.last_revert.expect("a failure is recorded");
        assert_eq!(record.reason, RevertReason::ApplyFailed);
        assert!(
            record.detail.contains("Cannot find device"),
            "{}",
            record.detail
        );
        assert_eq!(record.document, lan());
        let json = serde_json::to_value(&record).unwrap();
        assert!(
            json["detail"].is_string() && json.get("at").is_some(),
            "{json}"
        );

        // The next apply that works clears it, before any confirmation.
        rig.exec.heal();
        let pending = rig
            .service
            .apply(lan(), DEFAULT_WINDOW, false)
            .await
            .unwrap();
        assert!(rig.service.view().await.last_revert.is_none());
        rig.service.confirm(&pending.token).await.unwrap();
        assert!(rig.service.view().await.last_revert.is_none());
    }

    #[tokio::test]
    async fn a_file_restored_by_hand_is_noticed_without_a_restart() {
        let rig = split_board();
        set_mtime(&rig.file, 1_000);
        // The load saw the file before its mtime was pinned; read it again.
        assert_eq!(rig.service.view().await.file, FileOrigin::Unreadable);
        assert!(rig.service.view().await.configured.is_none());

        // Somebody restores the stock layout over SSH.
        std::fs::write(&rig.file, AddressDocument::Dhcp.render()).unwrap();
        set_mtime(&rig.file, 2_000);
        let view = rig.service.view().await;
        assert_eq!(view.file, FileOrigin::Bmcd);
        assert_eq!(view.configured, Some(AddressDocument::Dhcp));

        // And hand-edits it again, readable this time.
        std::fs::write(&rig.file, HAND_FILE).unwrap();
        set_mtime(&rig.file, 3_000);
        let view = rig.service.view().await;
        assert_eq!(view.file, FileOrigin::HandEdited);
        assert!(matches!(view.configured, Some(AddressDocument::Static(_))));
    }

    #[tokio::test]
    async fn an_edit_between_apply_and_confirm_is_kept_not_overwritten() {
        let rig = board(Some(&AddressDocument::Dhcp.render()), true);
        set_mtime(&rig.file, 1_000);
        let _ = rig.service.view().await;
        let pending = rig
            .service
            .apply(lan(), DEFAULT_WINDOW, false)
            .await
            .unwrap();
        std::fs::write(&rig.file, HAND_FILE).unwrap();
        set_mtime(&rig.file, 2_000);
        rig.service.confirm(&pending.token).await.unwrap();
        let backup = rig
            .service
            .view()
            .await
            .file_backup
            .expect("a copy was kept");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), HAND_FILE);
        let _ = &rig.dir;
    }

    /// The write fails after the address went live. The change must stay
    /// pending -- confirmable again, or put back by its window -- and the
    /// view must not claim a file that was never written.
    #[tokio::test]
    async fn a_write_that_fails_leaves_the_change_pending_and_the_view_unchanged() {
        let rig = board(Some(HAND_FILE), true);
        let pending = rig
            .service
            .apply(lan(), DEFAULT_WINDOW, true)
            .await
            .unwrap();
        let before = rig.service.view().await;
        // Writing goes through `interfaces.tmp`; a directory there stops it.
        let blocker = rig.file.with_extension("tmp");
        std::fs::create_dir(&blocker).unwrap();

        let error = rig
            .service
            .confirm(&pending.token)
            .await
            .expect_err("the write fails");
        assert!(matches!(error, AddressError::Io(_)), "{error:?}");
        assert!(!error.to_string().is_empty());

        let after = rig.service.view().await;
        assert_eq!(
            after.pending.as_ref().map(|p| &p.token),
            Some(&pending.token)
        );
        assert_eq!(after.configured, before.configured);
        assert_eq!(after.file, FileOrigin::HandEdited);
        assert!(after.file_backup.is_none());
        assert!(after.last_revert.is_none());
        assert_eq!(std::fs::read_to_string(&rig.file).unwrap(), HAND_FILE);
        // The copy made before the failed write is not left lying about.
        let names: Vec<_> = std::fs::read_dir(rig.file.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| !n.starts_with("interfaces.hand-written.")),
            "{names:?}"
        );

        // Confirming again, once the disk allows it, works.
        std::fs::remove_dir(&blocker).unwrap();
        rig.service.confirm(&pending.token).await.expect("retry");
        let view = rig.service.view().await;
        assert!(view.pending.is_none());
        assert_eq!(view.file, FileOrigin::Bmcd);
        assert_eq!(view.configured, Some(lan()));
        assert!(view.file_backup.is_some());
    }

    /// A Latin-1 byte in a comment made the file "not found" and so DHCP:
    /// hand-written content walked past the opt-in.
    #[test]
    fn a_file_that_is_not_utf8_is_unreadable_and_somebodys() {
        let dir = tempdir::TempDir::new("address-latin1").expect("temp dir");
        let path = dir.path().join("interfaces");
        std::fs::write(
            &path,
            b"# caf\xe9 uplink\nauto br0\niface br0 inet dhcp\n  bridge-ports ge0\n",
        )
        .unwrap();
        let read = read_file(&path);
        assert_eq!(read.origin, FileOrigin::Unreadable);
        assert_eq!(read.document, None);
        assert!(read.replaces_hand_written);
    }

    #[tokio::test]
    async fn a_file_that_is_not_utf8_needs_the_opt_in_and_is_kept_byte_for_byte() {
        let bytes: &[u8] = b"# caf\xe9 uplink\nauto br0\niface br0 inet dhcp\n  bridge-ports ge0\n";
        let rig = board(None, true);
        std::fs::write(&rig.file, bytes).unwrap();
        let service =
            AddressService::load_with(&rig.file, Applier::with(rig.exec.clone(), rig.dir.path()));
        let view = service.view().await;
        assert_eq!(view.file, FileOrigin::Unreadable);
        assert!(view.configured.is_none());
        let error = service
            .apply(lan(), DEFAULT_WINDOW, false)
            .await
            .expect_err("somebody's file");
        assert!(matches!(error, AddressError::HandWritten(_)));
        let pending = service.apply(lan(), DEFAULT_WINDOW, true).await.unwrap();
        service.confirm(&pending.token).await.unwrap();
        let backup = service.view().await.file_backup.expect("a copy");
        assert_eq!(std::fs::read(backup).unwrap(), bytes);
    }

    #[test]
    fn two_copies_in_one_second_do_not_overwrite_each_other() {
        let dir = tempdir::TempDir::new("address-copies").expect("temp dir");
        let path = dir.path().join("interfaces");
        let mut kept = Vec::new();
        for n in 0..3 {
            std::fs::write(&path, format!("version {n}\n")).unwrap();
            kept.push(keep_a_copy_at(&path, 1_000).unwrap().0);
        }
        let names: Vec<String> = kept
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "interfaces.hand-written.1000",
                "interfaces.hand-written.1000-1",
                "interfaces.hand-written.1000-2"
            ]
        );
        for (n, p) in kept.iter().enumerate() {
            assert_eq!(
                std::fs::read_to_string(p).unwrap(),
                format!("version {n}\n")
            );
        }
        // No scratch file is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 4);
    }

    #[test]
    fn only_the_newest_five_copies_are_kept_and_the_new_one_always_is() {
        let dir = tempdir::TempDir::new("address-prune").expect("temp dir");
        let path = dir.path().join("interfaces");
        std::fs::write(&path, "x\n").unwrap();
        let mut removed_total = Vec::new();
        for seconds in [100, 200, 300, 400, 500, 600, 700] {
            let (_, removed) = keep_a_copy_at(&path, seconds).unwrap();
            removed_total.extend(removed);
        }
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("interfaces.hand-written."))
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "interfaces.hand-written.300",
                "interfaces.hand-written.400",
                "interfaces.hand-written.500",
                "interfaces.hand-written.600",
                "interfaces.hand-written.700"
            ]
        );
        assert_eq!(removed_total.len(), 2);

        // A board whose clock went backwards: the copy just made is not the
        // newest by name, and is still not the one that goes.
        let (kept, removed) = keep_a_copy_at(&path, 50).unwrap();
        assert!(kept.exists());
        assert_eq!(removed.len(), 0, "{removed:?}");
    }

    #[test]
    fn a_copy_that_cannot_be_made_leaves_nothing_behind() {
        let dir = tempdir::TempDir::new("address-nocopy").expect("temp dir");
        let path = dir.path().join("interfaces");
        assert!(keep_a_copy_at(&path, 1).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Same mtime, different bytes: the length tells.
    #[tokio::test]
    async fn an_edit_inside_one_clock_tick_is_noticed_by_its_length() {
        let rig = split_board();
        set_mtime(&rig.file, 1_000);
        assert_eq!(rig.service.view().await.file, FileOrigin::Unreadable);
        std::fs::write(&rig.file, AddressDocument::Dhcp.render()).unwrap();
        set_mtime(&rig.file, 1_000);
        assert_eq!(rig.service.view().await.file, FileOrigin::Bmcd);
    }
}
