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
mod rockusb;
mod rpiboot;
mod rpiboot_2712;
pub mod topology;
use self::{rockusb::RockusbBoot, rpiboot::RpiBoot, rpiboot_2712::RpiBoot2712};
use crate::hal::NodeId;
use async_trait::async_trait;
use rusb::GlobalContext;
use std::{collections::HashSet, fmt::Display, path::PathBuf, time::Duration};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite};
use tokio_util::sync::CancellationToken;
use topology::{HubTopology, UsbPortPath};
use tracing::{info, warn};

/// How long a flash waits for the module's USB boot device to appear.
const BOOT_DEVICE_WAIT: Duration = Duration::from_secs(15);
/// How often the bus is looked at while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub trait DataTransport: AsyncRead + AsyncWrite + AsyncSeek + Send + Unpin {}
impl DataTransport for tokio::fs::File {}

#[async_trait]
pub trait UsbBoot: 'static + Send + Sync + Display {
    fn is_supported(&self, vid_pid: &(u16, u16)) -> bool;
    async fn load_as_block_device(
        &self,
        _device: &rusb::Device<GlobalContext>,
        _port: Option<&UsbPortPath>,
    ) -> Result<PathBuf, UsbBootError> {
        // Every backend overrides this; one that does not cannot do it.
        Err(UsbBootError::internal_error(
            "this USB boot backend cannot load a block device",
        ))
    }

    async fn load_as_stream(
        &self,
        device: &rusb::Device<GlobalContext>,
        port: Option<&UsbPortPath>,
    ) -> Result<Box<dyn DataTransport>, UsbBootError> {
        let path = self.load_as_block_device(device, port).await?;
        Ok(Box::new(
            tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .await?,
        ) as Box<dyn DataTransport>)
    }
}

pub struct NodeDrivers {
    backends: Vec<Box<dyn UsbBoot>>,
    /// How this board wires its modules to USB, or `None` on a board that
    /// describes no fanout hub. Read once: the device tree does not change
    /// while the daemon runs.
    topology: Option<HubTopology>,
}

impl NodeDrivers {
    pub fn new() -> Self {
        let topology = HubTopology::read();
        match &topology {
            Some(hub) => info!(
                "USB fanout hub described for nodes {:?}; flashes are matched by port",
                hub.nodes()
            ),
            None => info!(
                "no USB fanout hub in the device tree; \
                 this board shows one node at a time and the first match is it"
            ),
        }

        NodeDrivers {
            backends: vec![
                Box::new(RpiBoot {}),
                Box::new(RpiBoot2712::new()),
                Box::new(RockusbBoot {}),
            ],
            topology,
        }
    }

    /// The USB device belonging to one node, and the backend that speaks to it.
    ///
    /// On a board with a fanout hub every module in maskrom is visible at
    /// once, so "the first supported device" is whichever one answered first
    /// -- which is how a flash of node 2 wrote node 1 and reported success.
    /// Where the device tree describes the hub, a device is accepted only on
    /// the port that node is wired to, and a device on any other port is a
    /// refusal rather than a substitute.
    ///
    /// Where it does not (v2.4, one mux, one node visible), the first match is
    /// correct and is what happens.
    ///
    /// The bus is looked at again every `POLL_INTERVAL` until the device is
    /// there or `BOOT_DEVICE_WAIT` has passed: a module takes a moment to
    /// enumerate after power-on, and one that fails high-speed enumeration is
    /// retried by the kernel, so a single look one second in gave up early.
    async fn find_for_node(
        &self,
        node: NodeId,
        cancel: &CancellationToken,
    ) -> Result<(rusb::Device<GlobalContext>, &dyn UsbBoot), UsbBootError> {
        tracing::info!(
            "Waiting for the USB boot device of node {}...",
            node.number()
        );
        let (device, idx) = wait_for_device(
            &self.backends,
            self.expected_ports(node),
            node.number(),
            BOOT_DEVICE_WAIT,
            POLL_INTERVAL,
            cancel,
            || {
                let devices = rusb::devices()?;
                Ok(devices
                    .iter()
                    .filter_map(|dev| match dev.device_descriptor() {
                        Ok(d) => Some(SeenDevice {
                            vid_pid: (d.vendor_id(), d.product_id()),
                            ports: dev.port_numbers().unwrap_or_default(),
                            handle: Self::actual_path(&dev),
                            device: dev,
                        }),
                        Err(_) => {
                            warn!("dropping {:?}, could not load descriptor", dev);
                            None
                        }
                    })
                    .collect())
            },
        )
        .await?;
        Ok((device, self.backends[idx].as_ref()))
    }

    /// Which hub port a node hangs off, or `None` on a board that describes
    /// no hub.
    ///
    /// The chain of PORTS only. Deliberately not a bus number: see
    /// `UsbPortPath::same_port`.
    fn expected_ports(&self, node: NodeId) -> Option<Vec<u8>> {
        self.topology.as_ref()?.node_ports(node.number())
    }

    /// Where the device that answered actually is. Built from the device
    /// rather than from a constant, so anything downstream matching a sysfs
    /// path gets the bus the kernel really used.
    fn actual_path(device: &rusb::Device<GlobalContext>) -> UsbPortPath {
        UsbPortPath {
            bus: device.bus_number(),
            ports: device.port_numbers().unwrap_or_default(),
        }
    }

    pub async fn load_as_block_device(
        &self,
        node: NodeId,
        cancel: &CancellationToken,
    ) -> Result<PathBuf, UsbBootError> {
        let (device, driver) = self.find_for_node(node, cancel).await?;
        let here = self
            .expected_ports(node)
            .map(|_| Self::actual_path(&device));
        driver.load_as_block_device(&device, here.as_ref()).await
    }

    pub async fn load_as_stream(
        &self,
        node: NodeId,
        cancel: &CancellationToken,
    ) -> Result<Box<dyn DataTransport>, UsbBootError> {
        let (device, driver) = self.find_for_node(node, cancel).await?;
        let here = self
            .expected_ports(node)
            .map(|_| Self::actual_path(&device));
        driver.load_as_stream(&device, here.as_ref()).await
    }
}

#[derive(Error, Debug)]
pub enum UsbBootError {
    /// The flash was cancelled while waiting for the module.
    #[error("cancelled while waiting for the module's USB boot device")]
    Cancelled,
    /// The module never showed up. Names what else was on the lines, since a
    /// device in the mini-PCIe slot of nodes 1 and 2 shares the USB pair and
    /// can keep the module from enumerating.
    #[error("{}", timeout_message(*.node, *.waited, others))]
    BootDeviceTimeout {
        node: u8,
        waited: Duration,
        others: Vec<String>,
    },
    /// The whole point of this error is that it is not a success message.
    /// Every wrong-node flash so far ended with "done" and a module that had
    /// not been written, so the text names the port asked for and the port
    /// something was actually on.
    #[error("node {node} requested on {port}; found {found} instead")]
    WrongPort {
        node: u8,
        port: String,
        found: String,
    },
    #[error("USB")]
    RusbError(#[from] rusb::Error),
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error("Error loading USB device: {0}")]
    InternalError(String),
}

impl UsbBootError {
    pub fn internal_error<E: ToString>(error: E) -> UsbBootError {
        UsbBootError::InternalError(error.to_string())
    }
}

/// One USB device as seen in a single look at the bus. `D` is the handle the
/// caller wants back for the winner (a `rusb::Device`); tests use `()`.
pub(crate) struct SeenDevice<D> {
    pub vid_pid: (u16, u16),
    pub ports: Vec<u8>,
    pub handle: UsbPortPath,
    pub device: D,
}

fn timeout_message(node: u8, waited: Duration, others: &[String]) -> String {
    let waited = if waited.subsec_millis() == 0 {
        format!("{} s", waited.as_secs())
    } else {
        format!("{} ms", waited.as_millis())
    };
    let head = format!(
        "node {node}: the module's USB boot device (Rockchip 2207:350b, Raspberry Pi 0a5c:2711 or 0a5c:2712) \
         did not appear within {waited}"
    );
    if others.is_empty() {
        head
    } else {
        format!(
            "{head}; other USB devices present: {}. If one of them is a device in the node's \
             mini-PCIe slot (nodes 1 and 2), it shares the USB lines used for flashing; \
             remove it and retry",
            others.join(", ")
        )
    }
}

/// Result of one look at the bus.
enum Look<D> {
    Found(SeenDevice<D>, usize),
    Missing {
        /// Supported devices on the wrong port.
        elsewhere: Vec<String>,
        /// Unsupported devices on the candidate port(s), root hubs excluded.
        others: Vec<String>,
    },
}

fn look<D>(
    backends: &[Box<dyn UsbBoot>],
    expected: Option<&Vec<u8>>,
    node: u8,
    devices: Vec<SeenDevice<D>>,
    logged: &mut HashSet<String>,
) -> Look<D> {
    let mut elsewhere = Vec::new();
    let mut others = Vec::new();
    let mut found = None;

    for dev in devices {
        let (vid, pid) = dev.vid_pid;
        // Logged once per distinct device per wait: this runs every 250 ms
        // on a board with little RAM, and the same line forty times is noise.
        let first = logged.insert(format!("{vid:04x}:{pid:04x}@{}", dev.handle));
        if first {
            info!("trying {:#06x}:{:#06x} on {}", vid, pid, dev.handle);
        }
        let backend = backends.iter().position(|b| b.is_supported(&dev.vid_pid));
        let on_port = expected.is_none_or(|e| dev.ports == *e);
        match backend {
            Some(idx) if on_port => {
                if found.is_none() {
                    info!("ID {:#06x}:{:#06x} {}", vid, pid, backends[idx]);
                    found = Some((dev, idx));
                }
            }
            Some(idx) => {
                // PORTS, not the bus. The bus a device lands on is decided by
                // its SPEED, not by where it is plugged in: this board pairs
                // an OHCI and an EHCI controller as companions for the same
                // physical ports, so a full-speed device appears on bus 1 and
                // a high-speed one on bus 2 -- at the same port, on the same
                // hub. A Rockchip in maskrom is high-speed, so pinning the bus
                // to 1 refused every node it was asked to flash. The port
                // chain says which module this is; the bus says nothing.
                if first {
                    warn!(
                        "ignoring {} on {}: node {} is on port {}",
                        backends[idx],
                        dev.handle,
                        node,
                        expected
                            .map(|e| UsbPortPath::describe_ports(e))
                            .unwrap_or_default()
                    );
                }
                elsewhere.push(format!("{} on {}", backends[idx], dev.handle));
            }
            None if on_port && vid != 0x1d6b => {
                let s = format!("{vid:04x}:{pid:04x}");
                if !others.contains(&s) {
                    others.push(s);
                }
            }
            None => {}
        }
    }

    match found {
        Some((dev, idx)) => Look::Found(dev, idx),
        None => Look::Missing { elsewhere, others },
    }
}

/// Look at the bus every `interval` until a supported device is on the node's
/// port, or `wait` has passed. Dropping the future cancels the wait.
pub(crate) async fn wait_for_device<D>(
    backends: &[Box<dyn UsbBoot>],
    expected: Option<Vec<u8>>,
    node: u8,
    wait: Duration,
    interval: Duration,
    cancel: &CancellationToken,
    mut enumerate: impl FnMut() -> Result<Vec<SeenDevice<D>>, UsbBootError>,
) -> Result<(D, usize), UsbBootError> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut others: Vec<String> = Vec::new();
    let mut elsewhere: Vec<String> = Vec::new();
    let mut logged = HashSet::new();
    let mut enumerate_failed = false;

    loop {
        if cancel.is_cancelled() {
            return Err(UsbBootError::Cancelled);
        }
        // A failed enumeration is retried until the deadline: the bus is
        // busy re-enumerating exactly when this is most likely to fail.
        match enumerate() {
            Ok(devices) => match look(backends, expected.as_ref(), node, devices, &mut logged) {
                Look::Found(dev, idx) => return Ok((dev.device, idx)),
                Look::Missing {
                    elsewhere: e,
                    others: o,
                } => {
                    for x in o {
                        if !others.contains(&x) {
                            others.push(x);
                        }
                    }
                    elsewhere = e;
                }
            },
            Err(e) => {
                if !enumerate_failed {
                    warn!("listing USB devices failed, will retry: {e}");
                    enumerate_failed = true;
                }
            }
        }
        if tokio::time::Instant::now() + interval > deadline {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = cancel.cancelled() => return Err(UsbBootError::Cancelled),
        }
    }

    Err(match &expected {
        Some(e) if !elsewhere.is_empty() => UsbBootError::WrongPort {
            node,
            port: UsbPortPath::describe_ports(e),
            found: elsewhere.join(", "),
        },
        _ => UsbBootError::BootDeviceTimeout {
            node,
            waited: wait,
            others,
        },
    })
}

#[cfg(test)]
mod wait_tests {
    use super::*;
    use std::cell::Cell;

    struct Fake(Vec<(u16, u16)>);
    impl Display for Fake {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Fake")
        }
    }
    #[async_trait]
    impl UsbBoot for Fake {
        fn is_supported(&self, v: &(u16, u16)) -> bool {
            self.0.contains(v)
        }
    }

    fn backends() -> Vec<Box<dyn UsbBoot>> {
        vec![Box::new(Fake(vec![(0x2207, 0x350b), (0x0a5c, 0x2711)]))]
    }

    fn dev(vid: u16, pid: u16, ports: &[u8]) -> SeenDevice<u8> {
        SeenDevice {
            vid_pid: (vid, pid),
            ports: ports.to_vec(),
            handle: UsbPortPath {
                bus: 1,
                ports: ports.to_vec(),
            },
            device: 7,
        }
    }

    const FAST: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn found_on_third_poll() {
        let n = Cell::new(0);
        let r = wait_for_device(
            &backends(),
            None,
            1,
            Duration::from_secs(5),
            FAST,
            &CancellationToken::new(),
            || {
                n.set(n.get() + 1);
                Ok(if n.get() < 3 {
                    vec![dev(0x1d6b, 2, &[])]
                } else {
                    vec![dev(0x2207, 0x350b, &[1])]
                })
            },
        )
        .await;
        assert_eq!(r.unwrap(), (7, 0));
        assert_eq!(n.get(), 3);
    }

    #[tokio::test]
    async fn never_appears_names_other_device() {
        let e = wait_for_device(
            &backends(),
            None,
            2,
            Duration::from_millis(20),
            FAST,
            &CancellationToken::new(),
            || Ok(vec![dev(0x1d6b, 2, &[]), dev(0x1a86, 0x7523, &[1, 2])]),
        )
        .await
        .unwrap_err();
        let m = e.to_string();
        assert_eq!(
            m,
            "node 2: the module's USB boot device (Rockchip 2207:350b, Raspberry Pi 0a5c:2711 or 0a5c:2712) \
             did not appear within 20 ms; other USB devices present: 1a86:7523. If one of them is a \
             device in the node's mini-PCIe slot (nodes 1 and 2), it shares the USB lines used \
             for flashing; remove it and retry"
        );
        assert!(!m.contains("1d6b"));
    }

    #[tokio::test]
    async fn nothing_at_all_is_short() {
        let e = wait_for_device::<u8>(
            &backends(),
            None,
            3,
            Duration::from_millis(5),
            FAST,
            &CancellationToken::new(),
            || Ok(vec![dev(0x1d6b, 1, &[])]),
        )
        .await
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "node 3: the module's USB boot device (Rockchip 2207:350b, Raspberry Pi 0a5c:2711 or 0a5c:2712) \
             did not appear within 5 ms"
        );
    }

    #[test]
    fn real_wait_is_fifteen_seconds_in_the_text() {
        let m = UsbBootError::BootDeviceTimeout {
            node: 1,
            waited: BOOT_DEVICE_WAIT,
            others: vec![],
        }
        .to_string();
        assert!(m.ends_with("did not appear within 15 s"), "{m}");
    }

    #[tokio::test]
    async fn cancel_during_wait_returns_promptly() {
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            c2.cancel();
        });
        let start = std::time::Instant::now();
        let e = wait_for_device::<u8>(
            &backends(),
            None,
            1,
            Duration::from_secs(30),
            Duration::from_millis(500),
            &cancel,
            || Ok(vec![]),
        )
        .await
        .unwrap_err();
        assert!(matches!(e, UsbBootError::Cancelled), "{e}");
        assert!(start.elapsed() < Duration::from_millis(400));
    }

    #[tokio::test]
    async fn enumerate_errors_are_retried() {
        let n = Cell::new(0);
        let r = wait_for_device(
            &backends(),
            None,
            1,
            Duration::from_secs(5),
            FAST,
            &CancellationToken::new(),
            || {
                n.set(n.get() + 1);
                if n.get() < 3 {
                    Err(UsbBootError::RusbError(rusb::Error::Busy))
                } else {
                    Ok(vec![dev(0x2207, 0x350b, &[1])])
                }
            },
        )
        .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn port_filter_respected() {
        // A supported device on another port is not accepted; an unrelated
        // device on another port is not blamed.
        let e = wait_for_device(
            &backends(),
            Some(vec![2]),
            2,
            Duration::from_millis(5),
            FAST,
            &CancellationToken::new(),
            || Ok(vec![dev(0x2207, 0x350b, &[1]), dev(0x1a86, 0x7523, &[3])]),
        )
        .await
        .unwrap_err();
        assert!(matches!(e, UsbBootError::WrongPort { node: 2, .. }), "{e}");

        let e = wait_for_device(
            &backends(),
            Some(vec![2]),
            2,
            Duration::from_millis(5),
            FAST,
            &CancellationToken::new(),
            || Ok(vec![dev(0x1a86, 0x7523, &[3]), dev(0x1a86, 0x7523, &[2])]),
        )
        .await
        .unwrap_err();
        let m = e.to_string();
        assert!(m.contains("present: 1a86:7523."), "{m}");

        let ok = wait_for_device(
            &backends(),
            Some(vec![2]),
            2,
            Duration::from_millis(5),
            FAST,
            &CancellationToken::new(),
            || Ok(vec![dev(0x2207, 0x350b, &[2])]),
        )
        .await;
        assert!(ok.is_ok());
    }
}
