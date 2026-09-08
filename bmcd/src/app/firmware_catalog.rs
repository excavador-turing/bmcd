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
        let relation = if version == running {
            Relation::Current
        } else {
            // Deliberately not ordered. A file on a card carries no promise
            // that its name reflects its contents, and an unversioned build
            // has no place in an ordering at all.
            Relation::Unknown
        };
        candidates.push(Candidate {
            version,
            relation,
            prerelease: false,
            trust: Trust::Unverified,
            file: Some(path.to_string_lossy().to_string()),
            size_bytes: entry.metadata().ok().map(|m| m.len()),
        });
    }
    candidates.sort_by(|a, b| b.version.cmp(&a.version));
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

/// Every enabled source's offering, cached.
///
/// `force` skips the cache, which is what a "check now" control needs: a page
/// that can only tell you what it thought half an hour ago cannot be used to
/// confirm a release you just published.
pub async fn get(force: bool) -> Catalog {
    let mut guard = cache().lock().await;

    if !force {
        if let Some(cached) = guard.as_ref() {
            let ttl = if cached.ok { FRESH } else { FRESH_AFTER_ERROR };
            if cached.at.elapsed() < ttl {
                return cached.value.clone();
            }
        }
    }

    let sources = crate::app::firmware_sources::load().await;
    // The api layer already reads /etc/os-release for this; a second reader
    // here could drift from the one the About page uses.
    let running_hint = crate::api::legacy::firmware_version()
        .await
        .unwrap_or_else(|| "unknown".to_string());

    let enabled: Vec<Source> = sources.sources.into_iter().filter(|s| s.enabled).collect();

    let hint = running_hint.clone();
    let resolved = tokio::task::spawn_blocking(move || {
        enabled
            .into_iter()
            .map(|source| {
                let (candidates, error, running) = resolve(&source, &hint);
                (source, candidates, error, running)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();

    // The updater knows the running version too; prefer what it reported over
    // the local read, so both halves of the page agree.
    let running = resolved
        .iter()
        .find_map(|(_, _, _, r)| r.clone())
        .unwrap_or(running_hint);

    let mut ok = true;
    let sources = resolved
        .into_iter()
        .map(|(source, candidates, error, _)| {
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
    };

    *guard = Some(Cached {
        at: Instant::now(),
        value: value.clone(),
        ok,
    });
    value
}

#[cfg(test)]
mod tests {
    use super::*;

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
