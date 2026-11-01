// Copyright 2026 excavador
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
//! What each configured source is offering.
//!
//! Every remote source is resolved by `tpi-selfupdate --list`, not by code
//! here. The script already knows that `stable` excludes pre-releases, how to
//! order versions, and the HTTP directory layout -- and it is the thing that
//! performs the upgrade. A second implementation in this daemon would be free
//! to disagree with it, and the page could then offer an install the updater
//! refuses.
//!
//! ## Failures are per source
//!
//! One source rate-limited, unreachable, or serving an index this cannot parse
//! must not blank the others. Each carries its own error, and an error is
//! never rendered as "nothing new" -- those are different claims and only the
//! caller knows which one it is about to make.
use crate::app::firmware_sources::{Source, SourceKind};
use crate::app::sdcard_files::SDCARD_ROOT;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const SELFUPDATE: &str = "/sbin/tpi-selfupdate";

/// How long a source's answer stays good. Unauthenticated GitHub allows 60
/// requests an hour per address, and a browser left on the firmware page would
/// otherwise spend them.
const FRESH: Duration = Duration::from_secs(1800);
/// A failed source is retried sooner: the usual cause is a board with no route
/// out, which can be fixed at any moment.
///
/// Two minutes, not less. One source of ours -- the HTTP mirror -- publishes
/// no checksums and errors routinely, so this is the interval that actually
/// governs, and every expiry spawns one shell and one `curl` per source. At
/// 120 s a page left open costs the board four processes every two minutes
/// for as long as it is open. Raising it further trades freshness for calm;
/// lowering it is how a small board is worn down.
const FRESH_AFTER_ERROR: Duration = Duration::from_secs(120);
/// When the only thing wrong is that sources were slow (a timeout, or one still
/// busy from the last check), the retry waits longer: a board that is slow but
/// working would otherwise start the `curl`-heavy listings again every couple
/// of minutes, against a GitHub allowance of 60 requests an hour.
const FRESH_AFTER_SLOW: Duration = Duration::from_secs(900);

/// How a candidate relates to what is running.
///
/// `Unknown` is not a failure. A board built from a working tree reports
/// `VERSION=local`, and ordering that against real tags is meaningless --
/// measured on hardware, `sort -V` put `local` first and every release,
/// including genuinely older code, came out as an upgrade. Saying "unknown" is
/// the honest answer and lets the page say so rather than implying an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Relation {
    Current,
    Newer,
    Older,
    Unknown,
}

/// How much is known about an image's integrity.
///
/// Three genuinely different things that a page must not render alike: a
/// checksum published by the release and verified on download; TLS only,
/// because the publisher ships no checksums at all (upstream's server does
/// not); and a local file whose provenance is whatever put it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    Verified,
    Tls,
    Unverified,
}

/// Deserialize as well as Serialize: the same shape is written to
/// /mnt/overlay/firmware-catalog.json and read back at the next boot, so the
/// wire format and the stored format are one format by construction. Every
/// field that is skipped when serialising takes `default`, or a settled
/// catalogue would fail to load the moment it omitted one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Candidate {
    pub version: String,
    pub relation: Relation,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prerelease: bool,
    pub trust: Trust,
    /// For a local candidate, the file it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceCatalog {
    pub id: String,
    pub label: String,
    pub kind: SourceKind,
    pub location: String,
    pub candidates: Vec<Candidate>,
    /// Why this source produced nothing, when it produced nothing for a
    /// reason. An empty list with no error means the source genuinely offers
    /// nothing; an empty list WITH an error means it could not be read. The
    /// page must not collapse those two into "up to date".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Catalog {
    pub checked_at: String,
    pub running: String,
    pub sources: Vec<SourceCatalog>,
    /// These are the previous answers and a refresh is running behind them.
    ///
    /// The page draws a spinner on its "check now" control and leaves the
    /// list underneath readable, rather than blanking or freezing. Skipped
    /// when false so a settled catalogue serialises as it always did.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refreshing: bool,
    /// How old these answers are. A caller that needs to say "as of a minute
    /// ago" should not have to parse `checked_at` and trust two clocks.
    pub age_seconds: u64,
}

/// What `tpi-selfupdate --list` emits.
#[derive(Debug, Deserialize)]
struct ListedRelease {
    tag: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    relation: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Listing {
    running: String,
    releases: Vec<ListedRelease>,
}

fn relation_from(value: Option<&str>) -> Relation {
    match value {
        Some("current") => Relation::Current,
        Some("newer") => Relation::Newer,
        Some("older") => Relation::Older,
        _ => Relation::Unknown,
    }
}

/// How long one source may take before the others stop waiting for it.
///
/// The slowest fan-out on record is bmc-2 on 2026-09-12, about 230 s for the
/// four sources (see [`REFRESH_CLAIM`]), so 360 s leaves that board room to
/// work and still bounds a source that hangs. With [`SOURCE_GRACE`] it stays
/// well inside [`REFRESH_CLAIM`], which remains the outer guard.
const SOURCE_DEADLINE: Duration = Duration::from_secs(360);

/// Slack for the async side to wait beyond [`SOURCE_DEADLINE`], so that a
/// remote source reports its own timeout (and has killed its child) before
/// the caller gives up on it.
const SOURCE_GRACE: Duration = Duration::from_secs(5);

/// The error of a source that was skipped because its previous check never
/// returned. See [`gather`].
const STILL_BUSY: &str = "still busy from the previous check";

fn timed_out(deadline: Duration) -> String {
    format!("timed out after {} s", deadline.as_secs())
}

/// Is this error about slowness rather than about what the source said?
fn is_slow(error: &str) -> bool {
    error.starts_with("timed out after ") || error == STILL_BUSY
}

/// Runs `command` to completion like `Command::output`, but gives up at
/// `deadline`. `Ok(None)` means the deadline passed: the child's process
/// group has been sent SIGKILL and the child reaped.
///
/// `spawn_blocking` cannot be cancelled, so a deadline on the task alone would
/// leave the work behind: `tpi-selfupdate` is a shell script that runs `curl`,
/// and a `curl` that hangs would otherwise live on for as long as the TCP
/// connection does, one more for every refresh, on a board with 116 MB of RAM.
/// So the child is started in its own process group and the whole group is
/// killed on expiry -- killing only the script would leave its `curl` holding
/// the pipes.
///
/// The order matters in two places. The child is NOT reaped until both pipes
/// have reached end-of-file or the deadline has passed, because the pipes are
/// what a grandchild still running keeps open, and a reaped child's group id
/// can be reused: `killpg` is only ever sent while the child is still a
/// zombie, which keeps the id reserved. And everything after the pipes closes
/// is bounded by the same deadline.
///
/// What is not covered: a process that left the group on purpose (`setsid`)
/// and holds the pipes keeps the two reader threads alive until it lets go;
/// the caller does not wait for them.
fn output_within(
    mut command: Command,
    deadline: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::sync::mpsc::{channel, RecvTimeoutError};

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let started = Instant::now();
    let remaining = || deadline.saturating_sub(started.elapsed());

    // Drained on threads so a full pipe cannot stall the child, and each
    // reports once, at end-of-file, so the wait below has a timeout to use.
    let (done, finished) = channel::<(bool, Vec<u8>)>();
    fn drain<R: Read + Send + 'static>(
        is_stdout: bool,
        pipe: Option<R>,
        done: std::sync::mpsc::Sender<(bool, Vec<u8>)>,
    ) {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buffer);
            }
            let _ = done.send((is_stdout, buffer));
        });
    }
    drain(true, child.stdout.take(), done.clone());
    drain(false, child.stderr.take(), done);

    // The group id is the child's pid (`process_group(0)`). Sent while the
    // child is unreaped; `kill` and `wait` on the child itself follow whatever
    // `killpg` said, so nothing is left as a zombie.
    let kill = |child: &mut std::process::Child| {
        let group = nix::unistd::Pid::from_raw(child.id() as i32);
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        let _ = child.kill();
        let _ = child.wait();
    };

    let (mut stdout, mut stderr) = (None, None);
    while stdout.is_none() || stderr.is_none() {
        match finished.recv_timeout(remaining()) {
            Ok((true, bytes)) => stdout = Some(bytes),
            Ok((false, bytes)) => stderr = Some(bytes),
            Err(RecvTimeoutError::Timeout) => {
                kill(&mut child);
                return Ok(None);
            }
            Err(RecvTimeoutError::Disconnected) => {
                kill(&mut child);
                return Err(std::io::Error::other(
                    "a reader of the child's output ended without a result",
                ));
            }
        }
    }

    // Both pipes are closed: the child is finished or about to be.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(Some(std::process::Output {
                    status,
                    stdout: stdout.unwrap_or_default(),
                    stderr: stderr.unwrap_or_default(),
                }));
            }
            Ok(None) if remaining().is_zero() => {
                kill(&mut child);
                return Ok(None);
            }
            Ok(None) => std::thread::sleep(remaining().min(Duration::from_millis(50))),
            Err(e) => {
                kill(&mut child);
                return Err(e);
            }
        }
    }
}

/// Runs the updater's listing for one remote source.
///
/// Blocking on purpose, called from `spawn_blocking`; the script bounds its own
/// network calls with `curl --max-time`.
fn list_remote(source: &Source, deadline: Duration) -> Result<Listing, String> {
    let mut command = Command::new(SELFUPDATE);
    command.arg("--list");
    match source.kind {
        SourceKind::Github => {
            command.args(["--repo", &source.location]);
        }
        SourceKind::Http => {
            command.args(["--url", &source.location]);
        }
        SourceKind::Local => unreachable!("local sources are read directly"),
    }

    let output = match output_within(command, deadline) {
        Ok(Some(output)) => output,
        Ok(None) => return Err(timed_out(deadline)),
        Err(e) => return Err(format!("cannot run {SELFUPDATE}: {e}")),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // THE EXIT STATUS IS READ BEFORE THE OUTPUT, and that order is the whole
    // point. Until now this looked only at stdout: a `{` line was taken as a
    // listing however the process had ended. The updater used to print
    // `{"releases":[]}` and exit 0 when it could not reach the source -- the
    // reason went to stderr, where nothing read it -- so a board with no
    // resolver reported every remote source as offering nothing, and the
    // firmware page said there was no update. Reported from a 2.4 board on
    // 2026-09-22 as "checking github for updated firmware also fails
    // silently", and reproduced on board B.
    //
    // The updater was fixed to exit non-zero and print nothing (firmware
    // v2.36.0, tests/listing.sh). This is the other half: an empty list is
    // believed only from a process that said it succeeded, so the next
    // script that fails cheerfully cannot put the page back to lying.
    if !output.status.success() {
        return Err(failure_reason(&stderr, output.status.code()));
    }

    let line = stdout.lines().find(|l| l.trim_start().starts_with('{'));
    match line {
        Some(line) => serde_json::from_str(line)
            .map_err(|e| format!("cannot parse the updater's listing: {e}")),
        None => Err(failure_reason(&stderr, output.status.code())),
    }
}

/// What to show for a listing that did not happen: the updater's own last
/// word, or the exit status when it said nothing at all.
///
/// "no output" was what this used to say in both cases, which told a reader
/// nothing they could act on -- and on the page it is the only sentence they
/// get.
fn failure_reason(stderr: &str, code: Option<i32>) -> String {
    let last = stderr.lines().last().unwrap_or_default().trim();
    if !last.is_empty() {
        return last.to_string();
    }
    match code {
        Some(code) => format!("{SELFUPDATE} exited {code} without saying why"),
        None => format!("{SELFUPDATE} was killed by a signal"),
    }
}

/// Scans a directory for images.
///
/// Metadata only -- name and size. A sha256 costs about two seconds per image
/// on this SoC, which is fine on demand and far too slow for a page that lists
/// a dozen, so integrity here is reported as unverified rather than computed.
fn list_local(source: &Source, running: &str) -> Result<Vec<Candidate>, String> {
    let card = std::path::Path::new(SDCARD_ROOT);
    let state = match crate::app::upgrade_worker::is_mount_point(card) {
        Ok(true) => Card::Mounted,
        Ok(false) if crate::app::upgrade_worker::card_inserted() => Card::Unmounted,
        Ok(false) => Card::Absent,
        Err(e) => return Err(format!("cannot tell whether {SDCARD_ROOT} is mounted: {e}")),
    };
    list_local_in(source, running, card, state)
}

/// What is in the SD card slot, as far as the daemon can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Card {
    /// No card: `/mnt/sdcard` is an empty directory of the root filesystem.
    Absent,
    /// A card the kernel sees (`/dev/mmcblk0`) that nothing has mounted -- a
    /// filesystem it does not know, or a damaged one.
    Unmounted,
    Mounted,
}

/// [`list_local`] with the card's root and state passed in, so each state of a
/// real board can be tested without a mount.
///
/// **A source on the card that has nothing to read is not an error.** Without
/// a card the board has `/mnt/sdcard` as an empty directory on its root
/// filesystem, and a card that was never given a `firmware` folder has no
/// such directory either. Both are the ordinary state of a board and both
/// mean "this source offers nothing" -- which is what an empty list with no
/// error says. Reporting them as errors made every card-less board show a
/// warning, retry the whole fan-out every two minutes, and never persist its
/// catalogue.
///
/// What stays an error is a card that IS there and cannot be read: one the
/// kernel sees but nothing mounted, a `firmware` that is a file, a directory
/// that will not open. Those are things the operator can act on. A source
/// outside the card keeps the old rule too, since a location that is missing
/// there is a typo, not a state.
fn list_local_in(
    source: &Source,
    running: &str,
    card_root: &std::path::Path,
    card: Card,
) -> Result<Vec<Candidate>, String> {
    let dir = std::path::Path::new(&source.location);
    let on_card = dir.starts_with(card_root);
    // A folder left on the mount point's own directory (the root filesystem's)
    // is not the card's, and listing it would offer images that vanish when a
    // card is inserted.
    if on_card {
        match card {
            Card::Mounted => {}
            Card::Absent => return Ok(Vec::new()),
            Card::Unmounted => {
                return Err(
                    "an SD card is inserted but not mounted (unsupported or damaged filesystem?)"
                        .to_string(),
                );
            }
        }
    }
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(format!("{} is not a directory", source.location)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return if on_card {
                Ok(Vec::new())
            } else {
                Err(format!("{} is not a directory", source.location))
            };
        }
        Err(e) => return Err(format!("cannot read {}: {e}", source.location)),
    }

    let mut candidates = Vec::new();
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", source.location))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("tpu") {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let version = crate::app::upgrade_worker::tag_from_ota_name(&name)
            .map(str::to_string)
            .unwrap_or_else(|| name.clone());
        // A file on a card carries no promise that its name reflects its
        // contents -- but that is what `trust` says, and it already says
        // Unverified for every one of these. Refusing to order them as well
        // hides a newer image under "older or unrelated": a v2.8.1-rc1 parked
        // on the card while v2.8.0 ran showed as `?` and `firmware check`
        // never mentioned it. Order what is version-shaped, decline the rest.
        let relation = match crate::app::version::compare(&version, running) {
            Some(std::cmp::Ordering::Equal) => Relation::Current,
            Some(std::cmp::Ordering::Greater) => Relation::Newer,
            Some(std::cmp::Ordering::Less) => Relation::Older,
            None if version == running => Relation::Current,
            None => Relation::Unknown,
        };
        let prerelease = crate::app::version::is_prerelease(&version);
        candidates.push(Candidate {
            version,
            relation,
            prerelease,
            trust: Trust::Unverified,
            file: Some(path.to_string_lossy().to_string()),
            size_bytes: entry.metadata().ok().map(|m| m.len()),
        });
    }
    // Newest first, by the same ordering the relation used -- a lexical sort
    // here would put v2.9.0 above v2.10.0 in a list whose relations say the
    // opposite.
    candidates.sort_by(|a, b| {
        crate::app::version::compare(&b.version, &a.version)
            .unwrap_or_else(|| b.version.cmp(&a.version))
    });
    Ok(candidates)
}

/// What one source came to: its candidates, its error, the running version it
/// reported.
type Resolved = (Vec<Candidate>, Option<String>, Option<String>);

fn resolve(source: &Source, running_hint: &str, deadline: Duration) -> Resolved {
    match source.kind {
        SourceKind::Local => match list_local(source, running_hint) {
            Ok(c) => (c, None, None),
            Err(e) => (Vec::new(), Some(e), None),
        },
        SourceKind::Github | SourceKind::Http => match list_remote(source, deadline) {
            Ok(listing) => {
                let trust = match source.kind {
                    SourceKind::Github => Trust::Verified,
                    // The publisher ships no checksums; upstream's server has
                    // none at all. TLS is the whole of the guarantee.
                    _ => Trust::Tls,
                };
                let running = listing.running.clone();
                let candidates = listing
                    .releases
                    .into_iter()
                    .map(|r| Candidate {
                        version: r.tag,
                        relation: relation_from(r.relation.as_deref()),
                        prerelease: r.prerelease,
                        trust,
                        file: None,
                        size_bytes: None,
                    })
                    .collect();
                (candidates, None, Some(running))
            }
            Err(e) => (Vec::new(), Some(e), None),
        },
    }
}

/// Where the last good listing is kept between boots.
///
/// Beside `firmware-sources.json`, and on the overlay for the same reason:
/// both firmware images mount it, so the cache survives an A/B promotion --
/// which is exactly the moment someone opens this page.
pub const CATALOG_PATH: &str = "/mnt/overlay/firmware-catalog.json";

/// The longest a cache read off disk is allowed to claim as its age.
///
/// A board whose clock was wrong when it wrote, or which has been off for a
/// month, would otherwise report an age measured in weeks or a negative one.
/// Either is nonsense to show; both mean the same thing, which is "old enough
/// that it is being refreshed right now".
const MAX_RESTORED_AGE: Duration = Duration::from_secs(86_400);

/// Read the listing left by the last run, if there is one.
///
/// Every failure here is a miss, not an error: an absent file is the normal
/// state of a board that has never refreshed, and a corrupt one is not worth
/// refusing to boot over. The fan-out that follows will replace it.
async fn restore() -> Option<Cached> {
    let body = tokio::fs::read_to_string(CATALOG_PATH).await.ok()?;
    let value: Catalog = serde_json::from_str(&body)
        .map_err(|e| tracing::warn!("ignoring an unreadable {CATALOG_PATH}: {e}"))
        .ok()?;

    // Age comes from the stored timestamp, not from the file's mtime: what a
    // reader needs to know is when the SOURCES were asked, and the two differ
    // whenever the file was copied, restored from a backup, or written by the
    // other firmware image.
    let age = chrono::DateTime::parse_from_rfc3339(&value.checked_at)
        .ok()
        .and_then(|when| {
            (chrono::Utc::now() - when.with_timezone(&chrono::Utc))
                .to_std()
                .ok()
        })
        .unwrap_or(MAX_RESTORED_AGE)
        .min(MAX_RESTORED_AGE);

    Some(Cached {
        at: Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
        value,
        // Restored, therefore stale by definition: `get` compares the age
        // against FRESH and starts a refresh behind the answer it serves.
        ok: true,
        slow_only: false,
        stale: false,
    })
}

/// Do two catalogues say the same thing about what is on offer?
///
/// `checked_at` and `age_seconds` move on every refresh and say nothing about
/// the sources, so they are not part of the answer. Comparing the whole struct
/// would make every refresh a change, which is exactly the write this is here
/// to avoid.
fn same_offering(a: &Catalog, b: &Catalog) -> bool {
    a.running == b.running && a.sources == b.sources
}

/// Write the listing, but only if it says something different.
///
/// THE COMPARISON IS THE POINT. This is NAND, and on the reference board UBI
/// reports five free eraseblocks of 2040. A catalogue rewritten on every
/// refresh would be thousands of writes a year to a flash that is already
/// fully allocated, for bytes that change when somebody publishes a release --
/// a few times a week at most.
///
/// Failures are logged and swallowed. A board that cannot write its cache
/// still works; it just pays for a fan-out after each boot, which is what it
/// did before this existed.
async fn persist(value: &Catalog) {
    if !crate::app::firmware_sources::storage_available() {
        return;
    }
    if let Ok(existing) = tokio::fs::read_to_string(CATALOG_PATH).await {
        if let Ok(old) = serde_json::from_str::<Catalog>(&existing) {
            if same_offering(&old, value) {
                return;
            }
        }
    }

    let body = match serde_json::to_string(value) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!("cannot serialise the firmware catalogue: {e}");
            return;
        }
    };
    if let Err(e) = tokio::fs::write(CATALOG_PATH, body).await {
        tracing::warn!("cannot write {CATALOG_PATH}: {e}");
    }
}

struct Cached {
    at: Instant,
    value: Catalog,
    ok: bool,
    /// Not `ok`, and every error is a source being slow. See [`FRESH_AFTER_SLOW`].
    slow_only: bool,
    /// Something this listing reads has changed since it was taken, so the
    /// next `get` refreshes whatever its age. See [`invalidate`].
    stale: bool,
}

impl Cached {
    /// How long this answer is good for.
    fn ttl(&self) -> Duration {
        if self.ok {
            FRESH
        } else if self.slow_only {
            FRESH_AFTER_SLOW
        } else {
            FRESH_AFTER_ERROR
        }
    }
}

/// Is it time to ask the sources again? The one place the rule is written, so
/// a test of it is a test of `get`.
fn needs_refresh(force: bool, stale: bool, age: Duration, ttl: Duration) -> bool {
    force || stale || age >= ttl
}

/// How many times [`invalidate`] has been called. A fan-out compares the value
/// it started with against the value when it finishes, so a change made while
/// it was running -- after it had read the source that changed -- is not
/// papered over by a result that looks fresh.
static INVALIDATIONS: AtomicU64 = AtomicU64::new(0);

/// Marks the cached listing as out of date, so the next `get` starts a
/// refresh.
///
/// Called when something the catalogue reads is changed by the daemon itself:
/// an image parked on the card is invisible until the `local` source is read
/// again, and without this it stayed so for up to [`FRESH`]. Nothing is
/// fetched here; the cost is paid by the next reader, as for any other expiry.
pub async fn invalidate() {
    // The count moves under the cache lock, and a fan-out reads it under the
    // same lock when it stores its result, so either the fan-out sees this
    // change or this sees the fan-out's entry. Neither can miss the other.
    let mut guard = cache().lock().await;
    INVALIDATIONS.fetch_add(1, AtomicOrdering::AcqRel);
    if let Some(cached) = guard.as_mut() {
        cached.stale = true;
    }
}

fn cache() -> &'static Mutex<Option<Cached>> {
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// The longest a fan-out may hold the claim before another may take it.
///
/// Not a guess at how long the work takes -- it is how long a LOST refresh is
/// allowed to block every future one. The fan-out was 74-78 s when it was
/// first measured; on 2026-09-12 bmc-1 took 75-100 s and bmc-2 took about
/// 230 s for the same four sources. Ten minutes is comfortably past the
/// slowest real one and still short enough that a person who walks away and
/// comes back finds a board that works.
const REFRESH_CLAIM: Duration = Duration::from_secs(600);

/// When the running fan-out claimed the right to run, as seconds since this
/// process started. Zero means nobody holds it.
///
/// A bool here was a deadlock. `swap(true)` claims and `Drop` releases, which
/// is correct for a panic and wrong for everything else: a task dropped before
/// its first poll never constructs the guard, and a blocking join that never
/// returns never drops it. Either way the flag stayed set, `spawn_refresh`
/// returned early for ever after, and the board could not check for firmware
/// again until `bmcd` restarted. bmc-2 sat like that for three and a half
/// hours on 2026-09-11 with no fan-out process running at all (SQU-201).
///
/// A deadline cannot deadlock. The claim expires whether or not anything is
/// there to release it.
fn refresh_claim() -> &'static AtomicU64 {
    static CLAIMED_AT: OnceLock<AtomicU64> = OnceLock::new();
    CLAIMED_AT.get_or_init(|| AtomicU64::new(0))
}

/// Seconds since the process started, never zero, so zero can mean "unclaimed".
fn now_secs() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs() + 1
}

/// May a caller take the claim, given who holds it and what time it is?
///
/// Pure, and the only place the rule is written. `refreshing()` is its
/// negation and `claim_refresh()` is it plus a compare-exchange, so a test of
/// this is a test of both rather than of a copy of them.
fn may_claim(held: u64, now: u64) -> bool {
    held == 0 || now.saturating_sub(held) >= REFRESH_CLAIM.as_secs()
}

/// Is a fan-out running, as far as anyone can tell from here?
fn refreshing() -> bool {
    !may_claim(refresh_claim().load(AtomicOrdering::Acquire), now_secs())
}

/// Take the right to run a fan-out, unless someone holds it and has not run
/// out of time.
///
/// The compare-exchange is what keeps the dedup the old bool provided: without
/// it, every "check now" and every page opened during a refresh would start
/// another, and four sources would become twelve requests against a GitHub
/// quota of sixty an hour.
fn claim_refresh() -> bool {
    let now = now_secs();
    loop {
        let held = refresh_claim().load(AtomicOrdering::Acquire);
        if !may_claim(held, now) {
            return false;
        }
        if refresh_claim()
            .compare_exchange(held, now, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_ok()
        {
            return true;
        }
    }
}

fn release_refresh() {
    refresh_claim().store(0, AtomicOrdering::Release);
}

/// Sources whose blocking call has not returned, by id.
///
/// A blocking thread cannot be cancelled. A remote source's child is killed at
/// the deadline so its thread returns; an in-process read wedged on a dying
/// card would not, and without this every refresh would leave one more such
/// thread behind. A source still in here is skipped, not started again.
fn in_flight() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static IN_FLIGHT: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

/// Removes a source from [`in_flight`] when its blocking call ends, however
/// it ends.
struct InFlight(String);

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut set) = in_flight().lock() {
            set.remove(&self.0);
        }
    }
}

/// Asks each source, all at once, and waits for none of them longer than
/// `deadline` (plus `grace`).
///
/// A source that is still going when its turn is up reports "timed out" as its
/// own error and the rest are kept. Without the bound one hung source held the
/// whole fan-out until [`REFRESH_CLAIM`], and the timeout around the fan-out
/// then threw away the answers of every source that had already finished.
///
/// The resolver is a parameter so a test can use a source that sleeps. A
/// resolver that starts a child process must enforce `deadline` on it itself
/// (see [`output_within`]): the blocking thread cannot be cancelled from here,
/// only abandoned, and an in-process read that never returns simply has its
/// result dropped.
async fn gather<F>(
    sources: Vec<Source>,
    deadline: Duration,
    grace: Duration,
    resolver: F,
) -> Vec<(
    usize,
    Source,
    Vec<Candidate>,
    Option<String>,
    Option<String>,
)>
where
    F: Fn(&Source, Duration) -> Resolved + Send + Sync + 'static,
{
    let resolver = std::sync::Arc::new(resolver);
    let mut tasks = tokio::task::JoinSet::new();
    for (index, source) in sources.into_iter().enumerate() {
        let resolver = resolver.clone();
        tasks.spawn(async move {
            let claimed = in_flight()
                .lock()
                .map(|mut set| set.insert(source.id.clone()))
                .unwrap_or(true);
            if !claimed {
                tracing::warn!("firmware source {} is {STILL_BUSY}; skipping it", source.id);
                return (
                    index,
                    source,
                    Vec::new(),
                    Some(STILL_BUSY.to_string()),
                    None,
                );
            }
            let guard = InFlight(source.id.clone());
            let for_blocking = source.clone();
            let work = tokio::task::spawn_blocking(move || {
                let _guard = guard;
                resolver(&for_blocking, deadline)
            });
            let (candidates, error, running) = match tokio::time::timeout(deadline + grace, work)
                .await
            {
                Ok(Ok(resolved)) => resolved,
                Ok(Err(e)) => {
                    // A panicking source must not take the others with it,
                    // and must not be reported as "nothing new" either.
                    tracing::error!("firmware source {} panicked while listing: {e}", source.id);
                    (Vec::new(), Some(format!("failed while listing: {e}")), None)
                }
                Err(_) => {
                    tracing::warn!("firmware source {} {}", source.id, timed_out(deadline));
                    (Vec::new(), Some(timed_out(deadline)), None)
                }
            };
            (index, source, candidates, error, running)
        });
    }

    let mut resolved = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(one) => resolved.push(one),
            Err(e) => tracing::error!("a firmware source task failed: {e}"),
        }
    }
    resolved
}

/// Asks every enabled source what it offers, all at once.
///
/// One `spawn_blocking` per source rather than one for the lot. Each runs
/// `tpi-selfupdate --list`, which bounds itself with `curl --max-time 30`, so
/// the whole fan-out costs the slowest source rather than the sum of them.
/// Measured on the board before this change: four sources, 16 s; and asking
/// for one source cost the same 16 s, because the refresh was never per
/// source in the first place.
///
/// **That 16 s no longer holds.** Timed twice on the board on 2026-09-09, by
/// watching `checked_at` advance after a forced poll: **74 s and 78 s**. The
/// slowest source is `firmware.turingpi.com`, and it got slower, not us --
/// the shape is unchanged. Anything that waits on a poll must be sized
/// against the larger figure; `tpi` 1.5.1 allows 180 s for exactly this and
/// has a test pinning that bound to the measurement.
async fn fan_out() -> Catalog {
    let invalidations = INVALIDATIONS.load(AtomicOrdering::Acquire);
    let sources = crate::app::firmware_sources::load().await;
    // The api layer already reads /etc/os-release for this; a second reader
    // here could drift from the one the About page uses.
    let running_hint = crate::api::legacy::firmware_version()
        .await
        .unwrap_or_else(|| "unknown".to_string());

    let enabled: Vec<Source> = sources.sources.into_iter().filter(|s| s.enabled).collect();
    let running_hint_for_sources = running_hint.clone();

    let mut resolved = gather(
        enabled,
        SOURCE_DEADLINE,
        SOURCE_GRACE,
        move |source, deadline| resolve(source, &running_hint_for_sources, deadline),
    )
    .await;
    // Answers arrive in whatever order they finish; the page's order is the
    // configured one.
    resolved.sort_by_key(|(index, ..)| *index);

    // The updater knows the running version too; prefer what it reported over
    // the local read, so both halves of the page agree.
    let running = resolved
        .iter()
        .find_map(|(_, _, _, _, r)| r.clone())
        .unwrap_or(running_hint);

    let mut ok = true;
    let mut slow_only = true;
    let sources = resolved
        .into_iter()
        .map(|(_, source, candidates, error, _)| {
            if let Some(error) = &error {
                ok = false;
                slow_only &= is_slow(error);
            }
            SourceCatalog {
                id: source.id,
                label: source.label,
                kind: source.kind,
                location: source.location,
                candidates,
                error,
            }
        })
        .collect();

    let value = Catalog {
        checked_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        running,
        sources,
        refreshing: false,
        age_seconds: 0,
    };

    // The guard is taken BEFORE the count is read, and `invalidate` moves the
    // count under the same lock: a park that lands now is either counted here
    // or marks the entry this stores, never neither.
    {
        let mut guard = cache().lock().await;
        let stale = INVALIDATIONS.load(AtomicOrdering::Acquire) != invalidations;
        *guard = Some(Cached {
            at: Instant::now(),
            value: value.clone(),
            ok,
            slow_only: !ok && slow_only,
            stale,
        });
    }

    // Only a complete answer is kept. A fan-out where one source errored is
    // good enough to show -- the page renders the error beside the others --
    // and not good enough to leave on disk, where a transient failure would
    // become the listing a board boots with for as long as it keeps failing.
    if ok {
        persist(&value).await;
    }

    value
}

/// Starts a fan-out behind the caller's back, unless one is already running.
///
/// The lock is taken only to store the result. Holding it across the fan-out
/// -- which is what this used to do -- meant a "check now" blocked every other
/// reader of the catalogue for as long as the slowest source took, so a page
/// that wanted nothing but the cached list froze too.
fn spawn_refresh() {
    if !claim_refresh() {
        return;
    }
    tokio::spawn(async {
        // Three things release the claim, and it takes all three.
        //
        // The guard covers a panic. The timeout covers a fan-out that never
        // finishes -- one `spawn_blocking` per source, and a blocking join
        // that never returns would otherwise hold the guard for the life of
        // the process. And the claim's own deadline covers this task never
        // being polled at all, which constructs no guard and reaches no
        // timeout.
        //
        // Only the first two are visible here. The third is the reason the
        // claim is a deadline rather than a flag: a release that depends on
        // this code running cannot rescue the case where it does not.
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                release_refresh();
            }
        }
        let _clear = Clear;
        if tokio::time::timeout(REFRESH_CLAIM, fan_out())
            .await
            .is_err()
        {
            tracing::warn!(
                "the firmware catalogue refresh did not finish within {}s; \
                 leaving the cached listing in place",
                REFRESH_CLAIM.as_secs()
            );
        }
    });
}

/// Primes the catalogue at start-up, so the first page to ask has an answer.
///
/// Two halves. The listing the last run wrote is loaded FIRST and
/// synchronously enough that the next caller gets it, which is what makes a
/// board answer immediately after a reboot instead of making the person who
/// just flashed it wait through a fan-out. Then the refresh starts behind it,
/// exactly as it always did, and the answer is replaced when it arrives.
///
/// Restored or not, the age is real: `get` reports it, and a page that says
/// "checked four hours ago" while it refreshes is telling the truth about
/// both.
pub fn prime() {
    tokio::spawn(async {
        if let Some(restored) = restore().await {
            let mut guard = cache().lock().await;
            if guard.is_none() {
                tracing::info!(
                    "restored the firmware catalogue from {CATALOG_PATH}, checked {}",
                    restored.value.checked_at
                );
                *guard = Some(restored);
            }
        }
        spawn_refresh();
    });
}

/// Every enabled source's offering.
///
/// Answers from the cache and refreshes behind it. `force` -- what a "check
/// now" control sends -- starts the refresh immediately rather than waiting
/// for the entry to age out, and still returns at once with `refreshing` set,
/// because a control that freezes the page it is on cannot be used to confirm
/// a release you just published.
///
/// Only a cold cache waits, and only for one fan-out; `prime()` makes that
/// rare.
pub async fn get(force: bool) -> Catalog {
    let Some((mut value, age, ttl, stale)) = snapshot().await else {
        return cold().await;
    };

    if needs_refresh(force, stale, age, ttl) {
        spawn_refresh();
    }

    value.age_seconds = age.as_secs();
    value.refreshing = refreshing();
    value
}

/// The cached answer, how old it is, how long it is good for, and whether it
/// has been marked stale.
async fn snapshot() -> Option<(Catalog, Duration, Duration, bool)> {
    let guard = cache().lock().await;
    guard
        .as_ref()
        .map(|c| (c.value.clone(), c.at.elapsed(), c.ttl(), c.stale))
}

/// How often a caller with nothing cached looks again.
const COLD_POLL: Duration = Duration::from_millis(250);

/// A caller that finds nothing cached waits for THE refresh, not one of its own.
///
/// This used to run a fan-out directly, bypassing the claim, so every cold
/// request started another. With a fan-out that can take minutes, a few
/// requests from an impatient page would be a dozen `curl`s against a GitHub
/// allowance of sixty an hour. It now joins the refresh that `prime()` started
/// (or starts the one) and answers when it stores its result, as the direct
/// call did, so a client that waits for a complete answer -- `tpi firmware
/// check` -- still gets one.
///
/// Bounded by [`REFRESH_CLAIM`]. If nothing has been stored by then, the
/// answer is an empty list with `refreshing: true`, which is what the web
/// interface already draws while a refresh runs.
async fn cold() -> Catalog {
    let started = Instant::now();
    while started.elapsed() < REFRESH_CLAIM {
        spawn_refresh();
        tokio::time::sleep(COLD_POLL).await;
        if let Some((mut value, age, _, _)) = snapshot().await {
            value.age_seconds = age.as_secs();
            value.refreshing = refreshing();
            return value;
        }
    }
    Catalog {
        checked_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        running: "unknown".to_string(),
        sources: Vec::new(),
        refreshing: true,
        age_seconds: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::failure_reason;

    /// The page shows this sentence and nothing else, so it has to be the
    /// updater's own words when it has any.
    #[test]
    fn a_failed_listing_is_explained_by_the_updater_when_it_said_anything() {
        assert_eq!(
            failure_reason(
                "tpi-selfupdate: cannot reach the GitHub API\ncurl: (6) Could not resolve host: api.github.com\n",
                Some(6)
            ),
            "curl: (6) Could not resolve host: api.github.com"
        );
    }

    /// And when it said nothing, the status is all there is -- but "no
    /// output" was not something a reader could act on.
    #[test]
    fn a_silent_failure_names_the_exit_code_rather_than_saying_no_output() {
        let said = failure_reason("", Some(2));
        assert!(said.contains("exited 2"), "{said}");
        assert!(!said.contains("no output"), "{said}");
        assert!(failure_reason("", None).contains("signal"));
    }

    use super::*;
    use tempdir::TempDir;

    #[test]
    fn a_missing_relation_is_unknown_not_newer() {
        // An older updater emits no `relation`. Defaulting to Newer would
        // offer every release as an upgrade; Unknown says what is true.
        assert_eq!(relation_from(None), Relation::Unknown);
        assert_eq!(relation_from(Some("nonsense")), Relation::Unknown);
        assert_eq!(relation_from(Some("newer")), Relation::Newer);
        assert_eq!(relation_from(Some("older")), Relation::Older);
        assert_eq!(relation_from(Some("current")), Relation::Current);
    }

    #[test]
    fn the_updaters_listing_deserialises() {
        let line = r#"{"repo":"x/y","running":"v2.6.0","releases":[{"tag":"v2.6.0","prerelease":false,"newer":false,"current":true,"relation":"current"}]}"#;
        let l: Listing = serde_json::from_str(line).expect("parses");
        assert_eq!(l.running, "v2.6.0");
        assert_eq!(l.releases.len(), 1);
        assert_eq!(
            relation_from(l.releases[0].relation.as_deref()),
            Relation::Current
        );
    }

    /// A parked image whose name carries a version must be ordered against
    /// what is running, not hidden under "unknown".
    ///
    /// This is the case that was found on the board: v2.8.1-rc1 sitting on the
    /// SD card while v2.8.0 ran showed as `?`, so it sorted under "older or
    /// unrelated" and `firmware check` never mentioned it. `trust` already
    /// says Unverified for every local file -- that is the column which
    /// carries "a name is not a promise", and refusing to order as well told
    /// the user nothing twice.
    #[test]
    fn a_parked_image_is_ordered_against_the_running_version() {
        let dir = TempDir::new("catalog").expect("tempdir");
        for name in [
            "tp2-bmc-firmware-ota-v2.8.1-rc1.tpu",
            "tp2-bmc-firmware-ota-v2.7.0.tpu",
            "tp2-bmc-firmware-ota-v2.10.0.tpu",
            "tp2-bmc-firmware-ota-v2.9.0.tpu",
            "tp2-bmc-firmware-ota-local.tpu",
            "not-an-image.txt",
        ] {
            std::fs::write(dir.path().join(name), b"x").expect("write");
        }

        let source = Source {
            id: "local".into(),
            label: "SD card".into(),
            kind: SourceKind::Local,
            location: dir.path().to_string_lossy().to_string(),
            enabled: true,
        };
        let found = list_local(&source, "v2.8.0").expect("lists");

        let by_version = |v: &str| {
            found
                .iter()
                .find(|c| c.version == v)
                .unwrap_or_else(|| panic!("{v} missing from {found:?}"))
                .clone()
        };

        assert_eq!(found.len(), 5, "the .txt is not an image: {found:?}");
        assert_eq!(by_version("v2.8.1-rc1").relation, Relation::Newer);
        assert!(by_version("v2.8.1-rc1").prerelease);
        assert_eq!(by_version("v2.7.0").relation, Relation::Older);
        assert!(!by_version("v2.7.0").prerelease);

        // An unversioned build still orders against nothing, and is not a
        // prerelease -- it is simply not a release.
        assert_eq!(by_version("local").relation, Relation::Unknown);
        assert!(!by_version("local").prerelease);

        // Every local file is unverified regardless of its name.
        assert!(found.iter().all(|c| c.trust == Trust::Unverified));

        // Newest first, numerically: v2.10.0 above v2.9.0, which a lexical
        // sort reverses.
        let order: Vec<&str> = found.iter().map(|c| c.version.as_str()).collect();
        let ten = order.iter().position(|v| *v == "v2.10.0").expect("2.10.0");
        let nine = order.iter().position(|v| *v == "v2.9.0").expect("2.9.0");
        assert!(ten < nine, "v2.10.0 must sort above v2.9.0: {order:?}");
    }

    /// A running version that cannot be ordered leaves everything unknown
    /// rather than inventing a direction. `just build` stamps VERSION=local,
    /// and ordering real tags against it once offered v2.3.0 as an upgrade to
    /// a board running newer code.
    #[test]
    fn nothing_is_ordered_against_an_unversioned_running_build() {
        let dir = TempDir::new("catalog").expect("tempdir");
        std::fs::write(dir.path().join("tp2-bmc-firmware-ota-v2.3.0.tpu"), b"x").expect("write");

        let source = Source {
            id: "local".into(),
            label: "SD card".into(),
            kind: SourceKind::Local,
            location: dir.path().to_string_lossy().to_string(),
            enabled: true,
        };
        let found = list_local(&source, "local").expect("lists");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].relation, Relation::Unknown);
    }

    /// One source that hangs past the deadline must not cost the others their
    /// answers: the fast one is kept whole and the slow one says it timed out.
    #[tokio::test]
    async fn a_slow_source_times_out_alone() {
        let source = |id: &str| Source {
            id: id.into(),
            label: id.into(),
            kind: SourceKind::Local,
            location: format!("/nonexistent/{id}"),
            enabled: true,
        };
        let candidate = Candidate {
            version: "v2.9.0".into(),
            relation: Relation::Newer,
            prerelease: false,
            trust: Trust::Unverified,
            file: None,
            size_bytes: None,
        };
        let kept = candidate.clone();
        let resolved = gather(
            vec![source("slow"), source("fast")],
            Duration::from_millis(200),
            Duration::ZERO,
            move |source, _| {
                if source.id == "slow" {
                    std::thread::sleep(Duration::from_millis(1500));
                    (Vec::new(), None, None)
                } else {
                    (vec![kept.clone()], None, Some("v2.8.0".into()))
                }
            },
        )
        .await;

        let find = |id: &str| {
            resolved
                .iter()
                .find(|(_, s, ..)| s.id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
        };
        let (_, _, fast_candidates, fast_error, fast_running) = find("fast");
        assert_eq!(fast_candidates, &vec![candidate]);
        assert_eq!(fast_error, &None);
        assert_eq!(fast_running.as_deref(), Some("v2.8.0"));

        let (_, _, slow_candidates, slow_error, _) = find("slow");
        assert!(slow_candidates.is_empty());
        let message = slow_error.as_deref().unwrap_or_default();
        assert!(message.starts_with("timed out after"), "{message}");
    }

    /// The pid a shell wrote, waiting (bounded) for it to be there.
    fn pid_written_to(pidfile: &std::path::Path) -> i32 {
        for _ in 0..100 {
            if let Some(pid) = std::fs::read_to_string(pidfile)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the shell never wrote {}", pidfile.display());
    }

    /// Dead means gone, or a zombie waiting for an init that has not got to it
    /// yet (a container's pid 1 may never). Gives the kernel a moment.
    fn is_gone(pid: i32) -> bool {
        let running = |pid: i32| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')
                        .and_then(|(_, rest)| rest.trim_start().chars().next())
                })
                .is_some_and(|state| state != 'Z' && state != 'X')
        };
        for _ in 0..50 {
            if !running(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// The child, not just the wait, ends at the deadline. A shell that runs a
    /// long `sleep` is the shape of `tpi-selfupdate` running `curl`: killing
    /// only the shell would leave the `sleep` holding the pipes.
    #[test]
    fn a_hung_child_and_its_children_are_killed_at_the_deadline() {
        let marker = TempDir::new("hung").expect("tempdir");
        let pidfile = marker.path().join("pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", pidfile.display()));

        let started = Instant::now();
        let out = output_within(command, Duration::from_secs(2)).expect("spawns");
        assert!(out.is_none(), "it should have timed out");
        assert!(started.elapsed() < Duration::from_secs(10));

        assert!(
            is_gone(pid_written_to(&pidfile)),
            "the grandchild is still running"
        );
    }

    /// The script exits at once and leaves its `curl` holding the pipes. The
    /// output is not complete until they close, so the wait is bounded by the
    /// deadline, not by the grandchild, and the group is still killed -- which
    /// only works because the exited script has not been reaped yet.
    #[test]
    fn a_grandchild_holding_the_pipes_after_the_child_exits_is_bounded_and_killed() {
        let marker = TempDir::new("holder").expect("tempdir");
        let pidfile = marker.path().join("pid");
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "sleep 30 & echo $! > {}; exit 0",
            pidfile.display()
        ));

        let started = Instant::now();
        let out = output_within(command, Duration::from_secs(2)).expect("spawns");
        assert!(out.is_none(), "the pipes never closed, so it timed out");
        assert!(started.elapsed() < Duration::from_secs(10));

        assert!(
            is_gone(pid_written_to(&pidfile)),
            "the grandchild is still running"
        );
    }

    #[test]
    fn a_child_that_finishes_in_time_returns_its_output() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("echo out; echo err >&2; exit 3");
        let out = output_within(command, Duration::from_secs(10))
            .expect("spawns")
            .expect("finishes");
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    }

    /// A source whose call never returned is skipped on the next fan-out, not
    /// started again: no second thread behind the first.
    #[tokio::test]
    async fn a_source_still_running_from_the_last_check_is_skipped() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::{mpsc, Arc, Mutex as StdMutex};

        let source = Source {
            id: "wedged-test-source".into(),
            label: "wedged".into(),
            kind: SourceKind::Local,
            location: "/nonexistent/wedged".into(),
            enabled: true,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, wait) = mpsc::channel::<()>();
        let wait = Arc::new(StdMutex::new(wait));
        let resolver = {
            let calls = calls.clone();
            let wait = wait.clone();
            move |_: &Source, _: Duration| -> Resolved {
                calls.fetch_add(1, AtomicOrdering::SeqCst);
                let _ = wait.lock().unwrap().recv();
                (Vec::new(), None, None)
            }
        };

        let first = gather(
            vec![source.clone()],
            Duration::from_millis(100),
            Duration::ZERO,
            resolver.clone(),
        )
        .await;
        assert!(first[0]
            .3
            .as_deref()
            .unwrap_or_default()
            .starts_with("timed out"));

        // The first call is still blocked on `wait`.
        let second = gather(
            vec![source.clone()],
            Duration::from_millis(100),
            Duration::ZERO,
            resolver,
        )
        .await;
        assert_eq!(second[0].3.as_deref(), Some(STILL_BUSY));
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1, "no second call");
        assert!(is_slow(STILL_BUSY));

        // Let it go; the source can be asked again once it has returned.
        release.send(()).unwrap();
        for _ in 0..100 {
            if !in_flight().lock().unwrap().contains(&source.id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!in_flight().lock().unwrap().contains(&source.id));
    }

    #[test]
    fn slow_sources_are_retried_later_than_broken_ones() {
        let cached = |ok, slow_only| Cached {
            at: Instant::now(),
            value: empty_catalog(false, 0),
            ok,
            slow_only,
            stale: false,
        };
        assert_eq!(cached(true, false).ttl(), FRESH);
        assert_eq!(cached(false, false).ttl(), FRESH_AFTER_ERROR);
        assert_eq!(cached(false, true).ttl(), FRESH_AFTER_SLOW);
        assert!(is_slow(&timed_out(SOURCE_DEADLINE)));
        assert!(!is_slow("curl: (6) Could not resolve host"));
        assert!(SOURCE_DEADLINE + SOURCE_GRACE < REFRESH_CLAIM);
    }

    /// `get` asks this, so a stale mark is honoured at any age and a fresh,
    /// unmarked answer is left alone.
    #[test]
    fn a_stale_answer_is_refreshed_whatever_its_age() {
        let ttl = Duration::from_secs(1800);
        assert!(needs_refresh(false, true, Duration::ZERO, ttl));
        assert!(!needs_refresh(false, false, Duration::ZERO, ttl));
        assert!(needs_refresh(true, false, Duration::ZERO, ttl));
        assert!(needs_refresh(false, false, ttl, ttl));
    }

    fn card_source(card: &std::path::Path) -> Source {
        Source {
            id: "local".into(),
            label: "SD card".into(),
            kind: SourceKind::Local,
            location: card.join("firmware").to_string_lossy().to_string(),
            enabled: true,
        }
    }

    /// A board with no card: `/mnt/sdcard` is an empty directory on the root
    /// filesystem and nothing is mounted on it (`/proc/mounts` has no
    /// `/dev/mmcblk0p1 /mnt/sdcard vfat` line). The mount check is the shipped
    /// one -- a temporary directory shares its parent's device, exactly as
    /// the empty mount point does -- so this is not a stand-in for it.
    ///
    /// This used to be an error, which put a warning on every card-less board
    /// and held the catalogue at the two-minute retry for ever.
    #[test]
    fn no_card_is_an_empty_source_not_an_error() {
        let card = TempDir::new("sdcard").expect("tempdir");
        assert_eq!(
            crate::app::upgrade_worker::is_mount_point(card.path()).ok(),
            Some(false)
        );

        let source = card_source(card.path());
        let found = list_local_in(&source, "v2.8.0", card.path(), Card::Absent).expect("no error");
        assert!(found.is_empty(), "{found:?}");

        // Whatever sits in the empty mount point's directory is the root
        // filesystem's, not a card's, and is not offered.
        std::fs::create_dir_all(card.path().join("firmware")).expect("mkdir");
        std::fs::write(
            card.path().join("firmware/tp2-bmc-firmware-ota-v2.9.0.tpu"),
            b"x",
        )
        .expect("write");
        let found = list_local_in(&source, "v2.8.0", card.path(), Card::Absent).expect("no error");
        assert!(found.is_empty(), "{found:?}");
    }

    /// A card the kernel sees (`/dev/mmcblk0`) that nothing mounted is a
    /// fault the operator can fix, not an empty source.
    #[test]
    fn a_card_that_is_inserted_but_not_mounted_is_an_error() {
        let card = TempDir::new("sdcard").expect("tempdir");
        let source = card_source(card.path());
        let err =
            list_local_in(&source, "v2.8.0", card.path(), Card::Unmounted).expect_err("an error");
        assert!(err.contains("inserted but not mounted"), "{err}");
    }

    /// A card that is mounted and was never given a `firmware` folder.
    #[test]
    fn a_card_without_the_folder_is_an_empty_source_not_an_error() {
        let card = TempDir::new("sdcard").expect("tempdir");
        std::fs::write(card.path().join("talos.img"), b"x").expect("write");

        let source = card_source(card.path());
        let found = list_local_in(&source, "v2.8.0", card.path(), Card::Mounted).expect("no error");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_card_with_the_folder_lists_its_images() {
        let card = TempDir::new("sdcard").expect("tempdir");
        let folder = card.path().join("firmware");
        std::fs::create_dir_all(&folder).expect("mkdir");
        std::fs::write(folder.join("tp2-bmc-firmware-ota-v2.9.0.tpu"), b"x").expect("write");

        let source = card_source(card.path());
        let found = list_local_in(&source, "v2.8.0", card.path(), Card::Mounted).expect("lists");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].version, "v2.9.0");
        assert_eq!(found[0].relation, Relation::Newer);
    }

    /// What stays an error: a card that is there and cannot be read as a
    /// folder of images. Without these the fix would be "never report".
    #[test]
    fn a_card_that_is_there_but_unreadable_is_still_an_error() {
        let card = TempDir::new("sdcard").expect("tempdir");
        std::fs::write(card.path().join("firmware"), b"a file, not a folder").expect("write");

        let source = card_source(card.path());
        let err =
            list_local_in(&source, "v2.8.0", card.path(), Card::Mounted).expect_err("an error");
        assert!(err.contains("is not a directory"), "{err}");
    }

    /// A configured local folder away from the card that does not exist is a
    /// mistake in the configuration, not the state of a board.
    #[test]
    fn a_missing_folder_off_the_card_is_still_an_error() {
        let card = TempDir::new("sdcard").expect("tempdir");
        let elsewhere = TempDir::new("elsewhere").expect("tempdir");
        let source = Source {
            id: "usb".into(),
            label: "Elsewhere".into(),
            kind: SourceKind::Local,
            location: elsewhere
                .path()
                .join("absent")
                .to_string_lossy()
                .to_string(),
            enabled: true,
        };
        assert!(list_local_in(&source, "v2.8.0", card.path(), Card::Absent).is_err());
        assert!(list_local_in(&source, "v2.8.0", card.path(), Card::Mounted).is_err());
    }

    /// An image parked after the last fan-out must make the next `get` refresh.
    #[tokio::test]
    async fn invalidating_marks_the_cached_listing_stale() {
        *cache().lock().await = Some(Cached {
            at: Instant::now(),
            value: empty_catalog(false, 0),
            ok: true,
            slow_only: false,
            stale: false,
        });
        invalidate().await;
        let stale = cache().lock().await.as_ref().map(|c| c.stale);
        assert_eq!(stale, Some(true));
        *cache().lock().await = None;
    }

    fn empty_catalog(refreshing: bool, age_seconds: u64) -> Catalog {
        Catalog {
            checked_at: "2026-09-09T00:00:00Z".to_string(),
            running: "v2.8.1".to_string(),
            sources: Vec::new(),
            refreshing,
            age_seconds,
        }
    }

    /// The page tells "these are last half-hour's answers, a check is running"
    /// from "this is what the sources say" by this field alone. A settled
    /// catalogue must serialise as it always did, so an older consumer that
    /// never knew the field is unaffected.
    #[test]
    fn refreshing_is_only_on_the_wire_when_it_is_true() {
        let settled = serde_json::to_string(&empty_catalog(false, 12)).expect("serialises");
        assert!(!settled.contains("refreshing"), "{settled}");
        assert!(settled.contains("\"age_seconds\":12"), "{settled}");

        let refreshing = serde_json::to_string(&empty_catalog(true, 1801)).expect("serialises");
        assert!(refreshing.contains("\"refreshing\":true"), "{refreshing}");
    }

    /// Every press of "check now", and every page that opens while one is in
    /// flight, must join the refresh already running rather than start
    /// another. Four sources became twelve requests against a GitHub quota of
    /// sixty an hour otherwise.
    ///
    /// This calls the shipped rule. The version before it rebuilt the claim
    /// out of a local `AtomicBool` and asserted against that, which passed
    /// whatever the daemon did.
    #[test]
    fn only_one_refresh_runs_at_a_time() {
        let bound = REFRESH_CLAIM.as_secs();
        assert!(may_claim(0, 100), "nobody holds it, so anyone may");
        assert!(!may_claim(100, 100), "a refresh that just started holds it");
        assert!(
            !may_claim(100, 100 + bound - 1),
            "and still holds it one second inside the bound"
        );
    }

    /// The wedge this replaced a bool to fix.
    ///
    /// A task dropped before its first poll constructs no guard; a blocking
    /// join that never returns drops none. With a bool the claim was then held
    /// for the life of the process and the board could never check for
    /// firmware again -- bmc-2 sat that way for three and a half hours on
    /// 2026-09-11 with no fan-out process running at all (SQU-201).
    ///
    /// A deadline cannot be lost, so the next caller takes it.
    #[test]
    fn a_lost_refresh_stops_blocking_the_next_one() {
        let bound = REFRESH_CLAIM.as_secs();
        assert!(
            may_claim(100, 100 + bound),
            "at the bound the claim has expired and the next caller takes it"
        );
        assert!(
            may_claim(100, 100 + bound * 40),
            "and three and a half hours later it is certainly free"
        );
    }

    /// A claim released normally is free at once, not at the deadline: the
    /// common case must not wait ten minutes for the next "check now".
    #[test]
    fn a_finished_refresh_frees_the_claim_immediately() {
        assert!(
            may_claim(0, 100),
            "release stores zero, and zero is claimable now"
        );
    }

    /// `now_secs` must never return zero, because zero is how the claim says
    /// "nobody holds it". A clock that starts at zero would make the first
    /// refresh of a process look unclaimed to everyone.
    #[test]
    fn the_clock_never_collides_with_unclaimed() {
        assert!(now_secs() > 0);
    }

    fn one_source(version: &str) -> SourceCatalog {
        SourceCatalog {
            id: "fork".into(),
            label: "this fork".into(),
            kind: SourceKind::Github,
            location: "excavador-turing/BMC-Firmware".into(),
            candidates: vec![Candidate {
                version: version.into(),
                relation: Relation::Current,
                prerelease: false,
                trust: Trust::Verified,
                file: None,
                size_bytes: None,
            }],
            error: None,
        }
    }

    fn catalog_at(when: &str, version: &str, age: u64) -> Catalog {
        Catalog {
            checked_at: when.into(),
            running: "v2.26.0".into(),
            sources: vec![one_source(version)],
            refreshing: false,
            age_seconds: age,
        }
    }

    /// The stored catalogue is the wire catalogue, and the wire catalogue
    /// omits every field that is false or absent. Without `default` on those
    /// fields a settled listing would write cleanly and fail to load, and the
    /// board would silently go back to paying for a fan-out every boot.
    #[test]
    fn a_stored_catalogue_reads_back_as_itself() {
        let before = catalog_at("2026-09-12T00:00:00Z", "v2.26.0", 0);
        let body = serde_json::to_string(&before).expect("serialises");
        assert!(!body.contains("refreshing"), "{body}");
        assert!(!body.contains("prerelease"), "{body}");

        let after: Catalog = serde_json::from_str(&body).expect("reads back");
        assert_eq!(before, after);
    }

    /// The write-on-change rule, which exists because this is NAND: the
    /// reference board has five free eraseblocks of 2040, and a catalogue
    /// rewritten on every refresh would be thousands of writes a year for
    /// bytes that change when somebody publishes a release.
    #[test]
    fn only_a_changed_offering_is_worth_a_write() {
        let earlier = catalog_at("2026-09-12T00:00:00Z", "v2.26.0", 0);
        let later = catalog_at("2026-09-12T00:30:00Z", "v2.26.0", 1800);
        assert!(
            same_offering(&earlier, &later),
            "a later check of the same releases is not a change"
        );

        let published = catalog_at("2026-09-12T01:00:00Z", "v2.27.0", 0);
        assert!(
            !same_offering(&earlier, &published),
            "a new release is a change and must reach the disk"
        );
    }

    /// The shape the HTTP listing emits has a `url` key rather than `repo`,
    /// and must parse through the same type.
    #[test]
    fn the_http_listing_shape_parses_too() {
        let line = r#"{"url":"https://x/y","running":"v2.6.0","releases":[{"tag":"v2.0.5","prerelease":false,"newer":false,"current":false,"relation":"older"}]}"#;
        let l: Listing = serde_json::from_str(line).expect("parses");
        assert_eq!(
            relation_from(l.releases[0].relation.as_deref()),
            Relation::Older
        );
    }
}
