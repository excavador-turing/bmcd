# Changelog

The BMC daemon, as built by this fork. Upstream's own history is in the git
log; this file starts where the fork diverges, at 2.3.7.

The daemon ships to a board inside a firmware release, pinned by commit, so a
version here only reaches hardware once `BMC-Firmware` bumps that pin.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [2.15.1] — 2026-09-09

### Fixed

- **Park mode did not compile for the board.** `statvfs`'s block counts are
  `u64` on x86-64 and **`u32` on this board's 32-bit ARM**, so
  `blocks_available() * fragment_size()` built on a workstation and failed to
  cross-compile — and would have overflowed at 4 GB if it had. Cast to `u64`
  before multiplying, which is what `select_staging_dir` two hundred lines
  below already does.

  Caught by the firmware build, not by `just check`. That is the point worth
  recording: **a green workstation check is not evidence the daemon builds for
  the board.** Nothing in this repository's CI cross-compiles; the firmware
  build is the only thing that does, and it is two hours long.

  The widening is a small generic rather than a cast at the call site, because
  neither `as u64` nor `u64::from` works on both: both are required for the
  target and both trip clippy on the host, where the types already match. A
  generic conversion is correct on both platforms and carries the reason with
  it, which a lint suppression would not.

## [2.15.0] — 2026-09-09

### Added

- **`type=thermal` reports each zone's trip points** (SQU-135). The governor is
  `step_wise`, so the fan's step is a *consequence* of which trips the board
  has crossed — and "the fan is on step 4" with no reason attached is the
  question this fork was asked to investigate. Measured on the board:
  `fan_min` 20 °C, `fan_low` 45 °C, `fan_mid` 60 °C, `fan_high` 70 °C, all
  `active`, plus `hot` at 95 °C; the board reads 50.6 °C, which is above
  `fan_low`, which is step 4. Nothing mysterious, and now nothing hidden.

  Read from sysfs rather than written into a client, for the same reason the
  cooling levels are: it is a fact about *this* board. The trip type is passed
  through as the kernel spells it rather than mapped, so a client that meets an
  unfamiliar one shows it instead of swallowing it.

## [2.14.0] — 2026-09-09

### Added

- **Config export and import** (SQU-142). `opt=get&type=config` returns
  everything a person has configured as one JSON document — hostname, time
  servers, firmware sources, node names — and `opt=set&type=config` applies
  one. Board B is a named milestone, and the first thing anyone will want is
  "make it like board A"; today that is five settings re-entered by hand plus a
  metrics token re-pasted into a scrape config.

  **An allow-list, never "everything on the overlay."** The overlay also
  carries `htoprc` and a zero-byte `crond.reboot`, residue of tools that were
  removed, and a wholesale copy would clone that junk to a board that never had
  them.

  **The token is what makes an export sensitive.** `secrets=1` includes the
  metrics token; without it the key is absent from the document entirely rather
  than present and empty. `contains_secrets` is on the document's face, because
  the difference decides how the file has to be handled and nobody will
  remember which request produced it. A token being imported is validated as
  hexadecimal — it is written into a `KEY=VALUE` file and used as a Basic-auth
  password, so a newline or a colon in it would forge a field or split a
  credential.

  **Import is deliberately not transactional.** A hostname and a set of
  firmware sources cannot be rolled back together, and reporting a partial
  apply as a failure would leave an operator unsure which half took. Every
  field reports its own outcome as applied, skipped or failed.

  **`power_on_time` is never imported.** It is when *this* board last powered a
  node on; carried to another board it would report an uptime that never
  happened.

  The network configuration is not included and will not be: it is per-board by
  definition and already has its own reset path.
- `metrics_token::peek()` reads the stored token without minting one. A backup
  must record what a board has, not create a credential as a side effect of
  being backed up — which `ensure()` would have done.

## [2.13.0] — 2026-09-09

### Added

- **The time sources are a setting** (SQU-167). `opt=get&type=ntp` returns the
  configured servers, whether the running image can accept any, and the clock's
  state; `opt=set&type=ntp&servers=a,b` replaces them and reloads chrony live.

  The image ships `pool pool.ntp.org iburst`, so a board synchronises straight
  to the public pool with no way to change that short of an SSH session. The
  RTC covers boot, so this is not about correctness at power-on: it is about
  the one time somebody is standing at the board — a WAN outage — when chrony
  loses its only source and the clock quietly stops being disciplined.

  Written as a chrony `sourcedir` file rather than by rewriting
  `/etc/chrony.conf`: that config is in the read-only image, and
  `chronyc reload sources` picks up the change without restarting chronyd or
  losing the discipline it has built up. The first server is written with
  chrony's `prefer`, and only the first — chrony treats several preferred
  sources as equals, which is not what an ordered list means.

  Server names are validated against an allow-list before they are written,
  and that is not politeness: the lines are `server <value> iburst` in another
  daemon's config file, so a value carrying a newline would append directives
  of its own. `configurable` reports false on an image whose `chrony.conf`
  predates the `sourcedir` line, because a setting that is saved and silently
  never read is worse than one that is absent.

- **The hostname is a control** (SQU-138). `opt=get&type=hostname` returns the
  live name and the one that takes effect at the next boot — they differ when
  someone has run `hostname` by hand — and `opt=set&type=hostname&name=…`
  changes both and restarts `mdnsd` so the board stops advertising the name it
  no longer has.

  Validated as a single DNS label: letters, digits and hyphens, at most 63,
  no leading or trailing hyphen, and **no dots**. The file format would accept
  a qualified name and `mdnsd` would then advertise `a.b.local`, which is not
  what anyone meant.

  Renaming changes the `instance` label on every metrics series, so a
  Prometheus history does not follow it. That is a decision, not a side
  effect, and it belongs to whoever presses the button — the interface says so
  before it happens.

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

[Unreleased]: https://github.com/excavador-turing/bmcd/compare/v2.15.1...hive
[2.15.1]: https://github.com/excavador-turing/bmcd/releases/tag/v2.15.1
[2.15.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.15.0
[2.14.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.14.0
[2.13.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.13.0
[2.12.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.12.0
[2.11.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.11.0
[2.10.1]: https://github.com/excavador-turing/bmcd/releases/tag/v2.10.1
[2.10.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.10.0
[2.9.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.9.0
[2.8.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.8.0
[2.7.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.7.0
[2.5.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.5.0
[2.4.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.4.0
