# bmcd — `excavador` fork

> **This is a fork of [turing-machines/bmcd](https://github.com/turing-machines/bmcd).**
> `master` is upstream's `master`, commit for commit. **`hive` is the branch that
> gets built**: five commits on top of the `v2.3.7` tag. Everything below the fold
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

**Nothing in this section has run on hardware.** The three commits after
`27ec80f` are in no flashed image: the firmware's `hive` branch still pins
`27ec80f`, and the bump to `df1e8ec` sits on an open firmware pull request.
Read these as a diff with an argument behind it, not as behaviour.

| not yet proven | what the change does |
|---|---|
| **The About page stops saying `Build version: vundefined`** | The web UI reads the daemon version from a key named `build_version`; `get_about` only ever sent `bmcd_version`. It now sends the same value under both names — `bmcd_version` stays, it is the documented key of the legacy API. The value is bmcd's own crate version (`2.3.7`), not the firmware release. The UI's *other* version bug, the doubled `v` in `vv2.2.0-…`, is on the UI side and is untouched here |
| **The About page reports the Buildroot release instead of the firmware name** | `get_about` filled its `buildroot` field from `PRETTY_NAME` in `/etc/os-release`, which our firmware stamps with the Turing Pi release — so a board built on Buildroot 2025.02.17 called its "Buildroot release" `Turing Pi v2.2.0`. It now prefers a `BUILDROOT_VERSION` key and falls back to `PRETTY_NAME`. The key is written by the firmware's `post_build.sh` from Buildroot's own `BR2_VERSION`, and that too is still on the open firmware PR, so an image built today has only the fallback to offer. `get_system_information` is left alone: it is the deprecated duplicate of this call and its `buildroot` field has always been the pretty name |
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
| `info` | IP addresses and storage |
| `node_info` | per-node auxiliary info |
| `nodeinfo` | hard-coded zeros; there is no implementation behind it |
| `other` | the deprecated duplicate of `about` |
| `power` | per-node power state |
| `sdcard` | card size and free space |
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
`handle`), and `POST` with `opt=set&type=node_info` takes a JSON body. The
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
  cargo clippy --workspace --all-targets &&
  cargo test --workspace
'
```

The `rustup component add` is not optional: the `rust:` images ship neither
rustfmt nor clippy. `--workspace` is, for the same virtual-manifest reason as
above. This builds for the host, not for `armv7`; it is a correctness check, and
the real cross build is the firmware's.

**Expect exactly three clippy warnings on `hive`, and leave them alone.** They
are upstream's, inherited from the `v2.3.7` tag we branched from:

- `rand::thread_rng` is deprecated — twice, in the test helpers of
  `bmcd/src/app/upgrade_worker.rs` and `bmcd/src/utils/io.rs`.
- `needless_lifetimes` on `impl<'a, W> AsyncWrite for WriteMonitor<'a, W>` in
  `bmcd/src/utils/io.rs`.

Upstream fixed all three one commit past the tag, in `a15e8fc` on `master`,
which we did not take: its `needless_lifetimes` fix writes `impl<'_, W>`, and
`'_` is not something Rust accepts in a generics list. So adopting `master` is
not a free rebase, and we have not run a build to see how far it gets.

Note also that the inherited `Cargo CI` workflow has never run on this fork —
Actions are off — and it would fail if it did: it runs clippy with
`-- -D warnings`, which is what those three warnings are.

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

