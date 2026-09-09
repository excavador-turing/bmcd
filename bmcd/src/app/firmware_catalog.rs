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
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
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
const FRESH_AFTER_ERROR: Duration = Duration::from_secs(120);

/// How a candidate relates to what is running.
///
/// `Unknown` is not a failure. A board built from a working tree reports
/// `VERSION=local`, and ordering that against real tags is meaningless --
/// measured on hardware, `sort -V` put `local` first and every release,
/// including genuinely older code, came out as an upgrade. Saying "unknown" is
/// the honest answer and lets the page say so rather than implying an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    Verified,
    Tls,
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub version: String,
    pub relation: Relation,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub prerelease: bool,
    pub trust: Trust,
    /// For a local candidate, the file it came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Catalog {
    pub checked_at: String,
    pub running: String,
    pub sources: Vec<SourceCatalog>,
    /// These are the previous answers and a refresh is running behind them.
    ///
    /// The page draws a spinner on its "check now" control and leaves the
    /// list underneath readable, rather than blanking or freezing. Skipped
    /// when false so a settled catalogue serialises as it always did.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
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

/// Runs the updater's listing for one remote source.
///
/// Blocking on purpose, called from `spawn_blocking`; the script bounds its own
/// network calls with `curl --max-time`.
fn list_remote(source: &Source) -> Result<Listing, String> {
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

    let output = command
        .output()
        .map_err(|e| format!("cannot run {SELFUPDATE}: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().find(|l| l.trim_start().starts_with('{'));

    match line {
        Some(line) => serde_json::from_str(line)
            .map_err(|e| format!("cannot parse the updater's listing: {e}")),
        None => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(stderr
                .lines()
                .last()
                .unwrap_or("no output")
                .trim()
                .to_string())
        }
    }
}

/// Scans a directory for images.
///
/// Metadata only -- name and size. A sha256 costs about two seconds per image
/// on this SoC, which is fine on demand and far too slow for a page that lists
/// a dozen, so integrity here is reported as unverified rather than computed.
fn list_local(source: &Source, running: &str) -> Result<Vec<Candidate>, String> {
    let dir = std::path::Path::new(&source.location);
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", source.location));
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

fn resolve(
    source: &Source,
    running_hint: &str,
) -> (Vec<Candidate>, Option<String>, Option<String>) {
    match source.kind {
        SourceKind::Local => match list_local(source, running_hint) {
            Ok(c) => (c, None, None),
            Err(e) => (Vec::new(), Some(e), None),
        },
        SourceKind::Github | SourceKind::Http => match list_remote(source) {
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

struct Cached {
    at: Instant,
    value: Catalog,
    ok: bool,
}

fn cache() -> &'static Mutex<Option<Cached>> {
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Whether a fan-out is already running.
///
/// Without this, every press of "check now" and every page that opened while
/// one was in flight would start another, and four sources would become
/// twelve requests against a GitHub quota of sixty an hour.
fn refreshing() -> &'static AtomicBool {
    static REFRESHING: OnceLock<AtomicBool> = OnceLock::new();
    REFRESHING.get_or_init(|| AtomicBool::new(false))
}

/// Asks every enabled source what it offers, all at once.
///
/// One `spawn_blocking` per source rather than one for the lot. Each runs
/// `tpi-selfupdate --list`, which bounds itself with `curl --max-time 30`, so
/// the whole fan-out costs the slowest source rather than the sum of them.
/// Measured on the board before this change: four sources, 16 s; and asking
/// for one source cost the same 16 s, because the refresh was never per
/// source in the first place.
async fn fan_out() -> Catalog {
    let sources = crate::app::firmware_sources::load().await;
    // The api layer already reads /etc/os-release for this; a second reader
    // here could drift from the one the About page uses.
    let running_hint = crate::api::legacy::firmware_version()
        .await
        .unwrap_or_else(|| "unknown".to_string());

    let enabled: Vec<Source> = sources.sources.into_iter().filter(|s| s.enabled).collect();

    let mut tasks = tokio::task::JoinSet::new();
    for (index, source) in enabled.into_iter().enumerate() {
        let hint = running_hint.clone();
        tasks.spawn_blocking(move || {
            let (candidates, error, running) = resolve(&source, &hint);
            (index, source, candidates, error, running)
        });
    }

    let mut resolved = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(one) => resolved.push(one),
            // A panicking source must not take the others with it, and must
            // not be reported as "nothing new" either -- but there is no
            // source left to attach an error to, so say it in the log.
            Err(e) => tracing::error!("a firmware source panicked while listing: {e}"),
        }
    }
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
    let sources = resolved
        .into_iter()
        .map(|(_, source, candidates, error, _)| {
            if error.is_some() {
                ok = false;
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

    *cache().lock().await = Some(Cached {
        at: Instant::now(),
        value: value.clone(),
        ok,
    });
    value
}

/// Starts a fan-out behind the caller's back, unless one is already running.
///
/// The lock is taken only to store the result. Holding it across the fan-out
/// -- which is what this used to do -- meant a "check now" blocked every other
/// reader of the catalogue for as long as the slowest source took, so a page
/// that wanted nothing but the cached list froze too.
fn spawn_refresh() {
    if refreshing().swap(true, AtomicOrdering::AcqRel) {
        return;
    }
    tokio::spawn(async {
        fan_out().await;
        refreshing().store(false, AtomicOrdering::Release);
    });
}

/// Primes the catalogue at start-up, so the first page to ask has an answer.
///
/// Without it the first caller after a boot pays for the fan-out, which is
/// precisely the person watching a board come back from a firmware update.
pub fn prime() {
    spawn_refresh();
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
    let snapshot = {
        let guard = cache().lock().await;
        guard
            .as_ref()
            .map(|c| (c.value.clone(), c.at.elapsed(), c.ok))
    };

    let Some((mut value, age, ok)) = snapshot else {
        return fan_out().await;
    };

    let ttl = if ok { FRESH } else { FRESH_AFTER_ERROR };
    if force || age >= ttl {
        spawn_refresh();
    }

    value.age_seconds = age.as_secs();
    value.refreshing = refreshing().load(AtomicOrdering::Acquire);
    value
}

#[cfg(test)]
mod tests {
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
    #[test]
    fn only_one_refresh_runs_at_a_time() {
        let flag = AtomicBool::new(false);

        // What spawn_refresh does: claim the flag, and give up if it was
        // already claimed.
        let claim = || !flag.swap(true, AtomicOrdering::AcqRel);

        assert!(claim(), "the first caller starts the refresh");
        assert!(!claim(), "the second joins it rather than starting another");
        assert!(!claim(), "and so does the third");

        flag.store(false, AtomicOrdering::Release);
        assert!(
            claim(),
            "once it has finished, the next caller starts a new one"
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
