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
//! Where this board looks for firmware.
//!
//! ## Why the daemon owns this file and the shell script does not
//!
//! `tpi-selfupdate` resolves releases -- channels, version ordering,
//! `SHA256SUMS`, the HTTP directory layout -- and it should keep doing that,
//! because a second implementation would be free to disagree with the updater
//! that actually performs the upgrade. But it cannot read this file: the board
//! has no `jq` and no `python3`, only `awk` and `sed`.
//!
//! So the split is: this decides WHAT to look at, the script decides HOW. The
//! daemon reads the JSON and passes `--repo` or `--url` as flags.
//!
//! ## Where it lives
//!
//! `/mnt/overlay/firmware-sources.json`. Both firmware images mount the
//! overlay, so the source list survives an A/B promotion -- a list on the
//! rootfs would be lost by the very upgrade it configured.
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

pub const SOURCES_PATH: &str = "/mnt/overlay/firmware-sources.json";

/// Where a source's images come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// A GitHub repository, `owner/repo`. Checksums are published and checked.
    Github,
    /// An HTTP directory of `<prefix>/<version>/<image>.tpu`. Checksums are
    /// used when the publisher ships them; upstream's does not.
    Http,
    /// A directory on the SD card. This is also where uploads land, so an
    /// uploaded image becomes a candidate like any other.
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// Stable identifier, used by the install call. Not the label: renaming a
    /// source in the interface must not change what an install refers to.
    pub id: String,
    pub kind: SourceKind,
    /// What a human calls it.
    pub label: String,
    /// `owner/repo` for github, a URL prefix for http, a path for local.
    pub location: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sources {
    pub sources: Vec<Source>,
}

/// What a board carries when nobody has configured anything.
///
/// This fork's releases, the SD card, and BOTH of Turing Pi's own channels.
///
/// Upstream was omitted from an earlier draft on the grounds that it stops at
/// v2.0.5 and its HTTP server publishes no checksums, so following it would
/// walk a board backwards onto unverifiable images. Including it is the better
/// call, and the reason is worth stating: those risks are handled where they
/// belong rather than by pretending the sources do not exist. An older release
/// is reported as `older` and hidden until asked for; an image with no
/// published checksum is labelled `tls` rather than `verified`; and installing
/// either takes a confirmation that says which it is. Omission would have
/// hidden the option without removing the hazard, and left an operator who
/// wanted a stock image to type the location from memory.
///
/// Both of Turing Pi's channels are listed because THEY DISAGREE: as of
/// 2026-09-08 the GitHub releases reach v2.1.0 while firmware.turingpi.com
/// stops at v2.0.5. A source is a claim about what exists, and one publisher
/// can make two different ones.
/// Where this fork releases, and how the entry is labelled.
pub const FORK_LOCATION: &str = "excavador-turing/BMC-Firmware";
pub const FORK_LABEL: &str = "excavador-turing (this fork)";

/// Locations that have MOVED, and where they moved to.
///
/// The fork left a personal account for its own organization on 2026-09-08.
/// A board that had already written firmware-sources.json keeps whatever it
/// stored, and nothing about a stale entry LOOKS broken: the archived
/// repository still answers and still lists the releases it had. It simply
/// never sees another one. A silent stop is exactly the failure a board
/// owner would not notice, so the rewrite happens on read rather than
/// waiting for a firmware nobody can discover to carry it.
const RETIRED_LOCATIONS: &[(&str, &str)] = &[("excavador/tp2-bmc-firmware", FORK_LOCATION)];

/// The label that shipped beside the retired location.
const RETIRED_FORK_LABEL: &str = "excavador (this fork)";

/// Rewrites retired locations in place; returns whether anything changed.
///
/// A label is only rewritten when its own source's location was rewritten
/// AND the label is still the one that shipped -- a label the operator
/// edited is theirs to keep.
fn migrate(sources: &mut Sources) -> bool {
    let mut changed = false;

    for s in &mut sources.sources {
        let Some((_, to)) = RETIRED_LOCATIONS
            .iter()
            .find(|(from, _)| s.location == *from)
        else {
            continue;
        };

        s.location = (*to).to_string();
        if s.label == RETIRED_FORK_LABEL {
            s.label = FORK_LABEL.to_string();
        }
        changed = true;
    }

    changed
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            sources: vec![
                Source {
                    id: "fork".to_string(),
                    kind: SourceKind::Github,
                    label: FORK_LABEL.to_string(),
                    location: FORK_LOCATION.to_string(),
                    enabled: true,
                },
                Source {
                    id: "local".to_string(),
                    kind: SourceKind::Local,
                    label: "SD card".to_string(),
                    location: "/mnt/sdcard/firmware".to_string(),
                    enabled: true,
                },
                Source {
                    id: "turingpi".to_string(),
                    kind: SourceKind::Github,
                    label: "Turing Pi (official releases)".to_string(),
                    location: "turing-machines/BMC-Firmware".to_string(),
                    enabled: true,
                },
                Source {
                    id: "turingpi-http".to_string(),
                    kind: SourceKind::Http,
                    label: "Turing Pi (firmware.turingpi.com)".to_string(),
                    location: "https://firmware.turingpi.com/turing-pi2".to_string(),
                    enabled: true,
                },
            ],
        }
    }
}

/// Reads the configured sources, falling back to the default set.
///
/// A missing file is the normal state on a board nobody has configured, and a
/// CORRUPT file is treated the same way rather than failing the page: the
/// firmware screen must still render, and a board that cannot show its
/// sources is worse than one showing the defaults.
pub async fn load() -> Sources {
    let mut sources = match tokio::fs::read_to_string(SOURCES_PATH).await {
        Ok(body) => serde_json::from_str(&body).unwrap_or_else(|e| {
            tracing::warn!("{SOURCES_PATH} is not readable as sources ({e}); using defaults");
            Sources::default()
        }),
        Err(_) => Sources::default(),
    };

    // Persisted state outlives the firmware that wrote it, so a moved
    // location is corrected on the way out rather than only in the default
    // set -- a board that has ever opened this page has a file, and the
    // default set is not what it reads.
    if migrate(&mut sources) {
        tracing::info!("firmware sources: a retired location was rewritten");
        if let Err(e) = store(&sources).await {
            // Read-only overlay, full disk: the rewrite still applies to
            // THIS read, so the board keeps working; it just re-migrates
            // next time.
            tracing::warn!("could not persist the migrated sources ({e})");
        }
    }

    sources
}

/// Replaces the configured sources.
pub async fn store(sources: &Sources) -> io::Result<()> {
    let body = serde_json::to_string_pretty(sources)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    tokio::fs::write(SOURCES_PATH, body).await
}

/// Whether the overlay is mounted, so a write can survive a reboot.
pub fn storage_available() -> bool {
    Path::new(SOURCES_PATH)
        .parent()
        .map(|p| p.is_dir())
        .unwrap_or(false)
}

/// Rejects a source list that cannot be acted on later.
///
/// Checked here rather than at install time, because a list that is saved and
/// then fails on use is far harder to understand than one refused as it is
/// written.
pub fn validate(sources: &Sources) -> Result<(), String> {
    let mut seen = Vec::new();
    for source in &sources.sources {
        if source.id.trim().is_empty() {
            return Err("a source has an empty id".to_string());
        }
        if seen.contains(&source.id) {
            return Err(format!("duplicate source id {:?}", source.id));
        }
        seen.push(source.id.clone());

        if source.location.trim().is_empty() {
            return Err(format!("source {:?} has an empty location", source.id));
        }
        match source.kind {
            // owner/repo, and nothing else -- a URL here would be silently
            // pasted into an api.github.com path and 404 at list time.
            SourceKind::Github => {
                let parts: Vec<&str> = source.location.split('/').collect();
                if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
                    return Err(format!(
                        "source {:?} should be owner/repo, not {:?}",
                        source.id, source.location
                    ));
                }
            }
            // A directory prefix. Pointing this at a .tpu is the mistake this
            // catches: it would list nothing, and an empty list is
            // indistinguishable from a source with no new versions.
            SourceKind::Http => {
                if !source.location.starts_with("http") {
                    return Err(format!("source {:?} should be an http(s) URL", source.id));
                }
                if source.location.ends_with(".tpu") {
                    return Err(format!(
                        "source {:?} should be the DIRECTORY holding version folders, not an image",
                        source.id
                    ));
                }
            }
            SourceKind::Local => {
                if !source.location.starts_with('/') {
                    return Err(format!("source {:?} should be an absolute path", source.id));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(id: &str, kind: SourceKind, location: &str) -> Source {
        Source {
            id: id.to_string(),
            kind,
            label: id.to_string(),
            location: location.to_string(),
            enabled: true,
        }
    }

    #[test]
    fn the_default_set_is_valid() {
        assert!(validate(&Sources::default()).is_ok());
    }

    /// Upstream ships by two routes that disagree with each other, and both
    /// are offered. This asserts the default set rather than a count, because
    /// the interesting property is WHICH sources are there.
    #[test]
    fn both_of_turing_pis_channels_are_offered() {
        let d = Sources::default();
        assert!(d
            .sources
            .iter()
            .any(|s| s.location == "turing-machines/BMC-Firmware"));
        assert!(d
            .sources
            .iter()
            .any(|s| s.location == "https://firmware.turingpi.com/turing-pi2"));
    }

    /// The HTTP source must be the DIRECTORY of version folders. Pointing it
    /// at an image lists nothing, and nothing looks exactly like a source with
    /// no new versions -- so the default must not itself be the mistake the
    /// validator exists to catch.
    #[test]
    fn the_shipped_http_source_is_a_directory_not_an_image() {
        let d = Sources::default();
        let http = d
            .sources
            .iter()
            .find(|s| matches!(s.kind, SourceKind::Http))
            .expect("an http source ships by default");
        assert!(!http.location.ends_with(".tpu"));
        assert!(validate(&d).is_ok());
    }

    #[test]
    fn a_github_source_must_be_owner_slash_repo() {
        let bad = Sources {
            sources: vec![src(
                "x",
                SourceKind::Github,
                "https://github.com/excavador-turing/BMC-Firmware",
            )],
        };
        assert!(validate(&bad).is_err());
    }

    /// The mistake worth catching: an http source pointed at an image rather
    /// than the directory lists nothing, and nothing is indistinguishable from
    /// "no new versions".
    #[test]
    fn an_http_source_pointed_at_an_image_is_refused() {
        let bad = Sources {
            sources: vec![src(
                "x",
                SourceKind::Http,
                "https://firmware.turingpi.com/turing-pi2/v2.0.5/tp2-ota-v2.0.5.tpu",
            )],
        };
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let bad = Sources {
            sources: vec![
                src("dup", SourceKind::Github, "a/b"),
                src("dup", SourceKind::Github, "c/d"),
            ],
        };
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn a_source_list_round_trips_through_json() {
        let d = Sources::default();
        let text = serde_json::to_string(&d).expect("serialises");
        let back: Sources = serde_json::from_str(&text).expect("parses");
        assert_eq!(back, d);
    }

    /// The whole point of the move: a board that already has a file keeps
    /// pointing at the retired repository, which still answers and still
    /// lists what it had. Nothing looks wrong; it just never sees another
    /// release.
    #[test]
    fn a_retired_location_is_rewritten() {
        let mut stored = Sources {
            sources: vec![Source {
                id: "fork".to_string(),
                kind: SourceKind::Github,
                label: "excavador (this fork)".to_string(),
                location: "excavador/tp2-bmc-firmware".to_string(),
                enabled: true,
            }],
        };

        assert!(
            migrate(&mut stored),
            "the retired location must be recognised"
        );
        assert_eq!(stored.sources[0].location, FORK_LOCATION);
        assert_eq!(stored.sources[0].label, FORK_LABEL);
    }

    /// A label the operator chose is theirs. Only one still at its shipped
    /// default is rewritten alongside the location.
    #[test]
    fn an_edited_label_survives_the_migration() {
        let mut stored = Sources {
            sources: vec![Source {
                id: "fork".to_string(),
                kind: SourceKind::Github,
                label: "my board".to_string(),
                location: "excavador/tp2-bmc-firmware".to_string(),
                enabled: true,
            }],
        };

        assert!(migrate(&mut stored));
        assert_eq!(stored.sources[0].location, FORK_LOCATION);
        assert_eq!(stored.sources[0].label, "my board");
    }

    /// load() persists whenever migrate() reports a change, so a migration
    /// that kept reporting one would rewrite the file on every read.
    #[test]
    fn the_default_set_needs_no_migration() {
        let mut d = Sources::default();
        assert!(
            !migrate(&mut d),
            "the shipped default must already be current"
        );

        let mut once = Sources {
            sources: vec![src(
                "fork",
                SourceKind::Github,
                "excavador/tp2-bmc-firmware",
            )],
        };
        assert!(migrate(&mut once));
        assert!(!migrate(&mut once), "migration must be idempotent");
    }

    /// A default still naming the retired repository would ship the very
    /// state the migration exists to undo.
    #[test]
    fn the_default_no_longer_names_the_retired_location() {
        let d = Sources::default();
        for (retired, _) in RETIRED_LOCATIONS {
            assert!(
                !d.sources.iter().any(|s| s.location == *retired),
                "default set still ships retired location {retired}"
            );
        }
        assert!(d.sources.iter().any(|s| s.location == FORK_LOCATION));
    }
}
