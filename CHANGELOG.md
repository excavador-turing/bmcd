# Changelog

The BMC daemon, as built by this fork. Upstream's own history is in the git
log; this file starts where the fork diverges, at 2.3.7.

The daemon ships to a board inside a firmware release, pinned by commit, so a
version here only reaches hardware once `BMC-Firmware` bumps that pin.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [2.12.0] — 2026-09-09

### Added

- **Park an uploaded image instead of installing it** (SQU-134).
  `opt=set&type=firmware&park=1` writes the image to `/mnt/sdcard/firmware/`
  and stops: nothing is staged, nothing is armed, and the catalogue's `local`
  source lists it on the next read like any other candidate.

  Until now an upload was never a thing you *had*, only a thing that
  *happened* — `os_update` stages into a scratch directory, runs `osupdate` on
  it at once, and deletes the directory. That made the browser's upload
  control a second way to install, bypassing the catalogue, so an operator
  could upload one image and install another with the interface never showing
  which.

  Three refusals, each for a measured reason. The card must be mounted:
  `/mnt/sdcard` is an empty directory when no card is in the slot, and writing
  there fills the root filesystem instead. It must have room. And the write
  goes to a `.partial` file that is renamed only once it is complete and
  synced, because a truncated image in the directory the catalogue reads would
  be offered for install.

  Parking does **not** make the already-staged refusal, and that is
  deliberate: nothing is being armed, so a board with an image already staged
  can still be given another to choose from later.

## [2.11.0] — 2026-09-09

### Fixed

- **The catalogue no longer blocks the page, or every other request with it**
  (SQU-143, the cause behind SQU-132). Measured on the board first, because the
  ticket held a hypothesis and not a diagnosis:

  | request | before |
  |---|---|
  | `firmware_available&refresh=1`, four sources | **15.9 s** |
  | the same, asking for one source | **15.7 s** |
  | cached | 0.18 s |

  Three faults, compounding. The fan-out ran the sources through a single
  `spawn_blocking` and a `.map()`, so it cost their **sum** rather than the
  slowest. The cache mutex was **held for the whole of it**, so a "check now"
  froze every other reader too — including a page that wanted nothing but the
  cached list, which is why the freeze looked like it happened on open. And
  asking for one source refreshed all four, because the refresh was never per
  source at all.

  Now: one `spawn_blocking` per source through a `JoinSet`, answers reordered
  to the configured order; the lock taken only to store the result; and a
  caller gets the cached catalogue at once with `refreshing` set while the
  refresh runs behind it. `refresh=1` starts that refresh instead of waiting
  for the entry to age out, and returns immediately.

### Added

- `Catalog.refreshing` and `Catalog.age_seconds`, so a page can draw a spinner
  on its own control and say how old the list is without parsing a timestamp
  and trusting two clocks. `refreshing` is omitted when false, so a settled
  catalogue serialises exactly as before.
- The catalogue is primed at start-up. Otherwise the first caller after a boot
  pays for the fan-out, and that is the person watching a board come back from
  a firmware update.
- A refresh already in flight is joined, not duplicated: four sources would
  otherwise become twelve requests against a GitHub quota of sixty an hour.

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

[Unreleased]: https://github.com/excavador-turing/bmcd/compare/v2.12.0...hive
[2.12.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.12.0
[2.11.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.11.0
[2.10.1]: https://github.com/excavador-turing/bmcd/releases/tag/v2.10.1
[2.10.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.10.0
[2.9.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.9.0
[2.8.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.8.0
[2.7.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.7.0
[2.5.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.5.0
[2.4.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.4.0
