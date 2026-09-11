// Copyright 2026 excavador-turing
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

//! What is on the microSD card, so the interface can offer it rather than
//! asking an operator to type a path (SQU-198).
//!
//! `tpi flash --local` already reads an image off this card, and that is the
//! only sane way to write a multi-gigabyte image to a compute module: the
//! browser is not in the path, the bytes do not cross the network twice, and
//! an interrupted upload does not mean starting the flash again. What was
//! missing is any way to see what is there.
//!
//! ## Every path is confined to the card
//!
//! This is the part to be careful about. A listing that accepts `../..` is a
//! directory browser rooted at `/` on a device that can reflash four
//! computers, and the same resolver will later serve rename and delete. So
//! resolution is one function, it canonicalises, and it refuses anything that
//! does not end up under the root -- symlinks included, which is why the
//! check is on the *canonical* path and not on the text of the request.

use serde::Serialize;
use std::{
    io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use schemars::JsonSchema;

/// Where the card is mounted. The board mounts `/dev/mmcblk0p1` here at boot.
pub const SDCARD_ROOT: &str = "/mnt/sdcard";

/// Anything smaller than this is not an OS image, whatever it is called. A
/// truncated download is the common case and the one worth naming: it looks
/// exactly like a valid choice in a list of filenames.
const SMALLEST_PLAUSIBLE_IMAGE: u64 = 16 * 1024 * 1024;

/// One entry in the listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Entry {
    /// Path relative to the card's root, which is also what a later request
    /// sends back. Never absolute: the root is the daemon's to decide.
    pub path: String,
    pub name: String,
    /// `true` for a directory, so a client can render a tree without a second
    /// request per row.
    pub directory: bool,
    /// Bytes. Meaningless for a directory and reported as 0 rather than
    /// omitted, because an absent field and a zero are the same thing to most
    /// clients and only one of them is true.
    pub size: u64,
    /// Seconds since the epoch, or `None` where the filesystem will not say.
    pub modified: Option<i64>,
    /// Whether this could be written to a compute module.
    pub flashable: bool,
    /// Why not, when it is not. Present ONLY alongside `flashable: false`.
    ///
    /// A non-candidate is listed rather than hidden: an operator who cannot
    /// find the file they just copied will not conclude "it must be too
    /// small", they will conclude the page is broken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What went wrong, in terms a client can act on.
#[derive(Debug, thiserror::Error)]
pub enum SdCardError {
    /// The one that matters. Deliberately says nothing about what IS there.
    #[error("path is outside the microSD card")]
    OutsideRoot,
    #[error("no such directory on the microSD card")]
    NotFound,
    #[error("not a directory")]
    NotADirectory,
    #[error("the microSD card is not mounted")]
    NotMounted,
    #[error("reading the microSD card: {0}")]
    Io(#[from] io::Error),
}

/// Resolve a client-supplied relative path against the card's root.
///
/// The whole security property of this module is here. Rules:
///
/// * the request is always relative; a leading `/` is stripped rather than
///   honoured, because an absolute path from a client means the client's idea
///   of the root, which is not this one
/// * the ROOT is canonicalised once, so a root that is itself reached through
///   a symlink still compares equal
/// * the target is canonicalised, so `a/../../etc` and a symlink to `/etc`
///   are both caught by the same check rather than by a rule about `..`
/// * a target that does not exist is `NotFound`, never "outside" -- saying
///   "outside" for a typo tells a client something about the filesystem it
///   did not earn
pub fn resolve(root: &Path, relative: &str) -> Result<PathBuf, SdCardError> {
    let root = root.canonicalize().map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => SdCardError::NotMounted,
        _ => SdCardError::Io(err),
    })?;

    let trimmed = relative.trim_start_matches('/');
    if trimmed.is_empty() {
        return Ok(root);
    }

    let target = root.join(trimmed);
    let target = target.canonicalize().map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => SdCardError::NotFound,
        _ => SdCardError::Io(err),
    })?;

    if !target.starts_with(&root) {
        return Err(SdCardError::OutsideRoot);
    }

    Ok(target)
}

/// List one directory of the card, sorted directories first then by name.
///
/// Not recursive. A card holding a few multi-gigabyte images has a handful of
/// entries, and walking a tree the operator has not asked to see costs reads
/// on a board with 116 MB of RAM.
pub async fn list(root: &Path, relative: &str) -> Result<Vec<Entry>, SdCardError> {
    let dir = resolve(root, relative)?;
    let canonical_root = root.canonicalize()?;

    if !dir.is_dir() {
        return Err(SdCardError::NotADirectory);
    }

    let mut entries = Vec::new();
    let mut read = tokio::fs::read_dir(&dir).await?;

    while let Some(entry) = read.next_entry().await? {
        let Ok(name) = entry.file_name().into_string() else {
            // A name that is not UTF-8 cannot be sent back and then returned
            // to us intact, so it is skipped rather than mangled.
            continue;
        };

        // `lost+found` is the filesystem's, not the operator's, and it is
        // root-only anyway.
        if name == "lost+found" {
            continue;
        }

        let Ok(meta) = entry.metadata().await else {
            continue;
        };

        let directory = meta.is_dir();
        let size = if directory { 0 } else { meta.len() };
        let modified = meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_secs() as i64);

        let path = entry
            .path()
            .strip_prefix(&canonical_root)
            .map(|rest| rest.to_string_lossy().into_owned())
            .unwrap_or_else(|_| name.clone());

        let (flashable, reason) = judge(&name, directory, size);

        entries.push(Entry {
            path,
            name,
            directory,
            size,
            modified,
            flashable,
            reason,
        });
    }

    entries.sort_by(|left, right| {
        right
            .directory
            .cmp(&left.directory)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });

    Ok(entries)
}

/// Could this be written to a compute module, and if not, why not.
///
/// Extension and size only. Nothing here opens the file: deciding by content
/// would mean reading the head of every entry on every listing, and the thing
/// that actually protects a flash is the checksum the operator compares, not
/// a guess made from a magic number.
fn judge(name: &str, directory: bool, size: u64) -> (bool, Option<String>) {
    if directory {
        return (false, Some("a directory".to_string()));
    }

    let lower = name.to_lowercase();
    let looks_like_an_image = [".img", ".raw", ".bin", ".img.xz", ".raw.xz", ".img.gz"]
        .iter()
        .any(|suffix| lower.ends_with(suffix));

    if !looks_like_an_image {
        // Firmware images live here too and are flashed by a different
        // command entirely; saying so is more useful than "not an image".
        if lower.ends_with(".tpu") {
            return (
                false,
                Some("BMC firmware, not a node image — use the firmware page".to_string()),
            );
        }
        return (false, Some("not an OS image".to_string()));
    }

    if size < SMALLEST_PLAUSIBLE_IMAGE {
        return (
            false,
            Some(format!(
                "only {} bytes — too small to be an OS image, most likely a truncated download",
                size
            )),
        );
    }

    (true, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn card() -> tempdir::TempDir {
        let dir = tempdir::TempDir::new("sdcard").expect("temp dir");
        fs::create_dir_all(dir.path().join("images")).unwrap();
        fs::write(dir.path().join("talos.img"), vec![0u8; 20 * 1024 * 1024]).unwrap();
        fs::write(dir.path().join("truncated.img"), b"not really").unwrap();
        fs::write(dir.path().join("notes.txt"), b"hello").unwrap();
        fs::write(dir.path().join("fw-v2.24.0.tpu"), vec![0u8; 1024]).unwrap();
        fs::create_dir_all(dir.path().join("lost+found")).unwrap();
        dir
    }

    /// The one that matters. Every shape of escape, caught by the same rule.
    #[test]
    fn nothing_outside_the_card_can_be_named() {
        let dir = card();
        let root = dir.path();

        for attempt in [
            "..",
            "../..",
            "images/../..",
            "/etc",
            "images/../../../../etc",
        ] {
            match resolve(root, attempt) {
                Err(SdCardError::OutsideRoot) | Err(SdCardError::NotFound) => {}
                other => panic!("{attempt:?} was not refused: {other:?}"),
            }
        }
    }

    /// A symlink out is the same escape wearing a different hat, and a rule
    /// written against the TEXT of the request would miss it entirely.
    #[cfg(unix)]
    #[test]
    fn a_symlink_off_the_card_is_refused() {
        let dir = card();
        std::os::unix::fs::symlink("/etc", dir.path().join("escape")).unwrap();

        assert!(matches!(
            resolve(dir.path(), "escape"),
            Err(SdCardError::OutsideRoot)
        ));
    }

    /// The root itself, and a real directory under it, both resolve.
    #[test]
    fn the_card_and_its_own_directories_resolve() {
        let dir = card();
        let root = dir.path().canonicalize().unwrap();

        assert_eq!(resolve(dir.path(), "").unwrap(), root);
        assert_eq!(resolve(dir.path(), "/").unwrap(), root);
        assert_eq!(resolve(dir.path(), "images").unwrap(), root.join("images"));
        // A leading slash is stripped, not honoured: "/images" is the card's
        // images directory, not the machine's.
        assert_eq!(resolve(dir.path(), "/images").unwrap(), root.join("images"));
    }

    #[tokio::test]
    async fn a_listing_says_what_can_be_flashed_and_why_not() {
        let dir = card();
        let entries = list(dir.path(), "").await.expect("a listing");

        let by_name = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("{name} missing from {entries:?}"))
                .clone()
        };

        // Directories first, then alphabetical.
        assert!(entries[0].directory, "directories sort first");

        let image = by_name("talos.img");
        assert!(image.flashable);
        assert_eq!(image.reason, None, "a candidate carries no excuse");
        assert_eq!(image.size, 20 * 1024 * 1024);
        assert!(image.modified.is_some());

        // Listed, not hidden -- an operator who cannot see the file they just
        // copied concludes the page is broken, not that it was too small.
        let stub = by_name("truncated.img");
        assert!(!stub.flashable);
        assert!(
            stub.reason.as_deref().unwrap_or_default().contains("small"),
            "{:?}",
            stub.reason
        );

        let firmware = by_name("fw-v2.24.0.tpu");
        assert!(!firmware.flashable);
        assert!(firmware
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("firmware"));

        assert!(!by_name("notes.txt").flashable);
        assert!(by_name("images").directory);

        // The filesystem's bookkeeping is not the operator's business.
        assert!(
            !entries.iter().any(|entry| entry.name == "lost+found"),
            "lost+found should not be offered"
        );
    }

    #[tokio::test]
    async fn listing_a_file_is_not_a_listing() {
        let dir = card();
        assert!(matches!(
            list(dir.path(), "talos.img").await,
            Err(SdCardError::NotADirectory)
        ));
    }

    #[tokio::test]
    async fn an_unmounted_card_says_so() {
        assert!(matches!(
            list(Path::new("/nonexistent-mount-point"), "").await,
            Err(SdCardError::NotMounted)
        ));
    }
}
