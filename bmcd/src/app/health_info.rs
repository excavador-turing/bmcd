// Copyright 2023 Turing Machines
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
//! The condition of the BMC itself: how long it has been up, what it is
//! carrying, how much of its NAND is left and whether it knows what time it
//! is.
//!
//! Everything the daemon reports today is about the four compute modules or
//! about the board's peripherals. Nothing is about the board that runs the
//! daemon, which has 116 MB of RAM, a NAND with a countable number of spare
//! eraseblocks, and no battery on one of its two clocks.
use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// Where the kernel exposes process and system state.
const PROC: &str = "/proc";
/// Where the kernel exposes UBI devices.
const UBI_CLASS: &str = "/sys/class/ubi";
/// The one UBI device on this board.
const UBI_DEVICE: &str = "ubi0";
/// Where the kernel exposes real-time clocks.
const RTC_CLASS: &str = "/sys/class/rtc";
/// The tool that owns chrony's state.
const CHRONYC: &str = "chronyc";
/// What `chronyc tracking` calls a clock that is not disciplined.
const NOT_SYNCHRONISED: &str = "Not synchronised";

/// The one-, five- and fifteen-minute load averages.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Load {
    /// Whether `/proc/loadavg` could be read.
    pub present: bool,
    pub one_minute: Option<f64>,
    pub five_minutes: Option<f64>,
    pub fifteen_minutes: Option<f64>,
}

/// Memory, in bytes rather than the kibibytes `/proc/meminfo` speaks.
///
/// This is not an academic number on this board. It has 116 MB in total, and
/// a firmware image being uploaded into `/tmp` is competing with the daemon
/// for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Memory {
    /// Whether `/proc/meminfo` could be read.
    pub present: bool,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// `MemAvailable`: what a new allocation can actually get, reclaim
    /// included. Absent on kernels older than 3.14, which is why it is not
    /// derived from the other two.
    pub available_bytes: Option<u64>,
}

/// What is left of the NAND, as UBI counts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Nand {
    /// Whether the UBI device is there at all.
    pub present: bool,
    /// Logical eraseblocks on the device.
    pub total_eraseblocks: Option<u64>,
    /// Logical eraseblocks not yet handed to a volume. This is the number
    /// that says whether the next firmware upgrade has room.
    pub available_eraseblocks: Option<u64>,
    /// Physical eraseblocks the flash has retired.
    pub bad_eraseblocks: Option<u64>,
    /// Physical eraseblocks held back to replace bad ones. When this reaches
    /// zero the next bad block is a failure rather than a substitution.
    pub reserved_eraseblocks: Option<u64>,
    pub eraseblock_size_bytes: Option<u64>,
    /// `available_eraseblocks` in bytes, for callers that would otherwise
    /// multiply the two themselves.
    pub available_bytes: Option<u64>,
}

/// One real-time clock the kernel has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rtc {
    /// `rtc0`, `rtc1`.
    pub device: String,
    /// The driver's own name for it. Which of the two has a battery behind
    /// it is not something the kernel exposes, so it is not claimed here.
    pub name: Option<String>,
}

/// Whether the board knows what time it is, and how well.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Clock {
    /// Every RTC the kernel has. An empty list is a board with none, which
    /// is a board whose clock starts at the epoch on every cold boot.
    pub rtc: Vec<Rtc>,
    /// Whether the system clock is disciplined. `null` when chrony's state
    /// could not be read at all.
    pub synchronised: Option<bool>,
    /// What it is synchronised to, as chrony names it.
    pub source: Option<String>,
    pub stratum: Option<u32>,
    /// System clock minus true time: negative when the board is behind.
    pub offset_seconds: Option<f64>,
    /// How the three fields above were obtained, because it is not a file
    /// read like everything else here.
    pub measured_by: Option<String>,
}

/// The board's own condition.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Health {
    /// Seconds since the BMC booted, as `/proc/uptime` gives it. Not the
    /// uptime of any compute module.
    pub uptime_seconds: Option<f64>,
    pub load: Load,
    pub memory: Memory,
    pub nand: Nand,
    pub clock: Clock,
}

/// What `chronyc tracking` said, once parsed.
#[derive(Debug, Clone, PartialEq, Default)]
struct Tracking {
    source: Option<String>,
    stratum: Option<u32>,
    offset_seconds: Option<f64>,
    synchronised: Option<bool>,
}

/// Reads the board's condition. Every source is optional and every absent one
/// is reported as absent: an older kernel without `MemAvailable`, a board
/// without UBI, a board without an RTC and a board without chrony all answer
/// 200 with the fields they can fill.
pub async fn get_health() -> Health {
    read_health(
        Path::new(PROC),
        Path::new(UBI_CLASS),
        Path::new(RTC_CLASS),
        read_chrony().await,
    )
    .await
}

async fn read_health(
    proc: &Path,
    ubi_class: &Path,
    rtc_class: &Path,
    tracking: Option<Tracking>,
) -> Health {
    Health {
        uptime_seconds: read_uptime(proc).await,
        load: read_load(proc).await,
        memory: read_memory(proc).await,
        nand: read_nand(&ubi_class.join(UBI_DEVICE)).await,
        clock: Clock {
            rtc: read_rtcs(rtc_class).await,
            synchronised: tracking.as_ref().and_then(|t| t.synchronised),
            source: tracking.as_ref().and_then(|t| t.source.clone()),
            stratum: tracking.as_ref().and_then(|t| t.stratum),
            offset_seconds: tracking.as_ref().and_then(|t| t.offset_seconds),
            measured_by: tracking.is_some().then(|| format!("{} tracking", CHRONYC)),
        },
    }
}

/// `/proc/uptime` is "<seconds up> <seconds idle>". Only the first is ours;
/// the second is summed over cores and means something else.
async fn read_uptime(proc: &Path) -> Option<f64> {
    let uptime = tokio::fs::read_to_string(proc.join("uptime")).await.ok()?;
    uptime.split_whitespace().next()?.parse().ok()
}

/// `/proc/loadavg` is "<1m> <5m> <15m> <running>/<total> <last pid>".
async fn read_load(proc: &Path) -> Load {
    let Ok(loadavg) = tokio::fs::read_to_string(proc.join("loadavg")).await else {
        return Load {
            present: false,
            one_minute: None,
            five_minutes: None,
            fifteen_minutes: None,
        };
    };

    let mut fields = loadavg.split_whitespace();
    Load {
        present: true,
        one_minute: fields.next().and_then(|value| value.parse().ok()),
        five_minutes: fields.next().and_then(|value| value.parse().ok()),
        fifteen_minutes: fields.next().and_then(|value| value.parse().ok()),
    }
}

async fn read_memory(proc: &Path) -> Memory {
    let Ok(meminfo) = tokio::fs::read_to_string(proc.join("meminfo")).await else {
        return Memory {
            present: false,
            total_bytes: None,
            free_bytes: None,
            available_bytes: None,
        };
    };

    Memory {
        present: true,
        total_bytes: meminfo_bytes(&meminfo, "MemTotal"),
        free_bytes: meminfo_bytes(&meminfo, "MemFree"),
        available_bytes: meminfo_bytes(&meminfo, "MemAvailable"),
    }
}

/// One `/proc/meminfo` entry in bytes. The file's numbers are kibibytes and
/// say so on every line that is one; a line without the unit is a count and
/// is taken as it stands.
fn meminfo_bytes(meminfo: &str, key: &str) -> Option<u64> {
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))?;

    let mut fields = line.split_whitespace();
    let value: u64 = fields.next()?.parse().ok()?;
    match fields.next() {
        Some("kB") => Some(value * 1024),
        _ => Some(value),
    }
}

/// Reads UBI's own accounting of the NAND out of sysfs.
///
/// `ubinfo /dev/ubi0` prints the same four numbers -- 2040 logical
/// eraseblocks, 5 available, 0 bad, 40 reserved on this board -- but it reads
/// them through an ioctl on a device node and it is a separate binary from
/// mtd-utils that a smaller image may not carry. sysfs is the same
/// accounting, needs no fork on a 116 MB board, and is there whenever UBI is.
async fn read_nand(device_dir: &Path) -> Nand {
    if tokio::fs::metadata(device_dir).await.is_err() {
        return Nand {
            present: false,
            total_eraseblocks: None,
            available_eraseblocks: None,
            bad_eraseblocks: None,
            reserved_eraseblocks: None,
            eraseblock_size_bytes: None,
            available_bytes: None,
        };
    }

    let available_eraseblocks = read_attribute(device_dir, "avail_eraseblocks").await;
    let eraseblock_size_bytes = read_attribute(device_dir, "eraseblock_size").await;

    Nand {
        present: true,
        total_eraseblocks: read_attribute(device_dir, "total_eraseblocks").await,
        available_eraseblocks,
        bad_eraseblocks: read_attribute(device_dir, "bad_peb_count").await,
        reserved_eraseblocks: read_attribute(device_dir, "reserved_for_bad").await,
        eraseblock_size_bytes,
        available_bytes: available_eraseblocks
            .zip(eraseblock_size_bytes)
            .map(|(blocks, size): (u64, u64)| blocks * size),
    }
}

/// Every RTC the kernel registered, in device order.
async fn read_rtcs(rtc_class: &Path) -> Vec<Rtc> {
    let Ok(mut entries) = tokio::fs::read_dir(rtc_class).await else {
        return Vec::new();
    };

    let mut clocks = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let device = entry.file_name().to_string_lossy().into_owned();
        if !device.starts_with("rtc") {
            continue;
        }

        clocks.push(Rtc {
            name: read_attribute_string(&rtc_class.join(&device), "name").await,
            device,
        });
    }

    clocks.sort_by(|left, right| left.device.cmp(&right.device));
    clocks
}

/// Asks chrony what it is doing.
///
/// Unlike everything else in this module this is a fork and an exec. The
/// kernel's own view of whether the clock is disciplined lives behind
/// `adjtimex(2)`, which is not exposed by anything this daemon already
/// depends on, and chrony publishes its state over a unix socket in its own
/// binary protocol. `chronyc` speaks that protocol and is on the board.
/// A board without it, or with chronyd down, answers `null` for every field
/// this fills, which is how a caller can tell "not synchronised" from "we do
/// not know".
async fn read_chrony() -> Option<Tracking> {
    let output =
        tokio::task::spawn_blocking(|| Command::new(CHRONYC).arg("tracking").output()).await;

    let Ok(Ok(output)) = output else {
        return None;
    };
    if !output.status.success() {
        return None;
    }

    parse_tracking(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `chronyc tracking`. Its output is one `key : value` per line; only
/// four of them are of interest here, and a line whose value carries colons
/// of its own is not one of the four.
fn parse_tracking(output: &str) -> Option<Tracking> {
    let mut tracking = Tracking::default();
    let mut seen = false;

    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        seen = true;

        match key {
            "Reference ID" => tracking.source = parse_reference_id(value),
            "Stratum" => tracking.stratum = value.parse().ok(),
            "System time" => tracking.offset_seconds = parse_system_time(value),
            "Leap status" => tracking.synchronised = Some(value != NOT_SYNCHRONISED),
            _ => (),
        }
    }

    // Leap status and stratum disagree only when chrony has just lost its
    // source; a stratum of 0 is chrony saying it is not disciplining
    // anything, whatever the leap line says.
    if matches!(tracking.stratum, Some(0)) {
        tracking.synchronised = Some(false);
    }

    seen.then_some(tracking)
}

/// `C0A84D01 (192.168.77.1)` -- the name if chrony resolved one, the hex
/// reference id if it did not, and nothing for the all-zero id an
/// undisciplined chrony reports.
fn parse_reference_id(value: &str) -> Option<String> {
    let (id, name) = match value.split_once('(') {
        Some((id, name)) => (id.trim(), name.trim_end_matches(')').trim()),
        None => (value.trim(), ""),
    };

    if !name.is_empty() {
        return Some(name.to_string());
    }

    let unset = id.is_empty() || id.chars().all(|digit| digit == '0');
    (!unset).then(|| id.to_string())
}

/// `0.000003077 seconds slow of NTP time`. Reported as system clock minus
/// true time, so "slow" -- the board behind the world -- is negative.
fn parse_system_time(value: &str) -> Option<f64> {
    let mut fields = value.split_whitespace();
    let magnitude: f64 = fields.next()?.parse().ok()?;
    let direction = fields.find(|field| *field == "slow" || *field == "fast")?;

    Some(if direction == "slow" {
        -magnitude
    } else {
        magnitude
    })
}

/// Reads one sysfs attribute as trimmed text. A missing file, an unreadable
/// one and an empty one are all `None`.
async fn read_attribute_string(dir: &Path, attribute: &str) -> Option<String> {
    let value = tokio::fs::read_to_string(dir.join(attribute)).await.ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

async fn read_attribute<T: std::str::FromStr>(dir: &Path, attribute: &str) -> Option<T> {
    read_attribute_string(dir, attribute).await?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// What `chronyc tracking` prints. The stratum and the system-time offset
    /// are the board's; the reference address and the lines this parser does
    /// not read are a stand-in for the shape of the output.
    const CHRONY_TRACKING: &str = concat!(
        "Reference ID    : C0A84D01 (192.168.77.1)\n",
        "Stratum         : 3\n",
        "Ref time (UTC)  : Mon Sep 07 19:30:22 2026\n",
        "System time     : 0.000003077 seconds slow of NTP time\n",
        "Last offset     : +0.000000749 seconds\n",
        "RMS offset      : 0.000002000 seconds\n",
        "Frequency       : 8.294 ppm slow\n",
        "Residual freq   : +0.001 ppm\n",
        "Skew            : 0.284 ppm\n",
        "Root delay      : 0.002914095 seconds\n",
        "Root dispersion : 0.000771448 seconds\n",
        "Update interval : 64.4 seconds\n",
        "Leap status     : Normal\n",
    );

    /// A `/proc`, a `/sys/class/ubi` and a `/sys/class/rtc` for the board.
    /// The uptime, the NAND counts and the memory total are the board's own;
    /// the free and available memory, the load and the RTC driver names are
    /// shaped like it rather than read off it.
    fn fake_board() -> (tempdir::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempdir::TempDir::new("health").expect("tempdir");
        let proc = dir.path().join("proc");
        let ubi_class = dir.path().join("ubi");
        let rtc_class = dir.path().join("rtc");

        fs::create_dir_all(&proc).expect("create proc");
        fs::write(proc.join("uptime"), "172.43 640.71\n").expect("uptime");
        fs::write(proc.join("loadavg"), "0.08 0.03 0.01 1/85 1234\n").expect("loadavg");
        fs::write(
            proc.join("meminfo"),
            concat!(
                "MemTotal:         118784 kB\n",
                "MemFree:           20480 kB\n",
                "MemAvailable:      61440 kB\n",
                "Buffers:            2048 kB\n",
                "HugePages_Total:       0\n",
            ),
        )
        .expect("meminfo");

        let device = ubi_class.join(UBI_DEVICE);
        fs::create_dir_all(&device).expect("create ubi0");
        for (attribute, value) in [
            ("total_eraseblocks", "2040"),
            ("avail_eraseblocks", "5"),
            ("bad_peb_count", "0"),
            ("reserved_for_bad", "40"),
            ("eraseblock_size", "126976"),
        ] {
            fs::write(device.join(attribute), format!("{}\n", value)).expect("ubi attribute");
        }

        for (device, name) in [("rtc0", "sun6i-rtc"), ("rtc1", "pcf8563")] {
            let rtc = rtc_class.join(device);
            fs::create_dir_all(&rtc).expect("create rtc");
            fs::write(rtc.join("name"), format!("{}\n", name)).expect("rtc name");
        }

        (dir, proc, ubi_class, rtc_class)
    }

    #[tokio::test]
    async fn the_board_reports_its_own_condition() {
        let (_guard, proc, ubi_class, rtc_class) = fake_board();
        let health = read_health(
            &proc,
            &ubi_class,
            &rtc_class,
            parse_tracking(CHRONY_TRACKING),
        )
        .await;

        assert_eq!(health.uptime_seconds, Some(172.43));
        assert_eq!(health.load.one_minute, Some(0.08));
        assert_eq!(health.load.five_minutes, Some(0.03));
        assert_eq!(health.load.fifteen_minutes, Some(0.01));
        assert!(health.load.present);

        assert!(health.memory.present);
        assert_eq!(
            health.memory.total_bytes,
            Some(118784 * 1024),
            "meminfo speaks kibibytes and this speaks bytes"
        );
        assert_eq!(health.memory.free_bytes, Some(20480 * 1024));
        assert_eq!(health.memory.available_bytes, Some(61440 * 1024));
    }

    /// The four numbers `ubinfo /dev/ubi0` prints on this board, read out of
    /// sysfs instead.
    #[tokio::test]
    async fn the_nand_counts_match_what_ubinfo_prints() {
        let (_guard, _proc, ubi_class, _rtc_class) = fake_board();
        let nand = read_nand(&ubi_class.join(UBI_DEVICE)).await;

        assert!(nand.present);
        assert_eq!(nand.total_eraseblocks, Some(2040));
        assert_eq!(nand.available_eraseblocks, Some(5));
        assert_eq!(nand.bad_eraseblocks, Some(0));
        assert_eq!(nand.reserved_eraseblocks, Some(40));
        assert_eq!(nand.eraseblock_size_bytes, Some(126976));
        assert_eq!(
            nand.available_bytes,
            Some(5 * 126976),
            "five eraseblocks is what is left for the next upgrade"
        );
    }

    #[tokio::test]
    async fn both_clocks_are_listed_and_neither_is_called_the_battery_one() {
        let (_guard, _proc, _ubi_class, rtc_class) = fake_board();
        let clocks = read_rtcs(&rtc_class).await;

        assert_eq!(
            clocks
                .iter()
                .map(|clock| clock.device.as_str())
                .collect::<Vec<_>>(),
            vec!["rtc0", "rtc1"]
        );
        assert_eq!(clocks[0].name.as_deref(), Some("sun6i-rtc"));
    }

    #[tokio::test]
    async fn a_synchronised_clock_carries_its_source_and_offset() {
        let (_guard, proc, ubi_class, rtc_class) = fake_board();
        let clock = read_health(
            &proc,
            &ubi_class,
            &rtc_class,
            parse_tracking(CHRONY_TRACKING),
        )
        .await
        .clock;

        assert_eq!(clock.synchronised, Some(true));
        assert_eq!(clock.source.as_deref(), Some("192.168.77.1"));
        assert_eq!(clock.stratum, Some(3));
        assert_eq!(
            clock.offset_seconds,
            Some(-0.000003077),
            "slow of NTP time is a board that is behind"
        );
        assert_eq!(clock.measured_by.as_deref(), Some("chronyc tracking"));
    }

    /// chronyd running but disciplining nothing. Stratum 0 is chrony saying
    /// so even when the leap line has not caught up.
    #[test]
    fn an_undisciplined_clock_says_so() {
        let tracking = parse_tracking(concat!(
            "Reference ID    : 00000000 ()\n",
            "Stratum         : 0\n",
            "System time     : 0.000000000 seconds fast of NTP time\n",
            "Leap status     : Not synchronised\n",
        ))
        .expect("tracking");

        assert_eq!(tracking.synchronised, Some(false));
        assert_eq!(
            tracking.source, None,
            "an all-zero reference id is not a source"
        );
        assert_eq!(tracking.stratum, Some(0));
    }

    #[test]
    fn a_reference_id_without_a_name_is_the_id() {
        let tracking = parse_tracking(concat!(
            "Reference ID    : C0A84D01\n",
            "Stratum         : 2\n",
            "Leap status     : Normal\n",
        ))
        .expect("tracking");

        assert_eq!(tracking.source.as_deref(), Some("C0A84D01"));
        assert_eq!(tracking.synchronised, Some(true));
    }

    #[test]
    fn output_that_is_not_chronycs_is_not_a_reading() {
        assert_eq!(parse_tracking(""), None);
        assert_eq!(parse_tracking("506 Cannot talk to daemon\n"), None);
    }

    /// The board this endpoint has to survive: no UBI, no RTC, no chrony, and
    /// a kernel too old for `MemAvailable`. Every source says it is not there
    /// rather than answering with a zero that reads like a measurement.
    #[tokio::test]
    async fn a_board_that_can_tell_us_nothing_answers_with_nothing() {
        let dir = tempdir::TempDir::new("health").expect("tempdir");
        let empty = dir.path().join("absent");
        let health = read_health(&empty, &empty, &empty, None).await;

        assert_eq!(health.uptime_seconds, None);
        assert!(!health.load.present);
        assert_eq!(health.load.one_minute, None);
        assert!(!health.memory.present);
        assert!(!health.nand.present);
        assert_eq!(health.nand.available_bytes, None);
        assert!(health.clock.rtc.is_empty());
        assert_eq!(health.clock.synchronised, None);
        assert_eq!(
            health.clock.measured_by, None,
            "no chronyc means we do not know, not that the clock is wrong"
        );
    }

    /// A kernel from before `MemAvailable` existed. The two fields it does
    /// have are still reported; the third is not invented out of them.
    #[test]
    fn a_missing_meminfo_key_is_not_derived() {
        let meminfo = "MemTotal:         118784 kB\nMemFree:           20480 kB\n";

        assert_eq!(meminfo_bytes(meminfo, "MemTotal"), Some(118784 * 1024));
        assert_eq!(meminfo_bytes(meminfo, "MemAvailable"), None);
        assert_eq!(
            meminfo_bytes("HugePages_Total:       0\n", "HugePages_Total"),
            Some(0),
            "a line without a unit is a count, not kibibytes"
        );
    }
}
