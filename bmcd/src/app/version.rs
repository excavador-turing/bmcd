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
//! Ordering release tags.
//!
//! Two rules matter here and both have already been got wrong somewhere in
//! this fork.
//!
//! **Compare numbers as numbers.** As text `"2.10.0" < "2.9.0"`, so a lexical
//! compare tells a board on 2.10.0 that 2.9.0 is newer. The bug is invisible
//! until the tenth minor release and then it is everywhere at once.
//!
//! **A prerelease comes before its own release.** `v2.8.1-rc1` is newer than
//! `v2.8.0` and older than `v2.8.1`. GNU `sort -V`, which the updater script
//! used, gets the first right and the second **wrong**: it sorts
//! `v2.8.1-rc1` after `v2.8.1`, so a board on the finished release would be
//! offered the candidate as an upgrade.
//!
//! Anything that is not version-shaped compares to nothing. A build made by
//! `just build` reports `VERSION=local`, and an ordering that accepts it puts
//! it first, which once made every release on the list look like an upgrade.
//! `None` is the honest answer and callers render it as "unknown" rather than
//! inventing a direction.

use std::cmp::Ordering;

/// A release tag that can be ordered: a numeric release, and an optional
/// prerelease suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// `v2.8.1` -> `[2, 8, 1]`. Compared element by element, shorter padded
    /// with zeroes, so `v2.8` and `v2.8.0` are the same release.
    release: Vec<u64>,
    /// Everything after the first `-`, split on `.`. `None` for a final
    /// release, which is what makes it sort after its own candidates.
    pre: Option<Vec<String>>,
}

/// Parses a tag, or returns `None` when it is not version-shaped.
///
/// Accepts an optional leading `v`, then dot-separated integers, then
/// optionally `-` and anything. Rejects an empty numeric part, a component
/// that is not an integer, and anything with build metadata we cannot order
/// (`+`), because guessing is worse than declining.
pub fn parse(tag: &str) -> Option<Version> {
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    if tag.is_empty() || tag.contains('+') {
        return None;
    }

    let (numeric, pre) = match tag.split_once('-') {
        Some((numeric, pre)) if !pre.is_empty() => (numeric, Some(pre)),
        Some(_) => return None, // a trailing `-` with nothing after it
        None => (tag, None),
    };

    let release: Option<Vec<u64>> = numeric.split('.').map(|p| p.parse::<u64>().ok()).collect();
    let release = release?;
    if release.is_empty() {
        return None;
    }

    Some(Version {
        release,
        pre: pre.map(|p| p.split('.').map(str::to_string).collect()),
    })
}

impl Version {
    /// Whether this tag names a candidate for a release rather than the
    /// release itself.
    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some()
    }
}

/// Whether a tag names a prerelease. `false` for anything unorderable: a
/// hand-built image is not a release candidate, it is simply not a release.
pub fn is_prerelease(tag: &str) -> bool {
    parse(tag).is_some_and(|v| v.is_prerelease())
}

/// Orders two tags, or returns `None` when either is not version-shaped.
///
/// `Some(Greater)` means the first tag is the newer one.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    Some(order(&parse(a)?, &parse(b)?))
}

fn order(a: &Version, b: &Version) -> Ordering {
    let width = a.release.len().max(b.release.len());
    for i in 0..width {
        let left = a.release.get(i).copied().unwrap_or(0);
        let right = b.release.get(i).copied().unwrap_or(0);
        match left.cmp(&right) {
            Ordering::Equal => {}
            other => return other,
        }
    }

    match (&a.pre, &b.pre) {
        // Same release, and neither is a candidate for it.
        (None, None) => Ordering::Equal,
        // A finished release is newer than any candidate for itself. This is
        // the case `sort -V` reverses.
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => order_pre(left, right),
    }
}

/// Compares prerelease identifiers the way semver does: numeric ones
/// numerically, so `rc.9` precedes `rc.10`; a numeric identifier ranks below
/// an alphanumeric one; and a shorter run of identifiers precedes a longer one
/// that starts the same way, so `rc1` precedes `rc1.2`.
fn order_pre(a: &[String], b: &[String]) -> Ordering {
    for (left, right) in a.iter().zip(b.iter()) {
        let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
            (Ok(l), Ok(r)) => l.cmp(&r),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_is_after_nine() {
        // The reason this module exists rather than a string compare.
        assert_eq!(compare("v2.10.0", "v2.9.0"), Some(Ordering::Greater));
        assert_eq!(compare("v2.9.0", "v2.10.0"), Some(Ordering::Less));
    }

    #[test]
    fn a_candidate_sits_between_the_two_releases() {
        // Seen on the board 2026-09-09: v2.8.1-rc1 parked on the SD card while
        // v2.8.0 was running. It is an upgrade from v2.8.0 and a downgrade
        // from v2.8.1. `sort -V` agrees with the first and reverses the second.
        assert_eq!(compare("v2.8.1-rc1", "v2.8.0"), Some(Ordering::Greater));
        assert_eq!(compare("v2.8.1-rc1", "v2.8.1"), Some(Ordering::Less));
        assert_eq!(compare("v2.8.1", "v2.8.1-rc1"), Some(Ordering::Greater));
    }

    #[test]
    fn candidates_order_among_themselves() {
        assert_eq!(compare("v2.8.1-rc2", "v2.8.1-rc1"), Some(Ordering::Greater));
        assert_eq!(
            compare("v1.0.0-rc.10", "v1.0.0-rc.9"),
            Some(Ordering::Greater)
        );
        assert_eq!(compare("v1.0.0-rc1", "v1.0.0-rc1.2"), Some(Ordering::Less));
        assert_eq!(compare("v1.0.0-alpha", "v1.0.0-beta"), Some(Ordering::Less));
        // A numeric identifier ranks below an alphanumeric one.
        assert_eq!(compare("v1.0.0-1", "v1.0.0-alpha"), Some(Ordering::Less));
    }

    #[test]
    fn a_missing_component_is_zero() {
        assert_eq!(compare("v2.8", "v2.8.0"), Some(Ordering::Equal));
        assert_eq!(compare("v2.8.1", "v2.8"), Some(Ordering::Greater));
    }

    #[test]
    fn the_leading_v_is_optional_and_not_required_to_match() {
        assert_eq!(compare("2.8.1", "v2.8.0"), Some(Ordering::Greater));
        assert_eq!(compare("v2.8.1", "2.8.1"), Some(Ordering::Equal));
    }

    #[test]
    fn an_unversioned_build_orders_against_nothing() {
        // `just build` stamps VERSION=local. Ordering it against real tags is
        // what once offered v2.3.0 as an upgrade to a board running newer code.
        assert_eq!(compare("local", "v2.8.0"), None);
        assert_eq!(compare("v2.8.0", "local"), None);
        assert_eq!(compare("", "v2.8.0"), None);
        assert_eq!(compare("v", "v2.8.0"), None);
        assert_eq!(compare("v2.8.x", "v2.8.0"), None);
        assert_eq!(compare("vtwo.eight", "v2.8.0"), None);
        assert_eq!(compare("v2.8.0-", "v2.8.0"), None);
        assert_eq!(compare("v2.8.0+build7", "v2.8.0"), None);
        assert!(parse("local").is_none());
        assert!(parse("v2.8.1-rc1").is_some());
        // Unorderable is not the same as prerelease.
        assert!(!is_prerelease("local"));
        assert!(!is_prerelease("v2.8.1"));
        assert!(is_prerelease("v2.8.1-rc1"));
    }

    #[test]
    fn the_forks_own_unstable_tags_sort_below_their_release() {
        // v2.2.0-unstable-hive.12 is work towards v2.2.0, not after it.
        assert_eq!(
            compare("v2.2.0-unstable-hive.12", "v2.2.0"),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare("v2.2.0-unstable-hive.12", "v2.1.0"),
            Some(Ordering::Greater)
        );
    }
}
