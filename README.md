# bmcd — `excavador` fork

> **This is a fork of [turing-machines/bmcd](https://github.com/turing-machines/bmcd).**
> `master` is upstream's `master`, commit for commit. **`hive` is the branch that
> gets built**: eleven functional commits on top of the `v2.3.7` tag, plus the CI
> and documentation ones. Everything below the fold
> is upstream's own README, unchanged.
>
> The daemon does not ship on its own. Our [BMC firmware
> fork](https://github.com/excavador/tp2-bmc-firmware) pins it *by commit* and
> builds it into the image, so a change here is not real until that pin moves.

## Running now

One change from this branch is on hardware: firmware
[`v2.2.0-unstable-hive.5`](https://github.com/excavador/tp2-bmc-firmware/releases)
pins bmcd at `27ec80f`. It was measured on the board after that flash.

| verified | evidence |
|---|---|
| **A daemon restart — and therefore a firmware upgrade — no longer power-cycles running compute modules.** On start the daemon used to restore `activated_nodes` from `bmcd.bin` and drive the rails to it, so a stale persisted value switched running nodes off or on. It now reads the node enable lines back: when any node reads on, that live state is the truth and is written back to the persistency; only when all four read off (a cold boot) is the persisted state restored. Fixes upstream [bmcd#90](https://github.com/turing-machines/bmcd/issues/90) | Two firmware flashes and a daemon restart with four modules powered: every rail stayed on, every `/proc/uptime` stayed monotonic |

Two commits carry it. `810e134` adds `PowerController::get_power_node`, the
read-back counterpart of `set_power_node` — on the latching board the kernel
keeps the node enable latches across a reboot of the BMC, so what it returns is
what the nodes actually get, not what the daemon last asked for. `27ec80f` puts
`initialize_power` in front of the old two lines and logs which of the two paths
it took.

Both paths still call `activate_slot`, deliberately: after a warm boot the
kernel initialises regulator state from the device tree rather than from the
preserved latch, so writing `enabled` to a rail that is already on is
electrically a no-op that brings the regulator bookkeeping in line with the
hardware — without it a later power *off* would not reach the rail. When the
lines cannot be read the daemon warns and takes the cold-boot path, which is the
old behaviour. The stubbed HAL returns all-off, which keeps the stub on the
cold-boot path.

## Built, not yet on a board

**Nothing in this section has run on hardware.** Every functional commit after
`27ec80f` is in no flashed image: the firmware's `hive` branch still pins
`27ec80f`, and the bump to `df1e8ec` sits on an open firmware pull request.
Read these as a diff with an argument behind it, not as behaviour.

The last four of them do come from measurements taken on the board -- `ge1`
sitting at `lowerlayerdown`, the serial in the EEPROM, three power-on times
resetting to the BMC's own uptime, `tpi info` disagreeing with the about page --
but the *fixes* have not been back on hardware.

| not yet proven | what the change does |
|---|---|
| **A browser can open the serial console** | `/api/bmc/serial/ws` sits inside the authenticated `/api/bmc` scope, and the browser `WebSocket` constructor cannot set `Authorization` -- so the console worked from `curl` and answered a page with 401. A handshake that sends no `Authorization` header may now carry its bearer token as a websocket subprotocol, `Sec-WebSocket-Protocol: bmcd.serial.v1, bmcd.bearer.<token>`, which is what the Kubernetes API server does for `exec` and `attach`. The handshake response names the plain protocol back, never the credential, because a browser closes a connection the server did not answer with one of the names it offered. Not a query parameter: this daemon traces request paths, so a `?token=` would write the credential into the log. The fallback is read only off a complete handshake and only when `Authorization` is absent, so it is not a second way to authenticate a REST call; `Bearer`, `Basic` and the loopback exemption are untouched |
| **The SoC temperature is reachable over the API** | The firmware only just gained a working SoC thermal sensor -- it was never described in any device tree, and the node had to be added -- and nothing in the daemon exposed it, so the web UI could not show a temperature at all. `opt=get&type=thermal` now returns two lists: every `thermal_zone*` as a `name` (the zone's `type`, `bmc-thermal` here) and a `temperature_c`, and every `cooling_device*` as a `name`, `cur_state` and `max_state`. Read straight from `/sys/class/thermal`, no shelling out. Millidegrees are converted to degrees at one decimal -- the board reads `52539`, which is `52.5` -- because raw millidegrees are unreadable and a whole degree throws away detail the sensor has. Each cooling device also carries `levels` and `max_level` -- the PWM duty behind each step, so a client can show a real percentage instead of "4 of 6". That mapping is in the device tree and nowhere in `/sys/class/thermal`, so it is read off the board's own tree at request time: the device's `type` names the platform driver, the driver's directory names the device bound to it, and that device's `of_node` symlink lands on the node, whose `cooling-levels` property is raw big-endian cells -- `0 16 32 64 102 170 254` here, and `max_level` is the last of them, 254, not 255. No path in the device tree is assumed. The table is reported only when its length matches `max_state + 1`, which is how the pwm-fan driver derives that number in the first place; a length that disagrees, a driver with no bound device or with more than one, and a node with no `cooling-levels` are all `"levels": null` rather than another fan's numbers. A board with no thermal zone at all, which is every image before this one and every v2.4 board, is a 200 with two empty lists: a caller has to be able to tell "this board cannot measure temperature" from "this board is at 0 degrees". `type=cooling` is untouched |
| **The switch's own link state is reachable over the API** | Nothing in the daemon reported anything about the on-board Ethernet switch, although the kernel registers a netdev per port and knows all of it. `opt=get&type=network` now returns, for each of `node1`-`node4`, `ge0` and `ge1`: whether it is a node port or an uplink, whether the kernel has it at all, carrier, `operstate`, speed, duplex and the four byte/error counters. Read straight from `/sys/class/net`, no shelling out. The failure it exists for is a kernel where the switch driver does not probe -- the BMC stays perfectly reachable over its own interface while all four compute modules are cut off -- so every port is always listed and an absent one is `"present": false` rather than a missing entry or a 500 |
| **The A/B firmware slots are reachable over the API** | The board takes firmware upgrades A/B -- the new image goes into the rootfs UBI volume that is not running, `nextboot` sends U-Boot at it once, and a promotion script keeps it or puts the old one back -- and nothing in the API said which volume the board booted, how big either is, whether an upgrade is waiting for the next boot, or what the promotion script decided last time. `opt=get&type=firmware_slots` now answers all four, from `/sys/class/ubi`, `fw_printenv -n nextboot` and the tail of `/mnt/overlay/postupdate.log`. The running slot is the volume with a `ubiblock` device attached, not the one called `rootfs`. Its version comes from `/etc/os-release`; **the rollback volume is not mounted, so it has a name and a size and no version** rather than a guessed one. A board with no UBI, or without `fw_printenv`, answers 200 with `"present": false` and nulls -- and `update_staged` is `null` rather than `false`, because "nothing is staged" and "the environment could not be read" are different answers. Not `type=firmware`: that name has belonged to the transfer machinery since long before this fork |
| **The BMC's own condition is reachable over the API** | Everything the daemon reported was about the four compute modules or the board's peripherals; nothing was about the board running the daemon, which has 116 MB of RAM, five spare NAND eraseblocks and two clocks. `opt=get&type=health` now returns uptime and load from `/proc`, memory in bytes rather than meminfo's kibibytes, UBI's own eraseblock accounting from `/sys/class/ubi/ubi0` -- the same 2040 total, 5 available, 0 bad and 40 reserved that `ubinfo` prints, without the fork or the dependency on mtd-utils being in the image -- and every RTC the kernel registered. Clock synchronisation is the one thing that is not a file read: `chronyc tracking` is parsed for the stratum, the source and the offset, reported as system clock minus true time so a board that is behind is negative. `measured_by` says so. A board with no chrony answers `null` rather than `false`, because "not synchronised" and "we cannot tell" are different answers, and which of the two RTCs has a battery behind it is not claimed at all -- the kernel does not expose it |
| **There is a Prometheus scrape endpoint** | `/metrics` used to return the web UI's `index.html` through the catch-all, which is worse than a 404: a scraper sees HTTP 200 and a document it cannot parse. It now returns the text exposition format -- SoC temperature and fan state, per-port link, speed and byte/error counters, per-node power state and power-on time, the health values above, and the firmware slots as an info metric. Hand-written, no metrics crate: the format is two comment lines and a sample per value, and this daemon is cross-compiled into a firmware image that is at 78% of its flash slot. **It is authenticated**, behind the same `LinuxAuthenticator` that wraps `/api/bmc`, which accepts HTTP Basic -- so a scrape config authenticates with `basic_auth` and nothing else. Adding a second unauthenticated surface next to `/info` would have been the same finding twice |
| **`type=about` reports the board's serial number** | The 24c02 EEPROM at i2c 0x50 holds the factory serial next to the product name and the hardware revision, and the daemon already parses that header -- `board_model` and `board_revision` come out of it. `get_about` now also sends `board_serial` from the `FactorySerial` field of the same read, with the fixed-width field's NUL padding stripped and an unprogrammed EEPROM reported as `null` rather than as a string of padding. `board_model` and `board_revision` are left byte-for-byte as they were, padding included, because something may be matching on them |
| **`power_on_time` stops resetting for nodes 2, 3 and 4 on every daemon start** | `update_power_on_times` compared the new state of a node, which `bit_iterator` yields as 0 or 1, against `activated_nodes & (1 << idx)`, which is 0 or `1 << idx`. Those agree only for node 1. For every other node "already on, staying on" looked like a transition, so `initialize_power` -- which calls `activate_slot` on every start -- rewrote their power-on stamp to the moment the daemon came up. Measured after a BMC reboot with four modules running: node 1 at 52418 s, matching its own `/proc/uptime`, and nodes 2, 3 and 4 at 172 s, the BMC's own uptime. The comparison is now shifted down. Upstream bug, from 2023; boards carrying a wrong stamp recover it at the next real power cycle of that node |
| **The About page stops saying `Build version: vundefined`** | The web UI reads the daemon version from a key named `build_version`; `get_about` only ever sent `bmcd_version`. It now sends the same value under both names — `bmcd_version` stays, it is the documented key of the legacy API. The value is bmcd's own crate version (`2.3.7`), not the firmware release. The UI's *other* version bug, the doubled `v` in `vv2.2.0-…`, is on the UI side and is untouched here |
| **The About page reports the Buildroot release instead of the firmware name** | `get_about` filled its `buildroot` field from `PRETTY_NAME` in `/etc/os-release`, which our firmware stamps with the Turing Pi release — so a board built on Buildroot 2025.02.17 called its "Buildroot release" `Turing Pi v2.2.0`. It now prefers a `BUILDROOT_VERSION` key and falls back to `PRETTY_NAME`. The key is written by the firmware's `post_build.sh` from Buildroot's own `BR2_VERSION`, and that too is still on the open firmware PR, so an image built today has only the fallback to offer. `get_system_information` was left alone at the time and is fixed by the row below |
| **`tpi info` reports the Buildroot release too** | The fix above only reached `get_about`. `get_system_information` -- `opt=get&type=other`, and the unauthenticated `GET /info` -- kept deriving its `buildroot` field from `PRETTY_NAME`, so on the same board `type=about` answered `2025.02.17` while `type=other` answered `Turing Pi v2.2.0`, and `tpi info` reads the latter. Both now call one `buildroot_release` helper rather than spelling the rule out a third time. `type=other` also loses the surrounding quotes it used to pass through from the raw os-release line |
| **A firmware upload is staged on disk rather than in the RAM disk** | `os_update` copied the whole uploaded image into `/tmp/os_upgrade` before handing it to `osupdate`. `select_staging_dir` now takes the first of `/mnt/sdcard`, `/mnt/overlay`, `/tmp` that is a mount point *of its own*, is mounted read-write, and has room for the image. The mount-point test is what rejects an SD card that is not inserted — `/mnt/sdcard` exists as an empty directory either way, and a write into it would land on the root filesystem. Node flashing does not go through here; `flash_node` streams straight to the node block device |

Be careful with the last one, because the evidence is thinner than the story.
A 38 MiB upload did die about five seconds in with `exit status: 141`, and the
same write done by hand from the SD card completed. `/tmp` on this board is a
58 MB tmpfs and the BMC has 116 MB of RAM in total, so memory pressure is the
obvious reading — but **141 is 128+13, SIGPIPE, not the SIGKILL the OOM killer
sends**. Take RAM exhaustion as a strong hypothesis, not as a demonstrated
cause. What the change does prove is a class of failure removed: the image no
longer competes with the daemon for the same memory. And when none of the three
candidates qualifies it still falls back to `/tmp/os_upgrade`, so a board with
neither a card nor a writable overlay is exactly where it was.

## Things this daemon does that the code does not say out loud

Useful whether or not you care about the fork. All of it is upstream behaviour.

### The API is query-string RPC

Everything hangs off one resource. `GET /api/bmc?opt=get|set&type=X`, dispatched
by `api_entry` in [`bmcd/src/api/legacy.rs`](bmcd/src/api/legacy.rs) — a `match`
on the `(type, is_set)` pair. A missing `opt` or `type` is a 400, and so is a
`type` that no arm claims.

| `opt=get&type=` | |
|---|---|
| `about` | daemon version, build time, firmware version, Buildroot release |
| `cooling` | fan devices and speeds |
| `firmware_slots` | running and rollback firmware volumes, staged update, last promotion (*this fork*) |
| `health` | BMC uptime, load, memory, NAND wear and clock synchronisation (*this fork*) |
| `info` | IP addresses and storage |
| `node_info` | per-node auxiliary info |
| `nodeinfo` | hard-coded zeros; there is no implementation behind it |
| `network` | link state, speed, duplex and counters of the six switch ports (*this fork*) |
| `other` | the deprecated duplicate of `about` |
| `power` | per-node power state |
| `sdcard` | card size and free space |
| `thermal` | temperatures of every thermal zone, and the state of every cooling device with the duty behind its steps (*this fork*) |
| `uart` | drain the node's serial ring buffer |
| `usb` | USB host/device routing |
| `usb_node1` | node-1 USB alternative port |

| `opt=set&type=` | |
|---|---|
| `clear_usb_boot` | clear the USB boot pin |
| `cooling` | set a fan speed |
| `network` | reset the network interface |
| `node_to_msd` | expose a node as a mass-storage device |
| `nodeinfo` | deprecated, answers 501 |
| `power` | power nodes on and off |
| `reboot` | reboot the BMC |
| `reload` | restart the daemon (`/etc/init.d/S94bmcd restart`) |
| `reset` | pulse a node's reset line |
| `sdcard` | format the card |
| `uart` | write a line to a node's serial port |
| `usb` | set USB host/device routing |
| `usb_boot` | drive the USB boot pin |
| `usb_node1` | node-1 USB alternative port |

Three `type` values never reach that `match`, because actix guards route them
first: `type=flash` and `type=firmware` go to the transfer machinery (`opt=get`
is the status of a running transfer, `opt=set` starts one and returns a
`handle`), and `POST` with `opt=set&type=node_info` takes a JSON body. Those
guards matched the raw query string with `contains` until this fork made them
compare the whole parameter value -- a `type` that merely *starts* with
`firmware` was being swallowed by the transfer machinery. The
transfer itself is not query-string RPC: `POST /api/bmc/upload/{handle}` streams
the bytes and `GET /api/bmc/upload/{handle}/cancel` aborts it. Alongside those
sit `GET /api/bmc/backup` (a tar of `/mnt/overlay/upper`), `GET /api/bmc/info`,
`POST /api/bmc/serial/status`, and the serial websocket at `/api/bmc/serial/ws`.

### Authentication is `/etc/shadow`, watched

There is no user database. `LinuxAuthenticator` parses `/etc/shadow` at startup,
keeps username and hash in memory, skips any entry whose hash starts with `*`,
and — this is the part worth knowing — **inotify-watches the file**
(`CLOSE_WRITE`, plus `DELETE_SELF` to rebind after a rewrite). A `passwd` on the
board therefore takes effect on the next request, with no daemon restart. If the
watch cannot be set up the daemon logs `auto reloading of password-cache
disabled` and carries on with the cache it has.

Both schemes work. `Authorization: Basic` validates against the shadow hash on
every request. `POST /api/bmc/authenticate` exchanges credentials for a bearer
token, whose expiry (`token_expires`, default **10800 s** = 3 h) is counted from
its *last successful use*, not from issue.

**A websocket handshake has a third place to put the token.** The browser
`WebSocket` constructor takes a URL and a list of subprotocols and nothing else
-- a page cannot put a header on it -- so `/api/bmc/serial/ws` was reachable
from `curl` and unreachable from a browser. A handshake that sends no
`Authorization` header may name its bearer token as a subprotocol instead:

```text
Sec-WebSocket-Protocol: bmcd.serial.v1, bmcd.bearer.<token>
```

```js
new WebSocket(`wss://${host}/api/bmc/serial/ws?node=0`,
              ["bmcd.serial.v1", `bmcd.bearer.${token}`]);
```

`<token>` is the session token verbatim -- the `id` from
`POST /api/bmc/authenticate`, which is also its `X-Auth-Token` header. It is 64
characters of `[A-Za-z0-9]`, so it needs no encoding to be a legal subprotocol
name. It is treated as `Authorization: Bearer <token>` and nothing else: same
store, same expiry, same ban patrol.

The daemon answers with the first offered name that is **not** a credential:

```text
Sec-WebSocket-Protocol: bmcd.serial.v1
```

It never echoes the credential, and a browser fails a connection the server
answered with no subprotocol at all -- so **always offer a plain name, and put
it first**. `bmcd.serial.v1` is the name to use for the console; the daemon
does not care which it is. The fallback is read only off a complete websocket
handshake -- `GET`, `Connection: upgrade`, `Upgrade: websocket`,
`Sec-WebSocket-Version: 13` and a `Sec-WebSocket-Key` -- and only when
`Authorization` is absent, so it cannot become a second way to authenticate an
ordinary REST call. A token in the query string is **not** accepted, and that is
the point: request paths are traced, and a `?token=` would put the credential in
the log.

The ban is per peer address and lives in `ban_patrol.rs`. The real numbers:
`authentication_attempts` defaults to **5**, `BAN_DURATION` is **60 s** and
`BAN_LEVELS` is **10**. Consecutive failures are counted per peer; on the 5th the
peer is banned for 1 minute, and every failure after that doubles the ban —
2, 4, 8, … minutes — with the multiplier capped at `1 << 10`, so **1024 minutes,
about 17 hours**. One success clears the peer's counter outright. The
bookkeeping is in memory only: restarting the daemon forgives everyone.

### One endpoint answers without authentication

Two, really. Requests whose peer address is loopback skip the authentication
middleware entirely — that is how anything on the board talks to the daemon. And
when `redirect_http` is true the daemon also runs a plain-HTTP server on port 80
whose only job is to redirect to HTTPS; that server is built with `info_config`
but **without** the authentication wrapper, so `http://<board>/info` returns the
API version, build time, IPv4 address, `br0` MAC, firmware version and
`PRETTY_NAME` to anyone who asks.

### The scrape endpoint authenticates like everything else

`GET /metrics` answers in the Prometheus text exposition format, and it is
inside the authentication middleware -- `main` wraps it with the same
`LinuxAuthenticator` as `/api/bmc`. Basic is what a scraper can send, and the
daemon validates it against the shadow hash on every request:

```yaml
scrape_configs:
  - job_name: bmc
    scheme: https
    metrics_path: /metrics
    static_configs:
      - targets: ['<board>:443']
    basic_auth:
      username: root
      password_file: /etc/prometheus/bmc-password
    tls_config:
      # the daemon serves its own certificate; pin it with ca_file instead if
      # you have one
      insecure_skip_verify: true
```

Scrape the HTTPS port directly. Port 80 is the redirect server, and while a
scraper that follows redirects will land on the right place, it lands there
with an extra round trip per scrape. The loopback exemption applies here as it
does everywhere else: something running on the board itself needs no
credentials.

Absence is a missing metric, not a zero. A board with no thermal zone has no
`bmcd_temperature_celsius` at all -- not a `# TYPE` line with nothing under it,
and never a fabricated 0 -- so alert on `absent()` where the difference
matters. `bmcd_rtc_present` and `bmcd_build_info` are the two that are always
there.

## Who depends on this fork

Our firmware, and nothing else. `tp2bmc/package/bmcd/bmcd.mk` in
[excavador/tp2-bmc-firmware](https://github.com/excavador/tp2-bmc-firmware) pins
`BMCD_VERSION` to a **commit on `hive`**, not a tag, and fetches the GitHub
archive. Two consequences:

- **Every change here means recomputing `bmcd.hash`.** Buildroot runs
  `cargo vendor` and re-packs the archive (the `-cargo2` suffix), so the recorded
  sha256 covers the vendored crates as well as our source. It is computed in the
  pinned build container with Rust 1.85.0; a Rust bump can legitimately change
  it, and a stale hash fails the firmware build, not this one.
- The recipe installs with `--path ./bmcd`, because this repo's root
  `Cargo.toml` has been a **virtual manifest** since v2.3.5 split out
  `board_info`. `cargo install --path ./` on a workspace root fails.

## Building and checking it the way we do

We do not use `cargo cross` (upstream's instructions, below the fold). For a
change that only has to compile and pass its tests, one container is enough —
`rust:1.85-bookworm`, the same toolchain version the firmware vendors with:

```bash
docker run --rm -it -v "$PWD":/src -w /src rust:1.85-bookworm bash -c '
  apt-get update &&
  apt-get install -y libusb-1.0-0-dev libssl-dev pkg-config libudev-dev &&
  rustup component add rustfmt clippy &&
  cargo fmt --all -- --check &&
  cargo clippy --workspace --all-targets -- -D warnings &&
  cargo test --workspace
'
```

The `rustup component add` is not optional: the `rust:` images ship neither
rustfmt nor clippy. `--workspace` is, for the same virtual-manifest reason as
above. This builds for the host, not for `armv7`; it is a correctness check, and
the real cross build is the firmware's.

**Clippy is at zero warnings on `hive`, and should stay there.** The three
warnings inherited from the `v2.3.7` tag — `rand::thread_rng` deprecated twice
in test helpers, and `needless_lifetimes` on `WriteMonitor` — were fixed on
2026-09-07, along with two more that a dependency refresh surfaced. `Cargo CI`
runs clippy with `-- -D warnings`, so a new one fails the build.

Do **not** fix the lifetime warning the way upstream did. Upstream's `a15e8fc`,
one commit past our tag on `master`, cherry-picks onto `hive` without conflict
and then does not compile: it writes `impl<'_, W> AsyncWrite for
WriteMonitor<'_, W>`, and `'_` is a reserved name that cannot appear in a
generic parameter list, so rustc rejects it with `E0637`. Verified by trying
it. The form here is clippy's own suggestion — elide the parameter, keep `'_`
only in the type position. Adopting `master` is therefore still not a free
rebase, and a rebase that takes `a15e8fc` will conflict with our fix, which is
the outcome we want.

## Dependencies and security

Reviewed by hand on **2026-09-07** — there is no Renovate, no Dependabot and no
other bot on this repo, by choice. The next review is somebody's decision, not a
schedule's.

`cargo audit` reported **15 advisories and 11 warnings** before that review and
**3 and 3** after. What was applied was a `cargo update` inside the existing
semver ranges — no `Cargo.toml` requirement moved, no feature changed, and the
edition and `rust-version` are untouched. The upgrades that mattered for a
daemon in this position were `openssl` (use-after-free, and this is the TLS
stack the HTTPS listener runs on), `bytes` (integer overflow, on every request
path), `tracing-subscriber` (ANSI escapes from user input poisoning the log —
bmcd logs failed usernames), plus `tokio`, `crossbeam-channel` and two crates
that had been yanked.

Resolution is pinned to **Rust 1.85**. A plain `cargo update` pulls actix-web
4.15, `serde_with` 3.22, `time` 0.3.55 and the `icu_*` family, all of which now
require rustc 1.88 and none of which compile here, so they are held one minor
behind on purpose. Raising the toolchain is a decision about the Buildroot
toolchain, not about this repo — and it has to happen together with
`Cargo.lock`, `cargo_ci.yml` and `bmcd.hash`.

Three advisories were **declined**, each because closing it needs a major bump
or a toolchain move:

| advisory | reachable here? | why it is still open |
|---|---|---|
| `h2` 0.3.27 — RUSTSEC-2026-0258, unbounded empty DATA frames | **Yes, and pre-authentication.** `bind_openssl` advertises `h2` over ALPN, so a peer reaches HTTP/2 framing before the auth middleware runs | No fix exists at any version. The patch is in `h2 >= 0.4.16`, and every `actix-http` up to the newest (3.13.5) still pins `h2` 0.3.27. Only an actix-web 5 migration, or an upstream backport, closes it |
| `time` 0.3.45 — RUSTSEC-2026-0009, stack exhaustion | No. The flaw is in the RFC 2822 parse path; nothing in the graph uses it. actix parses HTTP dates with `httpdate`, `tracing-appender` only formats its own filename suffixes, and bmcd never touches cookies | Patched 0.3.47 requires rustc 1.88 |
| `remove_dir_all` 0.5.3 — RUSTSEC-2023-0018, TOCTOU | No. It arrives via the `tempdir` **dev-dependency** and is not in the shipped binary | Fixing it means replacing `tempdir` with `tempfile`, a test-code change left for its own decision |

Three warnings remain, none of them reachable as written: `bincode` and
`tempdir` are unmaintained (the first reads a local persistency file, the
second is test-only), and `circular-buffer`'s panic-safety unsoundness needs an
element type whose `Drop` or `Clone` can panic — the serial ring buffer is
`u8`.

Also worth knowing when reading `cargo audit` output here: it scans
`Cargo.lock`, which records the union over all feature combinations, not what
actually gets compiled. `rustls`, `rustls-webpki`, `ring`, `hyper-rustls` and
`tokio-rustls` are all in the lock and none of them is in the enabled-feature
graph — `reqwest` uses native-tls here. Five of the fifteen original advisories
were in that set. Check with `cargo tree -e normal -i <crate>` before treating
one as real.

---

# bmcd

`bmcd` or 'BMC Daemon' is part of the
[BMC-Firmware](https://www.github.com/turing-machines/BMC-Firmware) and is
responsible for hosting Restful APIs related to node management, and
configuration of a Turing-Pi 2 board.

## Building

This package will be built as part of the buildroot firmware located
[here](https://www.github.com/turing-machines/BMC-Firmware). If you want to
build bmcd in isolation, we recommend to use `cargo cross`. Given you have a
Rust toolchain installed, execute the following commands:

```bash
# Install cross environment
cargo install cross --git https://github.com/cross-rs/cross

# Execute cross build command for the Turing-Pi target.
cross build --target armv7-unknown-linux-gnueabi --release --features vendored
# A self contained binary is build when the "vendored" feature flag is defined.
# i.e. Openssl will be statically linked into the binary. This is not desirable
# when building the actual BMC-Firmware, but works great for debugging scenario's.

# Copy to turing-pi.
scp target/armv7-unknown-linux-gnueabi/release/bmcd root@turingpi.local:/usr/bin/
```

