# Changelog

The BMC daemon, as built by this fork. Upstream's own history is in the git
log; this file starts where the fork diverges, at 2.3.7.

The daemon ships to a board inside a firmware release, pinned by commit, so a
version here only reaches hardware once `BMC-Firmware` bumps that pin.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [2.10.1] — 2026-09-09

### Fixed

- **A parked image is ordered against what is running** (SQU-169). A local
  candidate was reported with an `unknown` relation whatever its name, so a
  `v2.8.1-rc1` on the SD card while the board ran v2.8.0 showed as `?`, sorted
  under "older or unrelated", and `tpi firmware check` never mentioned it.
  Found while flashing the gate test image.

  The old comment argued that a file on a card carries no promise that its name
  reflects its contents. True — and that is what the `trust` column says, which
  already reads `unverified` for every local file. Declining to order as well
  told the user the same thing twice and hid a real upgrade. Version-shaped
  names are now ordered; anything else still declines.
- Local candidates are listed newest first by that same ordering. A lexical
  sort put `v2.9.0` above `v2.10.0` in a list whose own `relation` fields said
  the opposite.
- `prerelease` is set from the tag rather than hard-coded to `false`, so a
  candidate can be labelled as one.

### Added

- `app::version`, an ordering for release tags, with the two rules this fork
  has already got wrong somewhere: numbers compare as numbers (`2.10.0` is
  after `2.9.0`, which a text compare reverses), and a prerelease comes before
  its own release (`v2.8.1-rc1` is after `v2.8.0` and before `v2.8.1`).
  Anything not version-shaped compares to nothing at all, because a build
  stamped `VERSION=local` once sorted first and made every release on the list
  look like an upgrade.

## [2.10.0] — 2026-09-08

### Changed

- Rust 1.98.1 across the workspace, dependencies refreshed, and 23 clippy lints
  fixed rather than suppressed. `NodeType`, `DummyValidator` and
  `FalseValidator` deleted as unused; an `unreachable!()` in `utils` replaced by
  `as_chunks::<2>()`, which makes the case impossible instead of asserting it.

## [2.9.0] — 2026-09-08

### Changed

- Firmware sources point at `excavador-turing/BMC-Firmware`. A board that still
  holds a retired location is migrated on load and the result persisted, so the
  move needs no action on the board.

## [2.8.0] — 2026-09-08

### Added

- Firmware sources are configurable and catalogued: GitHub releases, an HTTP
  directory, or the SD card, each candidate reported with how it compares to
  the running version and how much is known about its integrity.
- `firmware_install`, so a chosen version can be staged.

## [2.7.0] — 2026-09-07

### Added

- `/metrics` authenticates with its own token rather than the root password, so
  a scrape credential cannot touch `/api/bmc`.
- The daemon reports whether a newer firmware release exists.

### Changed

- Every switch metric series carries `kind`, not only the ones that are present.

## [2.5.0] — 2026-09-07

### Added

- `firmware_info` reports *which* image is staged, not merely that one is.
- The kernel release is reported on the About page.
- A release is published when a version tag is pushed.

## [2.4.0] — 2026-09-06

### Added

- A Prometheus scrape endpoint, behind authentication.
- The SoC temperature and the cooling state, the fan's real duty from the
  device tree, the running firmware slot and what a rollback lands on, and the
  condition of the BMC itself.
- A browser can authenticate the serial websocket.

### Fixed

- `/info` is no longer served unauthenticated over plain HTTP (SQU-125).
- A ban answers with 429 and a `Retry-After` rather than "wrong password".
- Only `http/1.1` is offered over ALPN, so h2 framing is unreachable (SQU-126).

[Unreleased]: https://github.com/excavador-turing/bmcd/compare/v2.10.1...hive
[2.10.1]: https://github.com/excavador-turing/bmcd/releases/tag/v2.10.1
[2.10.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.10.0
[2.9.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.9.0
[2.8.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.8.0
[2.7.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.7.0
[2.5.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.5.0
[2.4.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.4.0
