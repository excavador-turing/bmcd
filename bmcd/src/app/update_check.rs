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
//! Whether a newer firmware release exists, for the web interface to show.
//!
//! ## Why this shells out instead of calling GitHub
//!
//! `tpi-selfupdate` already resolves a channel to a release: it knows that
//! every pre-release must be excluded from `stable`, that `edge` sorts by
//! version rather than trusting the order GitHub returns, and what counts as
//! newer. That logic is proven on hardware. Reimplementing it here would mean
//! a second answer to the same question, free to disagree with the one that
//! actually performs the update -- and the interface would then be able to
//! promise an upgrade the updater refuses.
//!
//! So the script grew a `--json` mode and this reports what it says.
//!
//! ## Why it is cached
//!
//! Unauthenticated GitHub allows 60 requests an hour per address, and every
//! browser with the firmware page open would otherwise spend them. The board
//! also has 116 MB of RAM and one slow core, so spawning two processes per
//! page render is not free either.
use serde::Serialize;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// The updater, which owns release resolution.
const SELFUPDATE: &str = "/sbin/tpi-selfupdate";

/// How long a successful answer stays good. Releases are cut by hand; an
/// hour-old answer has never been wrong in a way that mattered.
const FRESH: Duration = Duration::from_secs(3600);

/// How long a FAILED answer is kept. Short, because the usual cause is a
/// board with no route out and that can be fixed at any moment -- but not
/// zero, or a board without a route would spawn two processes per render.
const FRESH_AFTER_ERROR: Duration = Duration::from_secs(300);

/// What one channel resolves to, straight from `tpi-selfupdate --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ChannelState {
    pub channel: String,
    pub repo: String,
    pub running: String,
    pub target: String,
    pub update_available: bool,
}

/// The answer the API returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateCheck {
    /// When this was last resolved, in the board's own idea of the time.
    pub checked_at: String,
    /// The stable channel: what GitHub marks Latest, pre-releases excluded.
    pub stable: Option<ChannelState>,
    /// The edge channel: the newest release including pre-releases.
    pub edge: Option<ChannelState>,
    /// Why a channel is missing, when one is. Present WITH a channel too,
    /// when only one of the two could be resolved.
    pub error: Option<String>,
}

struct Cached {
    at: Instant,
    value: UpdateCheck,
    ok: bool,
}

fn cache() -> &'static Mutex<Option<Cached>> {
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Runs the updater for one channel and parses its object.
///
/// Blocking on purpose, called from `spawn_blocking`. The script bounds its
/// own network calls with `curl --max-time`, so this cannot hang for longer
/// than the updater itself would.
fn resolve(channel: &str) -> Result<ChannelState, String> {
    let output = Command::new(SELFUPDATE)
        .args(["--check", "--json", "--channel", channel])
        .output()
        .map_err(|e| format!("cannot run {SELFUPDATE}: {e}"))?;

    // stdout carries the object and nothing else; every log line the script
    // writes goes to stderr. A non-zero exit with parseable stdout is still
    // an answer, so stdout is tried before the status is judged.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().find(|l| l.trim_start().starts_with('{'));

    match line {
        Some(line) => serde_json::from_str::<ChannelState>(line)
            .map_err(|e| format!("{channel}: cannot parse the updater's output: {e}")),
        None => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.lines().last().unwrap_or("no output").trim();
            Err(format!("{channel}: {detail}"))
        }
    }
}

/// Resolves both channels, using the cached answer when it is still fresh.
pub async fn get() -> UpdateCheck {
    let mut guard = cache().lock().await;

    if let Some(cached) = guard.as_ref() {
        let ttl = if cached.ok { FRESH } else { FRESH_AFTER_ERROR };
        if cached.at.elapsed() < ttl {
            return cached.value.clone();
        }
    }

    let (stable, edge) = tokio::task::spawn_blocking(|| (resolve("stable"), resolve("edge")))
        .await
        .unwrap_or_else(|e| {
            let msg = format!("update check task failed: {e}");
            (Err(msg.clone()), Err(msg))
        });

    let mut errors = Vec::new();
    if let Err(e) = &stable {
        errors.push(e.clone());
    }
    if let Err(e) = &edge {
        errors.push(e.clone());
    }

    let value = UpdateCheck {
        checked_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        stable: stable.ok(),
        edge: edge.ok(),
        error: (!errors.is_empty()).then(|| errors.join("; ")),
    };

    let ok = value.error.is_none();
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

    /// The shape the updater emits must deserialise exactly. This is the
    /// contract between two repositories, so it is asserted against a literal
    /// rather than against something this crate produced.
    #[test]
    fn the_updaters_object_deserialises() {
        let line = r#"{"channel":"stable","repo":"excavador/tp2-bmc-firmware","running":"v2.4.0","target":"v2.5.0","update_available":true}"#;
        let s: ChannelState = serde_json::from_str(line).expect("parses");
        assert_eq!(s.channel, "stable");
        assert_eq!(s.running, "v2.4.0");
        assert_eq!(s.target, "v2.5.0");
        assert!(s.update_available);
    }

    #[test]
    fn a_board_already_current_parses_too() {
        let line = r#"{"channel":"edge","repo":"r","running":"v2.4.0","target":"v2.4.0","update_available":false}"#;
        let s: ChannelState = serde_json::from_str(line).expect("parses");
        assert!(!s.update_available);
    }

    /// Anything that is not the contract must be an error, not a silent
    /// default -- a missing field defaulting to false would report "no update
    /// available" for a board that had never been checked.
    #[test]
    fn a_truncated_object_is_an_error() {
        assert!(serde_json::from_str::<ChannelState>(r#"{"channel":"stable"}"#).is_err());
        assert!(serde_json::from_str::<ChannelState>("").is_err());
    }
}
