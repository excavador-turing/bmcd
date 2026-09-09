# Changelog

The BMC daemon, as built by this fork. Upstream's own history is in the git
log; this file starts where the fork diverges, at 2.3.7.

The daemon ships to a board inside a firmware release, pinned by commit, so a
version here only reaches hardware once `BMC-Firmware` bumps that pin.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [2.25.0] — 2026-09-09

### Added

- **The published schemas are now checked against what the daemon actually
  sends** (SQU-149, step 3). A schema is derived from a type and a response is
  serialised from the same type, so the two agree only as far as `schemars`
  and `serde` agree — and they part company over exactly the attributes this
  daemon uses: `skip_serializing_if`, `rename_all`, and serde `default`s.
  Where they part, the document describes a shape the board never sends, which
  is what `tpi`'s three dead formatters were made of.

  Six tests build what a board would send and hold it against what the
  document promises. Proven by making the two disagree on purpose: adding
  `#[schemars(rename_all = "PascalCase")]` beside `PortKind`'s
  `#[serde(rename_all = "lowercase")]` fails both switch-port tests, naming the
  schema. Removing it returns 13 passed.

  The cases chosen are the ones with something to get wrong: a board with no
  promotion log, where `promotion_history` is omitted entirely rather than sent
  as null; the same board mid-update with every field present; a switch port
  whose `kind` is a renamed enum; a port the driver never probed, with every
  optional absent at once; and a fan both held and governed.

  `jsonschema` is a **dev-dependency**, so none of this reaches the board.

### Fixed

- Nothing was wrong: `schemars` handles `skip_serializing_if` and
  `rename_all` correctly today. These tests record that and stop it drifting,
  which is a different and smaller claim than fixing something.

## [2.24.0] — 2026-09-09

### Added

- **The OpenAPI document now describes what seven operations answer with**
  (SQU-149, step 2). `/thermal`, `/health`, `/cooling`, `/network`,
  `/firmware/slots`, `/firmware/sources` and `/firmware/check` carry real
  response schemas instead of `{}`.

  The schemas are **derived from the very types the handlers serialise**, not
  written out beside them. The failure this exists to stop is a client written
  against a shape the daemon has never sent — `tpi` shipped three such
  formatters, and nobody noticed because a fourth bug stopped any of them
  running. A hand-written schema is that same failure with an extra step
  between it and the reader.

- **The operations that describe nothing are listed, not merely absent.**
  Eleven handlers assemble their answer with `json!` from several sources and
  have no single type to derive from. They are named in `UNTYPED`, their `200`
  says so in words, and a test requires every read operation to be either
  described or on that list. Proven by removing `/hostname` from it: the test
  fails naming exactly that path.

  Two more tests hold the rest together: nothing may be both described and
  declared undescribed, and every `$ref` in the document must resolve to a
  component that exists. A dangling `$ref` renders as an empty box in every
  viewer and generates a client that will not compile.

### Changed

- `schemars` is a new dependency, and it costs **186 KB** of release binary,
  measured rather than assumed: 12,747,768 bytes before, 12,938,240 after.
  About half a percent of the rootfs, against a CI gate at ninety.

  A first measurement said it was free, which was wrong: with nothing calling
  the derived impls the linker had stripped them all. The number above is from
  a build where the document actually references them.

## [2.23.0] — 2026-09-09

### Added

- **One audit line per mutating API call** (SQU-108). Until now a `power off`
  from the web interface, from `tpi` on the board and from a token over the
  network all looked the same in the log, because none of them was recorded
  with an identity. Each now writes a line on an `audit` target naming the
  action, the node, the caller, their address and the outcome.

  Written around the dispatcher rather than in each handler, for the same
  reason the dispatcher exists: there is one table of operations and two
  spellings of it, so an endpoint added later cannot quietly miss the line.
  Reads are not logged — they are most of the traffic and none of the risk.

- **The loopback bypass is named in that line.** `/api/bmc` skips
  authentication entirely for requests from the board itself; the line for one
  says `loopback (unauthenticated)` rather than folding it in with a real
  credential. It is the entry somebody reading an audit trail would most want
  to be able to find, and it is the fault this fork already documents.

- **Audit lines go to the system log as well as the rotating file.** Both
  `/tmp` and `/var/log` are tmpfs on this board, so a line that reaches only
  the file is gone at the next boot — which is exactly the event one would be
  trying to account for. They are sent to `/dev/log` as `authpriv.info`; a
  board with no syslogd keeps the file copy and says so once on startup.

  Verified against the board's own BusyBox syslogd, which renders the datagram
  as `Sep  9 14:57:06 hive-bmc authpriv.info bmcd[4242]: …`.

### Changed

- **A bearer token now remembers who it was issued to.** The token store held
  only a last-access time, so a request authenticated by a token could not
  name its user — every audit line for every logged-in operator would have
  read the same. A unit test requires the username to come back from
  authorisation rather than merely `Ok`.

## [2.22.0] — 2026-09-09

### Fixed

- **A flash now targets the module that was asked for** (SQU-105). On a v2.5
  board all four modules sit behind one GL850 hub, so every module in maskrom
  enumerates at once — and `find_first` returned whichever answered first,
  ignoring the requested node. Its comment still said "only one node can be
  visible at any given time", which was true of v2.4's single mux. The result
  was a flash of node 2 that wrote node 1 and reported success, or
  "Several supported devices found" on a routine two-module bench.

  A device is now accepted only on the hub port that node is wired to. A
  device on any other port is refused, naming both: *"node 3 requested on
  1-1.3; found Rockusb on 1-1.1 instead"*. The whole failure mode was a
  success message, so the replacement had to be an error that says what it
  found.

  The same rule applies to the mass-storage side: `get_device_path` used to
  demand exactly one Rockchip block device on the entire board and give up
  otherwise. It now filters by port, so two modules in maskrom stop being an
  ambiguity to report and become two ports to tell apart.

- **The node-to-port mapping is read from the device tree, not assumed.** The
  v2.5 DTS declares `hub@1` with `node1@1` through `node4@4`, and the kernel
  publishes all of it under `/proc/device-tree`. Node N is port N on this
  board; hard-coding that would have been correct and would still have been a
  guess, and a wrong guess here writes the wrong module — which is the harm
  being fixed. A unit test builds a tree wired in the opposite order and
  requires the opposite answer.

  A board that describes no such hub — v2.4, one node visible at a time —
  yields no topology, and the first-match behaviour is kept unchanged. The
  device tree is therefore also the board-revision test, which is better than
  a revision string because it is the fact the kernel itself is acting on.

## [2.21.0] — 2026-09-09

### Added

- **A fan can now be held at a step** (SQU-170). `opt=set&type=cooling` takes a
  new optional `mode`: `manual` pauses the zone's governor and then writes the
  step, `auto` hands the fan back. Omitted, the request means exactly what it
  always did — write the step and leave the governor running — so a client
  written before this keeps its behaviour.

  Until now `set_cooling_speed` wrote `cur_state` into a zone whose policy is
  `step_wise`, and the governor returned the fan to the trip the board was
  above within a poll. The write succeeded and the setting did not survive,
  which is the defect behind the fan slider that springs back.

  The governor is paused *before* the step is written. A `step_wise` poll is
  short enough to land between two writes done the other way round, which
  would leave the governor off on a step nobody asked for. If the step then
  fails to write, the governor is handed back rather than left paused.

- **`type=cooling` reports `zone` and `overridden`**, so a client can tell
  whether a step it writes will hold instead of leaving a person to watch a
  slider and guess. `zone` is read from the zone's own `cdevN` symlinks rather
  than assumed: the kernel records the binding on the zone's side only, and
  `thermal_zone0` driving `cooling_device0` is true of this board, not of the
  interface.

- **`bmcd_cooling_overridden`**, 1 while a fan is held. A hold that outlives
  the person who set it is the failure mode worth alerting on.

### Changed

- **A held fan is taken back above the zone's hottest `active` trip**, checked
  every 15 seconds. This board declares no `critical` trip — the hottest thing
  in its device tree is `hot` at 95 °C, which notifies and does not act — so
  nothing else would intervene if a fan were left on a low step while the board
  climbed. Somebody who paused the governor and walked away is not expressing a
  preference about 80 °C.

  The ceiling is the zone's own trip rather than a constant, for the same
  reason the rest of the daemon reads its trips instead of carrying a table.

- **Every paused governor is resumed at startup.** A hold lives in the kernel,
  not in this process, so one left behind by a daemon that died is still in
  force with nothing tracking it.

- **A hold is deliberately not persisted**, which is the one way it differs
  from a plain speed. A persisted speed is replayed into a board whose governor
  overrules it immediately, so the worst it can do is be briefly wrong. A
  persisted hold would come back after a reboot with nothing regulating the fan
  and nobody present who remembers asking for it.

## [2.20.0] — 2026-09-09

### Added

- **`bmcd_process_threads`** (SQU-172). The companion to
  `bmcd_process_resident_bytes`, and the number that says *which kind* of
  growth is happening: a heap leak grows the resident set with the thread count
  flat, while a leaked task or an unreaped blocking thread grows both, because
  every thread carries a stack.

  Added because the 2026-09-09 outage could not be attributed. The board's
  memory series was queried afterwards and settles one thing: available memory
  fell about 0.95 MB a minute from 01:15 and **recovered at each daemon
  restart** (+3.4 MB, then +10.7 MB), so the memory was held by this process
  and freed when it exited. What it does not settle is whether that was the
  heap or threads. This metric answers that at a glance next time.

  Read from `/proc/self/status`, not by counting `/proc/self/task`, which would
  be a directory read per scrape on a 116 MB board.

### Changed

- **The hardware-less build starts** (SQU-117). `run_event_listener` opens
  `/dev/input/event0`, which exists only on the board, and returned an error
  `main` propagated -- so `--features stubbed`, the build whose entire purpose
  is running without a board, could not start on one. It now warns and carries
  on under that feature only. The default build, which is what the firmware
  compiles, still treats a missing input device as fatal.

  This does not make the daemon properly hardware-less. `/sys/class/thermal`
  is not stubbed at all: on a workstation it enumerates the host's cooling
  devices and reports them as the board's. See SQU-117 for the rest.


### Added

- **CI compiles for the board** (SQU-171). 2.15.0 multiplied two `statvfs`
  counts, which are `u64` on x86-64 and `u32` on this board's 32-bit ARM. Every
  CI job was green and the Buildroot cross-compile failed two hours later,
  because that build was the only thing in the estate that compiled this daemon
  for the target.

  A new `cross` job runs `cargo check` against
  `armv7-unknown-linux-gnueabihf` with an armhf sysroot, in about twenty
  seconds. Verified by reintroducing the 2.15.0 expression on a scratch tree:
  clippy stays green while the new job fails with `expected u32, found u64` at
  the comparison. That asymmetry is the whole point.

  The board is actually built **soft-float** (`BR2_ARM_EABI=y`), so its triple
  is `armv7-unknown-linux-gnueabi` -- Debian's `armel`, which Ubuntu does not
  carry. The hard-float target stands in for it, and the job asserts what makes
  that safe: rustc's cfg for the two differs in `target_abi` and nothing else,
  so type layout and integer widths are identical. The assertion fails if they
  ever diverge further.


## [2.19.1] — 2026-09-09

### Fixed

- **Two strings had source indentation in the middle of a sentence.** A literal
  wrapped across source lines and joined by rustfmt keeps the leading spaces of
  the continuation. Nothing complains: it compiles, it renders, and the
  exposition stays valid Prometheus.

  One was the `# HELP` text of `bmcd_firmware_promotion_total`, which is
  published to every scraper and shown in dashboard tooltips. The other was the
  error shown when parking an image with no SD card mounted.

- **A test that covered nothing.** The guard written for the above passed with
  the defect still present: both metric fixtures leave `promotion_history` at
  `None`, so neither renders the family the test was written for. It now fills
  the optional fields, and asserts the family is in the output before checking
  it -- so the test cannot go quiet again.

## [2.19.0] — 2026-09-09

### Added

- **A rollback can be named** (SQU-158). The interface said *"Rollback: version
  not readable"* — honest, because that volume is never mounted, and useless to
  somebody deciding whether to press Reboot.

  Only the stager ever knows: by the time the gate runs it is the *new* image
  reading `/etc/os-release`. So both stagers now write `REPLACES=` into the
  staged note, `S99postupdate` copies it to `/mnt/overlay/rollback-version` at
  the instant it promotes, and `firmware_slots` reports it as the rollback
  slot's version.

  A board with no such file still renders "not readable". The version is
  reported when it was recorded and never guessed — the same rule the rest of
  this endpoint follows.

## [2.18.0] — 2026-09-09

The board wedged at 02:11 UTC after losing roughly a megabyte a minute under
continuous polling (SQU-172). Its kernel still answered ICMP; nothing
listened. The four compute modules were untouched. Recovering it needs a
person at the rack, and the metrics could only show the *board's* memory
falling — not what was consuming it. These three changes are so the next
occurrence is diagnosable, and so one of the plausible causes cannot happen.

### Added

- **`bmcd_process_resident_bytes`**, this daemon's own resident set from
  `/proc/self/statm`. Board memory says the board is being consumed; only this
  says by whom. Its absence is why the diagnosis above had to be argued from
  timing rather than read off a graph.

### Fixed

- **The catalogue's `refreshing` flag can no longer stick.** It was cleared by
  a statement after `fan_out().await`, so a panic anywhere in the fan-out left
  it set for ever — and the interface polls every two seconds for as long as
  it is set. One browser tab left open then becomes a permanent 2-second poll
  of a board with 116 MB of RAM. It is cleared by a drop guard now, which runs
  on a panic as well as on success.

### Changed

- `FRESH_AFTER_ERROR` carries the arithmetic that makes it consequential: one
  configured source publishes no checksums and therefore errors routinely, so
  this two-minute value is the interval that actually governs, and each expiry
  spawns a shell and a `curl` per source.

## [2.17.0] — 2026-09-09

### Added

- **`bmcd_firmware_promotion_total{result}`** (SQU-141), counted from the
  gate's own log. Its history lived only in `/mnt/overlay/postupdate.log`, so
  "nineteen consecutive clean promotions" was a number counted by hand and
  written on a website, where it went stale.

  It had also gone *wrong*: the board's own log says **fifteen boots ran the
  gate and one was refused** — fourteen promotions, not nineteen. The
  hand-counted figure had drifted, which is the entire argument for the
  metric. The site's number will link to a query instead of a claim.

  `promoted` is derived as attempts minus rollbacks, because the gate has no
  single line meaning *kept*: it can finish through the metrics check, by
  skipping that check on a board with no `curl`, or by promoting anyway when
  there is no volume to fall back to. Deriving covers all three. The one
  inaccuracy is a board cut off mid-gate, whose unfinished attempt counts as
  promoted — said plainly in the metric's help text rather than hidden.
- `firmware_slots` carries `promotion_history`, so the same counts are
  available without a scrape credential.

## [2.16.0] — 2026-09-09

### Added

- **One path per operation** (SQU-149, step 1). `GET /api/bmc/thermal`,
  `POST /api/bmc/hostname`, and thirty-three more, each reaching **the same
  arm of the same dispatcher** as its `?opt=&type=` form — there is no second
  implementation to drift. The legacy form stays exactly as it is; it is what
  upstream's `tpi`, this fork's `tpi`, the web interface and a decade of
  scripts speak.

  What differs, and only on the new paths: a success answers with the bare
  result rather than `{"response":[{"result":…}]}`; a refusal answers with
  `application/problem+json` (RFC 9457) carrying the same message the legacy
  form puts in `result`; mutations are `POST` and take parameters as a form or
  a JSON body as well as in the query string, with the query string winning on
  a clash. `POST /firmware/sources` and `POST /config` accept the document
  itself as the JSON body — the shape a person would write — rather than only
  the escaped-string-in-a-parameter shape the legacy form needs.

- **`GET /api/bmc/openapi.json`**, OpenAPI 3.1, built from the same table that
  registers the routes, so an operation cannot be documented without existing
  or exist without being documented. This release describes every operation's
  path, method, summary, the parameters that are known, and the error shape.
  **Response bodies are still `{}`**: typing them from the serde types the
  handlers return is the next step, and an untyped-but-honest response is
  better than a typed one guessed from memory — the day before this was
  written, a client shipped three formatters against shapes this daemon has
  never sent.

### Changed

- `api_entry`'s dispatch is a function of its own, with two callers.

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

[Unreleased]: https://github.com/excavador-turing/bmcd/compare/v2.19.0...hive
[2.19.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.19.0
[2.18.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.18.0
[2.17.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.17.0
[2.16.0]: https://github.com/excavador-turing/bmcd/releases/tag/v2.16.0
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
