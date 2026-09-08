# bmcd — `excavador` fork

> **This is a fork of [turing-machines/bmcd](https://github.com/turing-machines/bmcd).**
> `master` is upstream's `master`, commit for commit. **`hive` is the branch that
> gets built**: nineteen functional commits on top of the `v2.3.7` tag, plus the
> CI, build-tooling and documentation ones. That count has gone stale twice
> already, so read `git log --oneline v2.3.7..hive` for the list and this
> sentence for the size. Everything below the fold is upstream's own README,
> unchanged.
>
> The daemon does not ship on its own. Our [BMC firmware
> fork](https://github.com/excavador/tp2-bmc-firmware) pins it *by commit* and
> builds it into the image, so a change here is not real until that pin moves.

## Running now: `v2.6.0`

Every functional change on this branch is on hardware. Firmware
[`v2.5.0`](https://github.com/excavador/tp2-bmc-firmware/releases) pins bmcd
at `9474e75`, and everything below was measured on the board after that
flash rather than inferred from a build.

This section used to say the opposite -- it described `27ec80f` under
firmware `hive.5` and carried a companion section headed "Built, not yet on
a board". Both went stale across ten releases while the code kept moving,
which is its own lesson: a fork's README is the one file nothing fails when
it is wrong.

| verified | evidence |
| -- | -- |
| **Node power state survives a BMC reboot.** The daemon reads the live rail state on start and adopts it, instead of re-applying what `bmcd.bin` persisted | Ten flashes and daemon restarts with four modules powered: every rail stayed on, every `/proc/uptime` monotonic. Fixes upstream [bmcd#90](https://github.com/turing-machines/bmcd/issues/90) |
| **`power_on_time` is per node, and is a duration** | Upstream inferred it from one shared bit, so the read could only ever agree for node 1. Confirmed live on 2026-09-08: node 1 reported 126327 s power-on against 58 s of OS uptime after a manual reboot -- the field tracks the rail, not the operating system |
| **An authenticated Prometheus endpoint**, with a credential that opens it and nothing else | 77 metric lines under the token; 401 for root, for a wrong token, and for the token against `/api/bmc` |
| **The About payload carries the kernel release** | `"kernel":"6.12.109"` on the board, matching `/proc/sys/kernel/osrelease` |
| **`firmware_slots` reports which image is staged** | A real stage wrote `VERSION=v2.5.0` with the checksum the updater had just verified; the promotion script cleared it. Proven by mutation too: no note reports `null`, a note reports every field, removing it returns to `null` |
| **`type=update_check` reports whether a newer release exists** | Answered by `tpi-selfupdate --check --json`, so the interface and the updater cannot disagree about what an upgrade would install |
| **HTTP/2 is refused rather than downgraded after the parser ran** | `curl --http2` negotiates HTTP/1.1. It previously answered `HTTP/2 401` -- the request had been through the h2 stack before authentication rejected it |
| **A serial console per module over a websocket** | A browser cannot set `Authorization` on a WS handshake, so the token rides the subprotocol, the pattern Kubernetes uses |
| **Firmware uploads are staged on disk, not in a 58 MB RAM disk** | Prefers `/mnt/sdcard`, then `/mnt/overlay`, then `/tmp` -- the first that is a real mount with room |
| **New API types**: `network`, `thermal`, `firmware_slots`, `health`, `metrics_token`, `update_check` | All answering on the board |


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
sit `GET /api/bmc/backup` (a tar of `/mnt/overlay/upper`),
`POST /api/bmc/serial/status`, and the serial websocket at `/api/bmc/serial/ws`.
There is no `GET /api/bmc/info`: it answers 401 unauthenticated, which makes it
look like a route, and once authenticated it falls through to the SPA and
returns `index.html`.

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
peer is banned for 1 minute, and each further failure doubles the ban —
2, 4, 8, … minutes — with the multiplier capped at `1 << 10`, so **1024 minutes,
about 17 hours**. Requests made *while* a ban stands are refused without
counting against it, so what walks the ladder is the next failure after each
ban lapses, not the retries during it. One success clears the peer's counter
outright. The bookkeeping is in memory only: restarting the daemon forgives
everyone.

**A ban is answered as a ban, not as a wrong password.** Every other
authentication failure is the 401 (or, on `POST /api/bmc/authenticate`, the
403) it has always been; `ExceededAllowedAttempts` is **429 Too Many
Requests** with `Retry-After` in seconds and the time left in the body, and no
`WWW-Authenticate`, since a challenge there only tells a browser to prompt for
the password again. `ban_patrol` runs on every authenticated request, not only
on the login endpoint, so a 429 can come back from anything under `/api/bmc`
or from `/metrics` — a client that treats 401 and 429 alike will read a
lockout as a credential problem, which is precisely the bug this replaced.
Loopback never sees it: that exemption is taken before the ban is consulted.

### One thing answers without authentication

The loopback exemption, and nothing else. Requests whose peer address is
loopback skip the authentication middleware entirely — that is how anything on
the board talks to the daemon.

There used to be a second, and this section used to say so. When `redirect_http`
is true the daemon runs a plain-HTTP server on port 80 whose only job is to
redirect to HTTPS, and `/info` was registered on it **without** the
authentication wrapper the HTTPS server gets — so `http://<board>/info` returned
the API version, build time, IPv4 address, `br0` MAC, firmware version and
`PRETTY_NAME` to anyone who could reach port 80, in the clear, and passively
readable by anything on the segment. `e4e5eee` removed it (SQU-125).

It was a removal rather than a move because there was nowhere to move it to:
`info_handler` was registered only through `info_config`, and `info_config` only
on that redirect server, so no authenticated equivalent existed. The same data
is available authenticated at `/api/bmc?opt=get&type=info`. Port 80 now does
nothing but redirect. **This deletes a public interface of upstream's daemon** —
anything outside this estate polling `http://<board>/info`, a discovery script
or a monitoring probe, stops getting an answer.

### The scrape endpoint has a credential of its own

`GET /metrics` answers in the Prometheus text exposition format, and since
v2.6.0 it is **not** behind the `LinuxAuthenticator` that guards `/api/bmc`.
It takes a separate token instead.

This is worth stating plainly because the obvious design is wrong here.
`LinuxAuthenticator` parses `/etc/shadow`, verifies a hash, and stops; there
is no role, no permission and no per-route check anywhere in the module. So
**every account with a valid shadow entry can power four compute modules off
and flash the firmware.** While `/metrics` shared that authenticator, the
only credential that could scrape the board was one that could also destroy
it -- and a scrape config is a bad place to keep such a thing. Adding a
second Unix account would not have helped: it would have created a second
credential with the same authority.

The token lives at `/mnt/overlay/metrics-token`, mode 0600, on the overlay
because both firmware images mount it -- a scrape must not break because the
board took an A/B update. It is created on **first read**, so a board nobody
scrapes never carries a credential, and it is rotated from the web
interface or through `opt=set&type=metrics_token`. The Basic username is
`metrics`, which is deliberately not a Unix account: nothing resolves it
against `/etc/passwd`.

Measured on the board when firmware v2.5.0 was flashed:

| request | answer |
| -- | -- |
| `metrics` + token -> `/metrics` | **200** |
| `metrics` + token -> `/api/bmc` | **401** |
| `root` + root's password -> `/metrics` | **401** |
| `metrics` + a wrong token -> `/metrics` | **401** |
| `root` + the token -> `/metrics` | **401** |

The second row is the one that matters: if it ever answers 200, the token is
merely a second root credential and the whole arrangement is pointless.

```yaml
scrape_configs:
  - job_name: bmc
    scheme: https
    metrics_path: /metrics
    static_configs:
      - targets: ['<board>:443']
    basic_auth:
      username: metrics
      password_file: /etc/prometheus/bmc-metrics-token
    tls_config:
      # the daemon serves its own certificate; pin it with ca_file instead if
      # you have one
      insecure_skip_verify: true
```

Scrape the HTTPS port directly. Port 80 is the redirect server, and while a
scraper that follows redirects will land on the right place, it lands there
with an extra round trip per scrape.

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

Add `--features stubbed` to the clippy and test lines to cover the other half.
That feature replaces the GPIO and sysfs HAL with an in-memory simulation, and
is the only way to build or run this daemon with no Turing Pi board under it.
It had not compiled since before `v2.3.7` — nothing built it — so `Cargo CI`
now runs clippy and the tests with it on as well as off. Not `--all-features`:
the other feature is `vendored`, which statically links OpenSSL for
cross-builds and would have the job compile OpenSSL from source for nothing.

A `stubbed` build will also *run* on a workstation, which is how an
ALPN-shaped or middleware-shaped change gets checked against a real socket
rather than argued about. It wants a config file pointing at a certificate and
key of its own, a `www` directory, a writable `/var/lib/bmcd`, and
`/dev/input/event0` — the front-panel power button; without it the daemon
starts and only warns.

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

`actix-http` and `actix-service` became direct dependencies when the HTTPS
listener stopped going through `HttpServer::bind_openssl`. Both were already in
the tree beneath `actix-web`; the lockfile gained two lines and no crate.

Resolution is pinned to **Rust 1.85**. A plain `cargo update` pulls actix-web
4.15, `serde_with` 3.22, `time` 0.3.55 and the `icu_*` family, all of which now
require rustc 1.88 and none of which compile here, so they are held one minor
behind on purpose. Raising the toolchain is a decision about the Buildroot
toolchain, not about this repo — and it has to happen together with
`Cargo.lock`, `cargo_ci.yml` and `bmcd.hash`.

Three advisories are still open in the lockfile. One of them has since been
closed off in the daemon rather than in the dependency; the other two are
**declined**, each because closing it needs a major bump or a toolchain move:

| advisory | reachable here? | why it is still open |
|---|---|---|
| `h2` 0.3.27 — RUSTSEC-2026-0258, unbounded empty DATA frames | **No longer.** It was, and pre-authentication: `bind_openssl` advertised `h2` over ALPN, so a peer reached HTTP/2 framing before the auth middleware ran. The listener now offers `http/1.1` and nothing else, and is built from an `HttpService` with `.h1()`, so the HTTP/2 dispatcher is never constructed — see the row on it above | Still no fix at any version. The patch is in `h2 >= 0.4.16`, and every `actix-http` up to the newest (3.13.5) still pins `h2` 0.3.27, so only an actix-web 5 migration or an upstream backport takes the advisory off the list. The crate is compiled into the binary and unreachable; dropping actix-web's default `http2` feature would take it out of the graph entirely and is its own decision |
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

