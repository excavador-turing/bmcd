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
mod event_listener;
mod io;

use crate::usb_boot::topology::UsbPortPath;
use anyhow::bail;
use std::time::{SystemTime, UNIX_EPOCH};

#[doc(inline)]
pub use event_listener::*;
pub use io::*;
use std::{path::PathBuf, process::Output};
use tokio::io::AsyncBufReadExt;
use tracing::warn;

pub fn string_from_utf16(bytes: &[u8], little_endian: bool) -> String {
    // as_chunks yields &[u8; 2], so the pair IS the array: the fallible
    // conversion this used to do -- and the unreachable!() guarding it --
    // were only there because chunks_exact hands back a slice whose length
    // the type system has forgotten.
    let u16s = bytes.as_chunks::<2>().0.iter().map(|pair| {
        if little_endian {
            u16::from_le_bytes(*pair)
        } else {
            u16::from_be_bytes(*pair)
        }
    });

    let mut string = char::decode_utf16(u16s)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect::<String>();

    if bytes.len() % 2 == 1 {
        string.push(char::REPLACEMENT_CHARACTER)
    }

    string
}

pub fn string_from_utf32(bytes: &[u8], little_endian: bool) -> String {
    bytes
        .chunks(4)
        .map(|slice| {
            let Ok(owned) = slice.try_into() else {
                return char::REPLACEMENT_CHARACTER;
            };

            let scalar = if little_endian {
                u32::from_le_bytes(owned)
            } else {
                u32::from_be_bytes(owned)
            };

            char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER)
        })
        .collect()
}

/// The block device a compute module presents over USB.
///
/// `expected_port` is where that module is wired, on a board whose device tree
/// describes a fanout hub. With four modules behind one hub, several can be
/// mass storage at the same time, and the old rule -- exactly one Rockchip
/// device on the whole board, or refuse -- turned a routine two-module bench
/// into "Several supported devices found". Filtering by port makes the
/// ambiguity disappear rather than reporting it.
///
/// `None` means no hub is described (v2.4, one node visible at a time), and
/// the old rule is kept: one match, or say so.
pub async fn get_device_path(
    allowed_vendors: &[&str],
    expected_port: Option<&UsbPortPath>,
) -> anyhow::Result<PathBuf> {
    let mut contents = tokio::fs::read_dir("/sys/block/").await.map_err(|err| {
        std::io::Error::new(err.kind(), format!("Failed to list devices: {}", err))
    })?;

    let mut matching_devices = vec![];
    // Devices of the right vendor sitting on some other module's port. Named
    // in the error, because "several found" was never the useful half.
    let mut wrong_port = vec![];

    while let Some(entry) = contents.next_entry().await.map_err(|err| {
        std::io::Error::new(
            err.kind(),
            format!("Intermittent IO error while listing devices: {}", err),
        )
    })? {
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        let vendor_path = format!("/sys/block/{}/device/vendor", file_name);
        let Ok(vendor) = tokio::fs::read_to_string(vendor_path).await else {
            continue;
        };
        let vendor = vendor.trim();

        if !allowed_vendors.contains(&vendor) {
            continue;
        }

        // Which port this block device hangs off. The canonical path of
        // /sys/block/<dev> runs through every hub between the controller and
        // the module, so the port's own directory is a component of it.
        match expected_port {
            None => matching_devices.push(file_name.clone()),
            Some(port) => {
                let link = tokio::fs::canonicalize(format!("/sys/block/{}", file_name)).await;
                match link {
                    Ok(path) if port.contains(&path) => matching_devices.push(file_name.clone()),
                    Ok(_) => wrong_port.push(file_name.clone()),
                    Err(e) => {
                        warn!("cannot resolve /sys/block/{}: {}", file_name, e);
                    }
                }
            }
        }
    }

    let name = match (&matching_devices[..], expected_port) {
        ([device], _) => device.clone(),
        ([], Some(port)) if !wrong_port.is_empty() => bail!(
            "no storage on {}; {} of the same kind {} on another port",
            port,
            wrong_port.join(", "),
            if wrong_port.len() == 1 { "is" } else { "are" }
        ),
        ([], Some(port)) => bail!("no storage on {}", port),
        ([], None) => bail!("No supported USB devices found"),
        // Two devices on one hub port is a hub we do not know about, not a
        // choice to make silently.
        (several, Some(port)) => bail!(
            "{} storage devices on {}: {}",
            several.len(),
            port,
            several.join(", ")
        ),
        (_, None) => bail!("Several supported devices found"),
    };

    Ok(tokio::fs::canonicalize(format!("/dev/{}", name)).await?)
}

/// Get current time in seconds since Unix epoch. Returns `None` if current time is before epoch.
pub fn get_timestamp_unix() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|x| x.as_secs())
}

pub async fn logging_sink_stdio(output: &Output) -> std::io::Result<()> {
    let mut lines = output.stdout.lines();
    while let Some(line) = lines.next_line().await? {
        tracing::info!("{}", line);
    }

    let mut lines = output.stderr.lines();
    while let Some(line) = lines.next_line().await? {
        tracing::error!("{}", line);
    }
    Ok(())
}
