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
//! `GET /metrics` in the Prometheus text exposition format.
//!
//! Everything here is already reachable over the legacy API. What a scrape
//! adds is a shape a time-series database can take without a translator in
//! between, which is what turns "the fan is at step 4" into "the fan has been
//! at step 4 for six hours and the temperature has not moved".
//!
//! The format is hand-written rather than taken from a metrics crate. It is
//! two comment lines and a sample per value, this daemon is cross-compiled
//! for armv7 into a firmware image that is at 78% of its flash slot, and the
//! smallest of the registry crates brings a client library, a registry, a
//! label-set encoder and their dependencies for text this module produces in
//! sixty lines. The cost of the choice is that nothing checks the names for
//! us; the tests assert the whole document, which is the reason they assert
//! the whole document.
//!
//! This endpoint is authenticated. It is mounted in `main` behind the same
//! `LinuxAuthenticator` that wraps `/api/bmc`, which accepts HTTP Basic, so a
//! scrape config authenticates with `basic_auth` and nothing else is needed.
//! There is an open finding about `/info` being served unauthenticated over
//! plain HTTP; a second unauthenticated surface -- one that reports the
//! board's serial-adjacent details, its firmware versions and its traffic
//! counters -- would be the same mistake twice.
use crate::app::bmc_application::BmcApplication;
use crate::app::cooling_device::CoolingDevice;
use crate::app::firmware_info::{get_firmware_slots, FirmwareSlots};
use crate::app::health_info::{get_health, Health};
use crate::app::switch_info::{get_switch_ports, PortKind, SwitchPort};
use crate::hal::NodeId;
use actix_web::{web, HttpResponse, Responder};
use std::fmt::Write;
use std::path::Path;

/// The content type of the Prometheus text exposition format. Version 0.0.4
/// is what every scraper in use understands.
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Where the kernel exposes thermal zones and cooling devices.
const THERMAL_CLASS: &str = "/sys/class/thermal";

/// One temperature the kernel can read.
///
/// This is the same source `opt=get&type=thermal` reads, and when that
/// endpoint lands on this branch these three lines become a call to
/// `app::thermal_info` rather than a second reader of the same directory.
#[derive(Debug, Clone, PartialEq)]
pub struct Sensor {
    pub name: String,
    pub temperature_c: Option<f64>,
}

/// One compute module's power state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePower {
    pub name: String,
    /// `None` when the daemon could not read the rail.
    pub on: Option<bool>,
    /// Seconds since the node was powered on, as the daemon has recorded it.
    pub power_on_seconds: Option<u64>,
}

/// Everything one scrape reports, gathered before any of it is formatted.
/// Collection touches the board; rendering is a pure function of this, which
/// is what makes the exposition testable against numbers read off hardware.
#[derive(Debug)]
pub struct Snapshot {
    pub daemon_version: String,
    pub sensors: Vec<Sensor>,
    pub cooling: Vec<CoolingDevice>,
    pub ports: Vec<SwitchPort>,
    pub nodes: Vec<NodePower>,
    pub health: Health,
    pub firmware: FirmwareSlots,
}

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("").route(web::get().to(handle_metrics)));
}

async fn handle_metrics(bmc: web::Data<BmcApplication>) -> impl Responder {
    let snapshot = collect(bmc.as_ref()).await;
    HttpResponse::Ok()
        .content_type(CONTENT_TYPE)
        .body(render(&snapshot))
}

/// Reads everything a scrape reports. Every source here already answers with
/// its own absences, so a board missing any of them produces a shorter
/// document rather than a failed scrape.
async fn collect(bmc: &BmcApplication) -> Snapshot {
    let node_infos = bmc.get_node_infos().await.unwrap_or_default();
    let mut nodes = Vec::with_capacity(node_infos.len());
    for (index, node) in [NodeId::Node1, NodeId::Node2, NodeId::Node3, NodeId::Node4]
        .into_iter()
        .enumerate()
    {
        nodes.push(NodePower {
            name: format!("node{}", index + 1),
            on: bmc.get_node_power(node).await.ok(),
            power_on_seconds: node_infos[index].power_on_time,
        });
    }

    // readdir order is arbitrary and `get_cooling_state` passes it through,
    // so the same board renders its cooling devices in a different order from
    // one scrape to the next. Prometheus does not mind; anyone diffing two
    // scrapes by hand does.
    let mut cooling = BmcApplication::get_cooling_devices()
        .await
        .unwrap_or_default();
    cooling.sort_by(|left, right| left.device.cmp(&right.device));

    Snapshot {
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        sensors: read_sensors(Path::new(THERMAL_CLASS)).await,
        cooling,
        ports: get_switch_ports().await,
        nodes,
        health: get_health().await,
        firmware: get_firmware_slots(super::legacy::firmware_version().await).await,
    }
}

/// Reads every `thermal_zone*` under a thermal class directory, in zone
/// order. Millidegrees become degrees to one decimal, the way
/// `opt=get&type=thermal` reports them; a zone that will not answer keeps its
/// name and loses its temperature rather than reporting a zero.
async fn read_sensors(thermal_class: &Path) -> Vec<Sensor> {
    let Ok(mut entries) = tokio::fs::read_dir(thermal_class).await else {
        return Vec::new();
    };

    let mut zones = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_name = entry.file_name().to_string_lossy().into_owned();
        let Some(index) = entry_name
            .strip_prefix("thermal_zone")
            .and_then(|index| index.parse::<u32>().ok())
        else {
            continue;
        };

        let zone = thermal_class.join(&entry_name);
        let name = read_attribute(&zone, "type")
            .await
            .unwrap_or_else(|| entry_name.clone());
        let temperature_c = read_attribute(&zone, "temp")
            .await
            .and_then(|temp| temp.parse::<i64>().ok())
            .map(millidegrees_to_degrees);

        zones.push((
            index,
            Sensor {
                name,
                temperature_c,
            },
        ));
    }

    // readdir order is arbitrary, and sorting by name puts thermal_zone10
    // before thermal_zone2.
    zones.sort_by_key(|(index, _)| *index);
    zones.into_iter().map(|(_, sensor)| sensor).collect()
}

/// 52539 is 52.5. Rounded rather than truncated, so 52999 is 53.0.
fn millidegrees_to_degrees(millidegrees: i64) -> f64 {
    (millidegrees as f64 / 100.0).round() / 10.0
}

async fn read_attribute(dir: &Path, attribute: &str) -> Option<String> {
    let value = tokio::fs::read_to_string(dir.join(attribute)).await.ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// One line of the exposition: a label set and a value.
#[derive(Debug, Clone, PartialEq)]
struct Sample {
    labels: Vec<(&'static str, String)>,
    value: f64,
}

impl Sample {
    fn new(labels: Vec<(&'static str, String)>, value: f64) -> Self {
        Sample { labels, value }
    }

    /// A metric with no labels at all.
    fn bare(value: f64) -> Self {
        Sample {
            labels: Vec::new(),
            value,
        }
    }
}

/// A single optional reading, as a family's worth of samples: absent means no
/// sample, never a zero. A gap in a time series is a gap; a zero is a claim.
fn optional(value: Option<impl Into<f64>>) -> Vec<Sample> {
    value
        .map(|value| vec![Sample::bare(value.into())])
        .unwrap_or_default()
}

/// Writes one metric family. A family with no samples is not written at all,
/// header included -- an exporter that emits a `# TYPE` with nothing under it
/// says the metric exists and has no value, which is not what "this board
/// cannot measure that" means.
fn family(out: &mut String, name: &str, kind: &str, help: &str, samples: &[Sample]) {
    if samples.is_empty() {
        return;
    }

    let _ = writeln!(out, "# HELP {} {}", name, help);
    let _ = writeln!(out, "# TYPE {} {}", name, kind);
    for sample in samples {
        out.push_str(name);
        if !sample.labels.is_empty() {
            let labels = sample
                .labels
                .iter()
                .map(|(key, value)| format!("{}=\"{}\"", key, escape(value)))
                .collect::<Vec<_>>()
                .join(",");
            let _ = write!(out, "{{{}}}", labels);
        }
        let _ = writeln!(out, " {}", format_value(sample.value));
    }
    out.push('\n');
}

/// Backslash, double quote and newline are the three characters the text
/// format escapes in a label value. Everything reaching this comes off the
/// board -- a UBI volume name, a cooling device the kernel named -- so it is
/// escaped rather than trusted to be tame.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Whole numbers are written without a decimal point: byte counters and
/// eraseblock counts are integers and reading them as `37019648` rather than
/// `37019648.0` is what anyone looking at the raw scrape expects. Rust's
/// float formatting never uses an exponent, so a microsecond offset comes out
/// as `-0.000003077` and stays parseable by every scraper.
fn format_value(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{}", value)
    }
}

fn labels(pairs: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
    pairs
        .iter()
        .map(|(key, value)| (*key, value.to_string()))
        .collect()
}

/// Renders a snapshot. Pure: the same snapshot always produces the same
/// bytes, which is what lets the tests below pin the whole document.
fn render(snapshot: &Snapshot) -> String {
    let mut out = String::new();

    family(
        &mut out,
        "bmcd_build_info",
        "gauge",
        "Version of the daemon that produced these metrics.",
        &[Sample::new(
            labels(&[("version", snapshot.daemon_version.as_str())]),
            1.0,
        )],
    );

    render_thermal(&mut out, snapshot);
    render_switch(&mut out, snapshot);
    render_nodes(&mut out, snapshot);
    render_health(&mut out, &snapshot.health);
    render_firmware(&mut out, &snapshot.firmware);

    out
}

fn render_thermal(out: &mut String, snapshot: &Snapshot) {
    family(
        out,
        "bmcd_temperature_celsius",
        "gauge",
        "Temperature reported by a kernel thermal zone.",
        &snapshot
            .sensors
            .iter()
            .filter_map(|sensor| {
                sensor.temperature_c.map(|celsius| {
                    Sample::new(labels(&[("sensor", sensor.name.as_str())]), celsius)
                })
            })
            .collect::<Vec<_>>(),
    );

    family(
        out,
        "bmcd_cooling_state",
        "gauge",
        "Step a cooling device is currently at.",
        &snapshot
            .cooling
            .iter()
            .map(|device| {
                Sample::new(
                    labels(&[("device", device.device.as_str())]),
                    device.speed as f64,
                )
            })
            .collect::<Vec<_>>(),
    );

    family(
        out,
        "bmcd_cooling_state_max",
        "gauge",
        "Highest step a cooling device accepts.",
        &snapshot
            .cooling
            .iter()
            .map(|device| {
                Sample::new(
                    labels(&[("device", device.device.as_str())]),
                    device.max_speed as f64,
                )
            })
            .collect::<Vec<_>>(),
    );
}

fn render_switch(out: &mut String, snapshot: &Snapshot) {
    let ports = &snapshot.ports;

    family(
        out,
        "bmcd_switch_port_present",
        "gauge",
        "Whether the kernel has a netdev for this switch port.",
        &ports
            .iter()
            .map(|port| {
                Sample::new(
                    labels(&[
                        ("port", port.name.as_str()),
                        (
                            "kind",
                            match port.kind {
                                PortKind::Node => "node",
                                PortKind::Uplink => "uplink",
                            },
                        ),
                    ]),
                    u8::from(port.present).into(),
                )
            })
            .collect::<Vec<_>>(),
    );

    family(
        out,
        "bmcd_switch_port_link",
        "gauge",
        "Whether a switch port has carrier.",
        &port_samples(ports, |port| port.link.map(|link| u8::from(link).into())),
    );

    family(
        out,
        "bmcd_switch_port_speed_bits_per_second",
        "gauge",
        "Negotiated line rate of a switch port.",
        &port_samples(ports, |port| {
            port.speed_mbps.map(|speed| f64::from(speed) * 1e6)
        }),
    );

    family(
        out,
        "bmcd_switch_port_rx_bytes_total",
        "counter",
        "Bytes received on a switch port.",
        &port_samples(ports, |port| port.rx_bytes.map(|count| count as f64)),
    );

    family(
        out,
        "bmcd_switch_port_tx_bytes_total",
        "counter",
        "Bytes transmitted on a switch port.",
        &port_samples(ports, |port| port.tx_bytes.map(|count| count as f64)),
    );

    family(
        out,
        "bmcd_switch_port_rx_errors_total",
        "counter",
        "Receive errors on a switch port.",
        &port_samples(ports, |port| port.rx_errors.map(|count| count as f64)),
    );

    family(
        out,
        "bmcd_switch_port_tx_errors_total",
        "counter",
        "Transmit errors on a switch port.",
        &port_samples(ports, |port| port.tx_errors.map(|count| count as f64)),
    );
}

fn port_samples(ports: &[SwitchPort], read: impl Fn(&SwitchPort) -> Option<f64>) -> Vec<Sample> {
    ports
        .iter()
        .filter_map(|port| {
            read(port).map(|value| Sample::new(labels(&[("port", port.name.as_str())]), value))
        })
        .collect()
}

fn render_nodes(out: &mut String, snapshot: &Snapshot) {
    family(
        out,
        "bmcd_node_power_state",
        "gauge",
        "Whether a compute module is powered on.",
        &snapshot
            .nodes
            .iter()
            .filter_map(|node| {
                node.on.map(|on| {
                    Sample::new(labels(&[("node", node.name.as_str())]), u8::from(on).into())
                })
            })
            .collect::<Vec<_>>(),
    );

    family(
        out,
        "bmcd_node_power_on_seconds",
        "gauge",
        "Seconds since a compute module was powered on.",
        &snapshot
            .nodes
            .iter()
            .filter_map(|node| {
                node.power_on_seconds.map(|seconds| {
                    Sample::new(labels(&[("node", node.name.as_str())]), seconds as f64)
                })
            })
            .collect::<Vec<_>>(),
    );
}

fn render_health(out: &mut String, health: &Health) {
    family(
        out,
        "bmcd_uptime_seconds",
        "gauge",
        "Seconds since the BMC booted.",
        &optional(health.uptime_seconds),
    );

    for (name, help, value) in [
        (
            "bmcd_load1",
            "1-minute load average of the BMC.",
            health.load.one_minute,
        ),
        (
            "bmcd_load5",
            "5-minute load average of the BMC.",
            health.load.five_minutes,
        ),
        (
            "bmcd_load15",
            "15-minute load average of the BMC.",
            health.load.fifteen_minutes,
        ),
    ] {
        family(out, name, "gauge", help, &optional(value));
    }

    for (name, help, value) in [
        (
            "bmcd_memory_total_bytes",
            "Total memory of the BMC.",
            health.memory.total_bytes,
        ),
        (
            "bmcd_memory_free_bytes",
            "Free memory of the BMC.",
            health.memory.free_bytes,
        ),
        (
            "bmcd_memory_available_bytes",
            "Memory available to a new allocation on the BMC, reclaim included.",
            health.memory.available_bytes,
        ),
    ] {
        family(out, name, "gauge", help, &optional_u64(value));
    }

    let nand = &health.nand;
    family(
        out,
        "bmcd_nand_eraseblocks",
        "gauge",
        "Eraseblocks of the BMC's NAND, by what UBI counts them as.",
        &[
            ("total", nand.total_eraseblocks),
            ("available", nand.available_eraseblocks),
            ("bad", nand.bad_eraseblocks),
            ("reserved", nand.reserved_eraseblocks),
        ]
        .into_iter()
        .filter_map(|(state, count)| {
            count.map(|count| Sample::new(labels(&[("state", state)]), count as f64))
        })
        .collect::<Vec<_>>(),
    );

    family(
        out,
        "bmcd_nand_eraseblock_size_bytes",
        "gauge",
        "Size of one logical eraseblock of the BMC's NAND.",
        &optional_u64(nand.eraseblock_size_bytes),
    );

    family(
        out,
        "bmcd_nand_available_bytes",
        "gauge",
        "Unallocated space on the BMC's NAND.",
        &optional_u64(nand.available_bytes),
    );

    let clock = &health.clock;
    family(
        out,
        "bmcd_rtc_present",
        "gauge",
        "Whether the BMC has a real-time clock at all.",
        &[Sample::bare(u8::from(!clock.rtc.is_empty()).into())],
    );

    family(
        out,
        "bmcd_rtc_info",
        "gauge",
        "A real-time clock the kernel registered on the BMC.",
        &clock
            .rtc
            .iter()
            .map(|rtc| {
                Sample::new(
                    labels(&[
                        ("device", rtc.device.as_str()),
                        ("name", rtc.name.as_deref().unwrap_or_default()),
                    ]),
                    1.0,
                )
            })
            .collect::<Vec<_>>(),
    );

    family(
        out,
        "bmcd_clock_synchronised",
        "gauge",
        "Whether the BMC's system clock is disciplined. Absent when that cannot be determined.",
        &optional(clock.synchronised.map(u8::from)),
    );

    family(
        out,
        "bmcd_clock_stratum",
        "gauge",
        "Stratum of the time source the BMC is synchronised to.",
        &optional(clock.stratum),
    );

    family(
        out,
        "bmcd_clock_offset_seconds",
        "gauge",
        "The BMC's system clock minus true time; negative when the board is behind.",
        &optional(clock.offset_seconds),
    );
}

fn render_firmware(out: &mut String, firmware: &FirmwareSlots) {
    let slots = [
        ("running", &firmware.running),
        ("rollback", &firmware.rollback),
    ];

    family(
        out,
        "bmcd_firmware_slot_info",
        "gauge",
        "A firmware slot on the BMC's NAND. The rollback slot is not mounted, so it has no version.",
        &slots
            .into_iter()
            .filter_map(|(role, slot)| {
                slot.as_ref().map(|slot| {
                    Sample::new(
                        labels(&[
                            ("slot", role),
                            ("volume", slot.volume.as_str()),
                            ("version", slot.version.as_deref().unwrap_or_default()),
                        ]),
                        1.0,
                    )
                })
            })
            .collect::<Vec<_>>(),
    );

    family(
        out,
        "bmcd_firmware_slot_size_bytes",
        "gauge",
        "Size of the firmware in a slot.",
        &slots
            .into_iter()
            .filter_map(|(role, slot)| {
                slot.as_ref().and_then(|slot| {
                    slot.size_bytes.map(|size| {
                        Sample::new(
                            labels(&[("slot", role), ("volume", slot.volume.as_str())]),
                            size as f64,
                        )
                    })
                })
            })
            .collect::<Vec<_>>(),
    );

    family(
        out,
        "bmcd_firmware_update_staged",
        "gauge",
        "Whether a firmware update is staged for the next boot. Absent when the U-Boot environment cannot be read.",
        &optional(firmware.update_staged.map(u8::from)),
    );
}

/// `u64` has no lossless `Into<f64>`, and every value this is used for -- a
/// byte count, an eraseblock count -- is far below the point where the cast
/// loses anything.
fn optional_u64(value: Option<u64>) -> Vec<Sample> {
    value
        .map(|value| vec![Sample::bare(value as f64)])
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::firmware_info::Slot;
    use crate::app::health_info::{Clock, Load, Memory, Nand, Rtc};

    fn port(name: &str, kind: PortKind, up: bool) -> SwitchPort {
        SwitchPort {
            name: name.to_string(),
            kind,
            present: true,
            link: Some(up),
            operstate: Some(if up { "up" } else { "lowerlayerdown" }.to_string()),
            speed_mbps: up.then_some(1000),
            duplex: up.then(|| "full".to_string()),
            rx_bytes: Some(1234),
            tx_bytes: Some(5678),
            rx_errors: Some(0),
            tx_errors: Some(0),
        }
    }

    /// A board answering everything. The SoC temperature and the fan step,
    /// the six switch ports, the NAND counts, the memory total, node 1's
    /// power-on time and both firmware volumes are numbers read off the
    /// board; the rest is shaped like it.
    fn a_healthy_board() -> Snapshot {
        Snapshot {
            daemon_version: "2.3.7".to_string(),
            sensors: vec![Sensor {
                name: "bmc-thermal".to_string(),
                temperature_c: Some(52.5),
            }],
            cooling: vec![CoolingDevice {
                device: "system fan".to_string(),
                speed: 4,
                max_speed: 6,
            }],
            ports: vec![
                port("node1", PortKind::Node, true),
                port("node2", PortKind::Node, true),
                port("node3", PortKind::Node, true),
                port("node4", PortKind::Node, true),
                port("ge0", PortKind::Uplink, true),
                port("ge1", PortKind::Uplink, false),
            ],
            nodes: vec![
                NodePower {
                    name: "node1".to_string(),
                    on: Some(true),
                    power_on_seconds: Some(52418),
                },
                NodePower {
                    name: "node2".to_string(),
                    on: Some(true),
                    power_on_seconds: Some(52401),
                },
                NodePower {
                    name: "node3".to_string(),
                    on: Some(false),
                    power_on_seconds: None,
                },
                NodePower {
                    name: "node4".to_string(),
                    on: None,
                    power_on_seconds: None,
                },
            ],
            health: Health {
                uptime_seconds: Some(172.43),
                load: Load {
                    present: true,
                    one_minute: Some(0.08),
                    five_minutes: Some(0.03),
                    fifteen_minutes: Some(0.01),
                },
                memory: Memory {
                    present: true,
                    total_bytes: Some(121634816),
                    free_bytes: Some(20971520),
                    available_bytes: Some(62914560),
                },
                nand: Nand {
                    present: true,
                    total_eraseblocks: Some(2040),
                    available_eraseblocks: Some(5),
                    bad_eraseblocks: Some(0),
                    reserved_eraseblocks: Some(40),
                    eraseblock_size_bytes: Some(126976),
                    available_bytes: Some(634880),
                },
                clock: Clock {
                    rtc: vec![
                        Rtc {
                            device: "rtc0".to_string(),
                            name: Some("sun6i-rtc".to_string()),
                        },
                        Rtc {
                            device: "rtc1".to_string(),
                            name: Some("pcf8563".to_string()),
                        },
                    ],
                    synchronised: Some(true),
                    source: Some("192.168.77.1".to_string()),
                    stratum: Some(3),
                    offset_seconds: Some(-0.000003077),
                    measured_by: Some("chronyc tracking".to_string()),
                },
            },
            firmware: FirmwareSlots {
                present: true,
                running: Some(Slot {
                    volume: "rootfs".to_string(),
                    volume_id: 1,
                    size_bytes: Some(37019648),
                    version: Some("v2.2.0-unstable-hive.5".to_string()),
                }),
                rollback: Some(Slot {
                    volume: "rootfs_prev".to_string(),
                    volume_id: 3,
                    size_bytes: Some(37011456),
                    version: None,
                }),
                update_staged: Some(false),
                nextboot: None,
                last_promotion: None,
            },
        }
    }

    /// A board that can answer none of it: no thermal zone, no switch driver,
    /// no UBI, no RTC, no chrony. What is left is the two things that are
    /// always true -- which daemon answered, and that there is no RTC.
    fn a_silent_board() -> Snapshot {
        Snapshot {
            daemon_version: "2.3.7".to_string(),
            sensors: Vec::new(),
            cooling: Vec::new(),
            ports: Vec::new(),
            nodes: vec![NodePower {
                name: "node1".to_string(),
                on: None,
                power_on_seconds: None,
            }],
            health: Health {
                uptime_seconds: None,
                load: Load {
                    present: false,
                    one_minute: None,
                    five_minutes: None,
                    fifteen_minutes: None,
                },
                memory: Memory {
                    present: false,
                    total_bytes: None,
                    free_bytes: None,
                    available_bytes: None,
                },
                nand: Nand {
                    present: false,
                    total_eraseblocks: None,
                    available_eraseblocks: None,
                    bad_eraseblocks: None,
                    reserved_eraseblocks: None,
                    eraseblock_size_bytes: None,
                    available_bytes: None,
                },
                clock: Clock {
                    rtc: Vec::new(),
                    synchronised: None,
                    source: None,
                    stratum: None,
                    offset_seconds: None,
                    measured_by: None,
                },
            },
            firmware: FirmwareSlots {
                present: false,
                running: None,
                rollback: None,
                update_staged: None,
                nextboot: None,
                last_promotion: None,
            },
        }
    }

    /// Zones are read in index order, not readdir order and not text order:
    /// `thermal_zone10` sorts before `thermal_zone2` as a string. A zone that
    /// will not answer a read keeps its name and loses its temperature rather
    /// than reporting a zero.
    #[tokio::test]
    async fn thermal_zones_are_read_in_index_order() {
        let dir = tempdir::TempDir::new("thermal").expect("tempdir");
        let root = dir.path();

        for (zone, kind, temp) in [
            ("thermal_zone0", "bmc-thermal", Some("52539")),
            ("thermal_zone2", "cpu-thermal", Some("48120")),
            ("thermal_zone10", "unreadable", None),
        ] {
            let path = root.join(zone);
            std::fs::create_dir_all(&path).expect("create zone");
            std::fs::write(path.join("type"), format!("{}\n", kind)).expect("type");
            if let Some(temp) = temp {
                std::fs::write(path.join("temp"), format!("{}\n", temp)).expect("temp");
            }
        }

        let sensors = read_sensors(root).await;
        assert_eq!(
            sensors,
            vec![
                Sensor {
                    name: "bmc-thermal".to_string(),
                    temperature_c: Some(52.5),
                },
                Sensor {
                    name: "cpu-thermal".to_string(),
                    temperature_c: Some(48.1),
                },
                Sensor {
                    name: "unreadable".to_string(),
                    temperature_c: None,
                },
            ]
        );
    }

    /// A board with no thermal zone at all -- every image older than the one
    /// that describes the sensor, and every v2.4 board.
    #[tokio::test]
    async fn a_board_with_no_thermal_zones_reads_as_empty() {
        let dir = tempdir::TempDir::new("thermal").expect("tempdir");
        assert_eq!(read_sensors(&dir.path().join("absent")).await, Vec::new());
    }

    /// The whole document, asserted byte for byte. Nothing else checks the
    /// metric names, the units or the `# HELP`/`# TYPE` lines -- there is no
    /// client library here to do it -- so this test is the contract.
    #[test]
    fn a_healthy_board_renders_the_whole_exposition() {
        assert_eq!(
            render(&a_healthy_board()),
            concat!(
                "# HELP bmcd_build_info Version of the daemon that produced these metrics.\n",
                "# TYPE bmcd_build_info gauge\n",
                "bmcd_build_info{version=\"2.3.7\"} 1\n",
                "\n",
                "# HELP bmcd_temperature_celsius Temperature reported by a kernel thermal zone.\n",
                "# TYPE bmcd_temperature_celsius gauge\n",
                "bmcd_temperature_celsius{sensor=\"bmc-thermal\"} 52.5\n",
                "\n",
                "# HELP bmcd_cooling_state Step a cooling device is currently at.\n",
                "# TYPE bmcd_cooling_state gauge\n",
                "bmcd_cooling_state{device=\"system fan\"} 4\n",
                "\n",
                "# HELP bmcd_cooling_state_max Highest step a cooling device accepts.\n",
                "# TYPE bmcd_cooling_state_max gauge\n",
                "bmcd_cooling_state_max{device=\"system fan\"} 6\n",
                "\n",
                "# HELP bmcd_switch_port_present Whether the kernel has a netdev for this switch port.\n",
                "# TYPE bmcd_switch_port_present gauge\n",
                "bmcd_switch_port_present{port=\"node1\",kind=\"node\"} 1\n",
                "bmcd_switch_port_present{port=\"node2\",kind=\"node\"} 1\n",
                "bmcd_switch_port_present{port=\"node3\",kind=\"node\"} 1\n",
                "bmcd_switch_port_present{port=\"node4\",kind=\"node\"} 1\n",
                "bmcd_switch_port_present{port=\"ge0\",kind=\"uplink\"} 1\n",
                "bmcd_switch_port_present{port=\"ge1\",kind=\"uplink\"} 1\n",
                "\n",
                "# HELP bmcd_switch_port_link Whether a switch port has carrier.\n",
                "# TYPE bmcd_switch_port_link gauge\n",
                "bmcd_switch_port_link{port=\"node1\"} 1\n",
                "bmcd_switch_port_link{port=\"node2\"} 1\n",
                "bmcd_switch_port_link{port=\"node3\"} 1\n",
                "bmcd_switch_port_link{port=\"node4\"} 1\n",
                "bmcd_switch_port_link{port=\"ge0\"} 1\n",
                "bmcd_switch_port_link{port=\"ge1\"} 0\n",
                "\n",
                "# HELP bmcd_switch_port_speed_bits_per_second Negotiated line rate of a switch port.\n",
                "# TYPE bmcd_switch_port_speed_bits_per_second gauge\n",
                "bmcd_switch_port_speed_bits_per_second{port=\"node1\"} 1000000000\n",
                "bmcd_switch_port_speed_bits_per_second{port=\"node2\"} 1000000000\n",
                "bmcd_switch_port_speed_bits_per_second{port=\"node3\"} 1000000000\n",
                "bmcd_switch_port_speed_bits_per_second{port=\"node4\"} 1000000000\n",
                "bmcd_switch_port_speed_bits_per_second{port=\"ge0\"} 1000000000\n",
                "\n",
                "# HELP bmcd_switch_port_rx_bytes_total Bytes received on a switch port.\n",
                "# TYPE bmcd_switch_port_rx_bytes_total counter\n",
                "bmcd_switch_port_rx_bytes_total{port=\"node1\"} 1234\n",
                "bmcd_switch_port_rx_bytes_total{port=\"node2\"} 1234\n",
                "bmcd_switch_port_rx_bytes_total{port=\"node3\"} 1234\n",
                "bmcd_switch_port_rx_bytes_total{port=\"node4\"} 1234\n",
                "bmcd_switch_port_rx_bytes_total{port=\"ge0\"} 1234\n",
                "bmcd_switch_port_rx_bytes_total{port=\"ge1\"} 1234\n",
                "\n",
                "# HELP bmcd_switch_port_tx_bytes_total Bytes transmitted on a switch port.\n",
                "# TYPE bmcd_switch_port_tx_bytes_total counter\n",
                "bmcd_switch_port_tx_bytes_total{port=\"node1\"} 5678\n",
                "bmcd_switch_port_tx_bytes_total{port=\"node2\"} 5678\n",
                "bmcd_switch_port_tx_bytes_total{port=\"node3\"} 5678\n",
                "bmcd_switch_port_tx_bytes_total{port=\"node4\"} 5678\n",
                "bmcd_switch_port_tx_bytes_total{port=\"ge0\"} 5678\n",
                "bmcd_switch_port_tx_bytes_total{port=\"ge1\"} 5678\n",
                "\n",
                "# HELP bmcd_switch_port_rx_errors_total Receive errors on a switch port.\n",
                "# TYPE bmcd_switch_port_rx_errors_total counter\n",
                "bmcd_switch_port_rx_errors_total{port=\"node1\"} 0\n",
                "bmcd_switch_port_rx_errors_total{port=\"node2\"} 0\n",
                "bmcd_switch_port_rx_errors_total{port=\"node3\"} 0\n",
                "bmcd_switch_port_rx_errors_total{port=\"node4\"} 0\n",
                "bmcd_switch_port_rx_errors_total{port=\"ge0\"} 0\n",
                "bmcd_switch_port_rx_errors_total{port=\"ge1\"} 0\n",
                "\n",
                "# HELP bmcd_switch_port_tx_errors_total Transmit errors on a switch port.\n",
                "# TYPE bmcd_switch_port_tx_errors_total counter\n",
                "bmcd_switch_port_tx_errors_total{port=\"node1\"} 0\n",
                "bmcd_switch_port_tx_errors_total{port=\"node2\"} 0\n",
                "bmcd_switch_port_tx_errors_total{port=\"node3\"} 0\n",
                "bmcd_switch_port_tx_errors_total{port=\"node4\"} 0\n",
                "bmcd_switch_port_tx_errors_total{port=\"ge0\"} 0\n",
                "bmcd_switch_port_tx_errors_total{port=\"ge1\"} 0\n",
                "\n",
                "# HELP bmcd_node_power_state Whether a compute module is powered on.\n",
                "# TYPE bmcd_node_power_state gauge\n",
                "bmcd_node_power_state{node=\"node1\"} 1\n",
                "bmcd_node_power_state{node=\"node2\"} 1\n",
                "bmcd_node_power_state{node=\"node3\"} 0\n",
                "\n",
                "# HELP bmcd_node_power_on_seconds Seconds since a compute module was powered on.\n",
                "# TYPE bmcd_node_power_on_seconds gauge\n",
                "bmcd_node_power_on_seconds{node=\"node1\"} 52418\n",
                "bmcd_node_power_on_seconds{node=\"node2\"} 52401\n",
                "\n",
                "# HELP bmcd_uptime_seconds Seconds since the BMC booted.\n",
                "# TYPE bmcd_uptime_seconds gauge\n",
                "bmcd_uptime_seconds 172.43\n",
                "\n",
                "# HELP bmcd_load1 1-minute load average of the BMC.\n",
                "# TYPE bmcd_load1 gauge\n",
                "bmcd_load1 0.08\n",
                "\n",
                "# HELP bmcd_load5 5-minute load average of the BMC.\n",
                "# TYPE bmcd_load5 gauge\n",
                "bmcd_load5 0.03\n",
                "\n",
                "# HELP bmcd_load15 15-minute load average of the BMC.\n",
                "# TYPE bmcd_load15 gauge\n",
                "bmcd_load15 0.01\n",
                "\n",
                "# HELP bmcd_memory_total_bytes Total memory of the BMC.\n",
                "# TYPE bmcd_memory_total_bytes gauge\n",
                "bmcd_memory_total_bytes 121634816\n",
                "\n",
                "# HELP bmcd_memory_free_bytes Free memory of the BMC.\n",
                "# TYPE bmcd_memory_free_bytes gauge\n",
                "bmcd_memory_free_bytes 20971520\n",
                "\n",
                "# HELP bmcd_memory_available_bytes Memory available to a new allocation on the BMC, reclaim included.\n",
                "# TYPE bmcd_memory_available_bytes gauge\n",
                "bmcd_memory_available_bytes 62914560\n",
                "\n",
                "# HELP bmcd_nand_eraseblocks Eraseblocks of the BMC's NAND, by what UBI counts them as.\n",
                "# TYPE bmcd_nand_eraseblocks gauge\n",
                "bmcd_nand_eraseblocks{state=\"total\"} 2040\n",
                "bmcd_nand_eraseblocks{state=\"available\"} 5\n",
                "bmcd_nand_eraseblocks{state=\"bad\"} 0\n",
                "bmcd_nand_eraseblocks{state=\"reserved\"} 40\n",
                "\n",
                "# HELP bmcd_nand_eraseblock_size_bytes Size of one logical eraseblock of the BMC's NAND.\n",
                "# TYPE bmcd_nand_eraseblock_size_bytes gauge\n",
                "bmcd_nand_eraseblock_size_bytes 126976\n",
                "\n",
                "# HELP bmcd_nand_available_bytes Unallocated space on the BMC's NAND.\n",
                "# TYPE bmcd_nand_available_bytes gauge\n",
                "bmcd_nand_available_bytes 634880\n",
                "\n",
                "# HELP bmcd_rtc_present Whether the BMC has a real-time clock at all.\n",
                "# TYPE bmcd_rtc_present gauge\n",
                "bmcd_rtc_present 1\n",
                "\n",
                "# HELP bmcd_rtc_info A real-time clock the kernel registered on the BMC.\n",
                "# TYPE bmcd_rtc_info gauge\n",
                "bmcd_rtc_info{device=\"rtc0\",name=\"sun6i-rtc\"} 1\n",
                "bmcd_rtc_info{device=\"rtc1\",name=\"pcf8563\"} 1\n",
                "\n",
                "# HELP bmcd_clock_synchronised Whether the BMC's system clock is disciplined. Absent when that cannot be determined.\n",
                "# TYPE bmcd_clock_synchronised gauge\n",
                "bmcd_clock_synchronised 1\n",
                "\n",
                "# HELP bmcd_clock_stratum Stratum of the time source the BMC is synchronised to.\n",
                "# TYPE bmcd_clock_stratum gauge\n",
                "bmcd_clock_stratum 3\n",
                "\n",
                "# HELP bmcd_clock_offset_seconds The BMC's system clock minus true time; negative when the board is behind.\n",
                "# TYPE bmcd_clock_offset_seconds gauge\n",
                "bmcd_clock_offset_seconds -0.000003077\n",
                "\n",
                "# HELP bmcd_firmware_slot_info A firmware slot on the BMC's NAND. The rollback slot is not mounted, so it has no version.\n",
                "# TYPE bmcd_firmware_slot_info gauge\n",
                "bmcd_firmware_slot_info{slot=\"running\",volume=\"rootfs\",version=\"v2.2.0-unstable-hive.5\"} 1\n",
                "bmcd_firmware_slot_info{slot=\"rollback\",volume=\"rootfs_prev\",version=\"\"} 1\n",
                "\n",
                "# HELP bmcd_firmware_slot_size_bytes Size of the firmware in a slot.\n",
                "# TYPE bmcd_firmware_slot_size_bytes gauge\n",
                "bmcd_firmware_slot_size_bytes{slot=\"running\",volume=\"rootfs\"} 37019648\n",
                "bmcd_firmware_slot_size_bytes{slot=\"rollback\",volume=\"rootfs_prev\"} 37011456\n",
                "\n",
                "# HELP bmcd_firmware_update_staged Whether a firmware update is staged for the next boot. Absent when the U-Boot environment cannot be read.\n",
                "# TYPE bmcd_firmware_update_staged gauge\n",
                "bmcd_firmware_update_staged 0\n",
                "\n",
            )
        );
    }

    #[test]
    fn a_silent_board_renders_only_what_is_true() {
        assert_eq!(
            render(&a_silent_board()),
            concat!(
                "# HELP bmcd_build_info Version of the daemon that produced these metrics.\n",
                "# TYPE bmcd_build_info gauge\n",
                "bmcd_build_info{version=\"2.3.7\"} 1\n",
                "\n",
                "# HELP bmcd_rtc_present Whether the BMC has a real-time clock at all.\n",
                "# TYPE bmcd_rtc_present gauge\n",
                "bmcd_rtc_present 0\n",
                "\n",
            )
        );
    }

    /// A family with nothing to report is left out entirely rather than
    /// emitted as a header with no samples: a scraper reading `# TYPE` with
    /// nothing under it learns the metric exists and is unset, which is not
    /// what "this board has no thermal zone" means.
    #[test]
    fn a_family_with_no_samples_is_not_written() {
        let mut out = String::new();
        family(&mut out, "bmcd_nothing", "gauge", "Nothing at all.", &[]);
        assert_eq!(out, "");
    }

    /// Label values come off the board -- a volume name, a cooling device the
    /// kernel named -- and the three characters the text format escapes are
    /// escaped rather than assumed absent.
    #[test]
    fn label_values_are_escaped() {
        let mut out = String::new();
        family(
            &mut out,
            "bmcd_awkward",
            "gauge",
            "A label that needs escaping.",
            &[Sample::new(
                vec![("volume", "root\"fs\\1\nweird".to_string())],
                1.0,
            )],
        );

        assert_eq!(
            out,
            concat!(
                "# HELP bmcd_awkward A label that needs escaping.\n",
                "# TYPE bmcd_awkward gauge\n",
                "bmcd_awkward{volume=\"root\\\"fs\\\\1\\nweird\"} 1\n",
                "\n",
            )
        );
    }

    /// Counters are integers and a microsecond offset is not. Neither may
    /// come out in exponent form, which some scrapers reject.
    #[test]
    fn values_are_written_the_way_a_scraper_expects() {
        assert_eq!(format_value(37019648.0), "37019648");
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(52.5), "52.5");
        assert_eq!(format_value(-0.000003077), "-0.000003077");
        assert_eq!(format_value(1e9), "1000000000");
    }

    #[test]
    fn millidegrees_become_degrees_to_one_decimal() {
        assert_eq!(millidegrees_to_degrees(52539), 52.5);
        assert_eq!(millidegrees_to_degrees(52999), 53.0);
        assert_eq!(millidegrees_to_degrees(0), 0.0);
    }
}
