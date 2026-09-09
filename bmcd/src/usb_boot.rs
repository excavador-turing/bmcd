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
pub mod topology;
use self::{rockusb::RockusbBoot, rpiboot::RpiBoot};
use crate::hal::NodeId;
use async_trait::async_trait;
use rusb::GlobalContext;
use std::{fmt::Display, path::PathBuf};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite};
use topology::{HubTopology, UsbPortPath};
use tracing::{info, warn};

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
        Err(UsbBootError::NotSupported)
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
            backends: vec![Box::new(RpiBoot {}), Box::new(RockusbBoot {})],
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
    fn find_for_node(
        &self,
        node: NodeId,
    ) -> Result<(rusb::Device<GlobalContext>, &dyn UsbBoot), UsbBootError> {
        tracing::info!("Checking for presence of a USB device...");
        let expected = self.expected_port(node);
        let devices = rusb::devices()?;

        // Devices a backend understands that are on the wrong port. Kept so a
        // refusal can say what was there instead of only what was missing.
        let mut seen_elsewhere: Vec<String> = Vec::new();

        for backend in &self.backends {
            for dev in devices.iter() {
                let Ok(descriptor) = dev.device_descriptor() else {
                    warn!("dropping {:?}, could not load descriptor", dev);
                    continue;
                };

                let vid_pid = (descriptor.vendor_id(), descriptor.product_id());
                info!("trying {:#06x}:{:#06x}", vid_pid.0, vid_pid.1);

                if !backend.is_supported(&vid_pid) {
                    continue;
                }

                info!("ID {:#06x}:{:#06x} {}", vid_pid.0, vid_pid.1, backend);

                // No described hub: this board shows one node at a time, so
                // the device that answered is the one that was asked for.
                let Some(expected) = expected.as_ref() else {
                    return Ok((dev, backend.as_ref()));
                };

                let here = UsbPortPath {
                    bus: dev.bus_number(),
                    ports: dev.port_numbers().unwrap_or_default(),
                };

                if &here == expected {
                    return Ok((dev, backend.as_ref()));
                }

                warn!(
                    "ignoring {} on {}: node {} is on {}",
                    backend,
                    here,
                    node.number(),
                    expected
                );
                seen_elsewhere.push(format!("{} on {}", backend, here));
            }
        }

        Err(match expected {
            Some(expected) if !seen_elsewhere.is_empty() => UsbBootError::WrongPort {
                node: node.number(),
                port: expected.to_string(),
                found: seen_elsewhere.join(", "),
            },
            Some(expected) => UsbBootError::NotOnPort {
                node: node.number(),
                port: expected.to_string(),
            },
            None => UsbBootError::NotSupported,
        })
    }

    /// Where a node's device must appear, or `None` on a board that describes
    /// no hub.
    fn expected_port(&self, node: NodeId) -> Option<UsbPortPath> {
        // Bus 1 is the controller the fanout hub hangs off; a device the hub
        // is not on cannot be one of these nodes anyway, and `find_for_node`
        // compares the whole path including the bus.
        self.topology.as_ref()?.port_path(1, node.number())
    }

    pub async fn load_as_block_device(&self, node: NodeId) -> Result<PathBuf, UsbBootError> {
        let expected = self.expected_port(node);
        let (device, driver) = self.find_for_node(node)?;
        driver
            .load_as_block_device(&device, expected.as_ref())
            .await
    }

    pub async fn load_as_stream(
        &self,
        node: NodeId,
    ) -> Result<Box<dyn DataTransport>, UsbBootError> {
        let expected = self.expected_port(node);
        let (device, driver) = self.find_for_node(node)?;
        driver.load_as_stream(&device, expected.as_ref()).await
    }
}

#[derive(Error, Debug)]
pub enum UsbBootError {
    #[error("Compute module's USB interface not found or supported")]
    NotSupported,
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
    #[error("node {node} requested on {port}; nothing is in maskrom or rockusb there")]
    NotOnPort { node: u8, port: String },
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
