// Copyright 2026 Vincent Nguyen
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

//! A CM5 (BCM2712) over rpiboot.
//!
//! The CM4 path hands the ROM to `rustpiboot`, which knows no BCM2712 and
//! answers every file request with the one file it embeds. A BCM2712 has no
//! VideoCore mass-storage firmware at all: its ROM takes `bootcode5.bin`, which
//! asks for `bootmain` and the chip's memory and RP1 blobs, which in turn asks
//! for `config.txt` and `boot.img` -- a Linux ramdisk that exports the eMMC as
//! a USB mass-storage gadget. This is the protocol of Raspberry Pi's `usbboot`
//! with a file server that looks the requested name up.
//!
//! Those files are about 12 MB, which the firmware image has no room for, so
//! they live on the overlay at [`PAYLOAD_DIR`], laid out the way
//! `rpiboot -d <dir>` expects. `tools/cm5-payload` builds and installs them.

use super::{topology::UsbPortPath, UsbBoot, UsbBootError};
use crate::utils::get_device_path;
use async_trait::async_trait;
use rusb::{request_type, Device, DeviceHandle, Direction, GlobalContext, Recipient, RequestType};
use std::{
    fmt::Display,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

pub const BCM2712_VID_PID: (u16, u16) = (0x0a5c, 0x2712);

/// Where the payload is installed. On the UBIFS overlay itself rather than
/// under `/`, so it is not carried into the overlay backups `upper/` feeds.
pub const PAYLOAD_DIR: &str = "/mnt/overlay/rpiboot-2712";

/// Per-chip subdirectory, looked in first, as `usbboot` does.
const CHIP_DIR: &str = "2712";
const SECOND_STAGE: &str = "bootcode5.bin";
/// What a payload cannot boot without. Checked before any USB traffic, because
/// a missing payload is the likeliest failure and the least obvious from the
/// module's side: it just sits in the ROM.
const REQUIRED: [&str; 3] = ["2712/bootcode5.bin", "config.txt", "boot.img"];

/// The SCSI vendor of the eMMC LUN. The payload's gadget sets each LUN's
/// inquiry string to the kernel's name for the device, and the first eight
/// bytes of it are the vendor field.
const EMMC_VENDOR: &str = "mmcblk0";

const MAX_TRANSFER: usize = 16 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
const BULK_TIMEOUT: Duration = Duration::from_secs(5);
/// The second stage takes its time to ask for the next file; `usbboot` waits
/// 20 s for a file server message.
const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// From the ROM answering to the gadget enumerating: bootloader, kernel, and
/// the ramdisk waiting for its storage, which upstream's own gadget allows
/// 30 s.
const BOOT_TIMEOUT: Duration = Duration::from_secs(90);
/// From the gadget enumerating to its disk appearing under `/sys/block`.
const DISK_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(250);
/// A ROM that keeps coming back asking for its second stage is not going to
/// take it.
const MAX_SECOND_STAGE_SENDS: u32 = 3;
const MAX_READ_RETRIES: u32 = 5;

pub struct RpiBoot2712 {
    dir: PathBuf,
}

impl RpiBoot2712 {
    pub fn new() -> Self {
        RpiBoot2712 {
            dir: PathBuf::from(PAYLOAD_DIR),
        }
    }

    fn check_payload(&self) -> Result<(), UsbBootError> {
        let missing: Vec<&str> = REQUIRED
            .iter()
            .copied()
            .filter(|f| !self.dir.join(f).is_file())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        Err(UsbBootError::internal_error(format!(
            "this is a CM5 (BCM2712), which boots from a payload this firmware does not \
             carry; {} is missing {}. Build and install it with tools/cm5-payload",
            self.dir.display(),
            missing.join(", ")
        )))
    }
}

#[async_trait]
impl UsbBoot for RpiBoot2712 {
    fn is_supported(&self, vid_pid: &(u16, u16)) -> bool {
        vid_pid == &BCM2712_VID_PID
    }

    async fn load_as_block_device(
        &self,
        device: &rusb::Device<GlobalContext>,
        port: Option<&UsbPortPath>,
    ) -> Result<PathBuf, UsbBootError> {
        self.check_payload()?;
        let dir = self.dir.clone();
        let rom = device.clone();
        let gadget = tokio::task::spawn_blocking(move || boot(&dir, &rom))
            .await
            .map_err(UsbBootError::internal_error)??;
        info!("CM5 storage gadget is up on {gadget}, waiting for its eMMC");

        // The bus the gadget landed on, not the one the ROM did: the
        // companion controllers put a device on a bus by its speed.
        let expected = port.map(|_| gadget);
        wait_for_disk(expected.as_ref()).await
    }
}

impl Display for RpiBoot2712 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rpiboot-2712")
    }
}

async fn wait_for_disk(expected: Option<&UsbPortPath>) -> Result<PathBuf, UsbBootError> {
    let deadline = Instant::now() + DISK_TIMEOUT;
    loop {
        match get_device_path(&[EMMC_VENDOR], expected).await {
            Ok(path) => return Ok(path),
            Err(e) if Instant::now() >= deadline => {
                return Err(UsbBootError::internal_error(format!(
                    "the CM5 storage gadget enumerated but no eMMC appeared within {} s: {e}",
                    DISK_TIMEOUT.as_secs()
                )))
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Drive the module from its ROM to the storage gadget, and return where the
/// gadget enumerated.
///
/// The module re-enumerates at every stage, so the device is looked up again
/// each time by the chain of ports the ROM answered on -- never by bus, and
/// never by "the first BCM2712 on the board".
fn boot(dir: &Path, rom: &Device<GlobalContext>) -> Result<UsbPortPath, UsbBootError> {
    let ports = rom.port_numbers()?;
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut handled: Option<(u8, u8)> = None;
    let mut second_stage_sends = 0;
    let mut sessions = 0;

    while Instant::now() < deadline {
        for dev in rusb::devices()?.iter() {
            if dev.port_numbers().ok().as_deref() != Some(&ports[..]) {
                continue;
            }
            let Ok(desc) = dev.device_descriptor() else {
                continue;
            };
            let here = (dev.bus_number(), dev.address());
            let vid_pid = (desc.vendor_id(), desc.product_id());

            if vid_pid != BCM2712_VID_PID {
                if sessions > 0 {
                    return Ok(UsbPortPath {
                        bus: dev.bus_number(),
                        ports: ports.clone(),
                    });
                }
                continue;
            }
            // The same enumeration again: the module has not moved on yet.
            if handled == Some(here) {
                continue;
            }

            // It can be gone again by the time it is opened, mid re-enumeration;
            // the next look finds whatever it became.
            let (handle, out_ep) = match open(&dev) {
                Ok(opened) => opened,
                Err(e) => {
                    debug!("BCM2712 on {here:?} could not be opened yet: {e}");
                    break;
                }
            };
            if is_rom(desc.serial_number_string_index()) {
                second_stage_sends += 1;
                if second_stage_sends > MAX_SECOND_STAGE_SENDS {
                    return Err(UsbBootError::internal_error(format!(
                        "the BCM2712 ROM asked for {SECOND_STAGE} {MAX_SECOND_STAGE_SENDS} times \
                         and did not start it"
                    )));
                }
                info!("BCM2712 ROM: sending {SECOND_STAGE}");
                send_second_stage(&handle, out_ep, dir)?;
            } else {
                info!("BCM2712 second stage: serving files from {}", dir.display());
                file_server(&handle, out_ep, dir)?;
                sessions += 1;
            }
            handled = Some(here);
            break;
        }
        std::thread::sleep(POLL);
    }

    Err(UsbBootError::internal_error(if sessions == 0 {
        format!(
            "the BCM2712 never got past its ROM within {} s",
            BOOT_TIMEOUT.as_secs()
        )
    } else {
        format!(
            "the CM5 took its boot files but its storage gadget did not appear within {} s; \
             the node's UART shows why",
            BOOT_TIMEOUT.as_secs()
        )
    }))
}

/// Whether the device wants its second stage rather than files. `usbboot`
/// decides on serial string index 0 or 3, and so does this.
fn is_rom(serial_index: Option<u8>) -> bool {
    matches!(serial_index, None | Some(3))
}

fn open(dev: &Device<GlobalContext>) -> Result<(DeviceHandle<GlobalContext>, u8), UsbBootError> {
    let config = dev.active_config_descriptor()?;
    // One interface on everything but the BCM2837, which puts a mass-storage
    // interface first and the vendor one second. Kept for parity with usbboot.
    let (interface, out_ep) = if config.num_interfaces() == 1 {
        (0, 1)
    } else {
        (1, 3)
    };
    let handle = dev.open()?;
    handle.claim_interface(interface)?;
    Ok((handle, out_ep))
}

fn send_second_stage(
    handle: &DeviceHandle<GlobalContext>,
    out_ep: u8,
    dir: &Path,
) -> Result<(), UsbBootError> {
    let path = lookup(dir, SECOND_STAGE).ok_or_else(|| {
        UsbBootError::internal_error(format!("{SECOND_STAGE} is not in {}", dir.display()))
    })?;
    let mut code = Vec::new();
    File::open(&path)?.read_to_end(&mut code)?;

    // The boot message: the length, then a 20-byte signature that is all
    // zeroes on a module without secure boot.
    let mut message = [0u8; 24];
    message[..4].copy_from_slice(&u32::try_from(code.len()).unwrap_or(u32::MAX).to_le_bytes());
    ep_write(handle, out_ep, &message)?;
    ep_write(handle, out_ep, &code)?;

    std::thread::sleep(Duration::from_secs(1));
    let mut status = [0u8; 4];
    match ep_read(handle, &mut status) {
        Ok(_) => match u32::from_le_bytes(status) {
            0 => Ok(()),
            code => Err(UsbBootError::internal_error(format!(
                "the BCM2712 ROM rejected {SECOND_STAGE}: {code:#x}"
            ))),
        },
        // The ROM may drop off the bus before it answers; it then comes back
        // as the second stage.
        Err(rusb::Error::NoDevice | rusb::Error::Io) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Request<'a> {
    GetFileSize(&'a str),
    ReadFile(&'a str),
    Metadata(&'a str),
    Done,
}

/// A file server message: a little-endian command, then a NUL-terminated name.
fn parse_request(message: &[u8; 260]) -> Result<Request<'_>, UsbBootError> {
    let command = u32::from_le_bytes([message[0], message[1], message[2], message[3]]);
    let name = &message[4..];
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    let name = std::str::from_utf8(&name[..end])
        .map_err(|_| UsbBootError::internal_error("file server: a name that is not UTF-8"))?;

    // An empty name is also how the second stage says it is done.
    if name.is_empty() || command == 2 {
        return Ok(Request::Done);
    }
    // `*KEY*VALUE`: the bootloader reporting serial, MAC and the like.
    if let Some(metadata) = name.strip_prefix('*') {
        return Ok(Request::Metadata(metadata));
    }
    match command {
        0 => Ok(Request::GetFileSize(name)),
        1 => Ok(Request::ReadFile(name)),
        other => Err(UsbBootError::internal_error(format!(
            "file server: unknown command {other}"
        ))),
    }
}

fn file_server(
    handle: &DeviceHandle<GlobalContext>,
    out_ep: u8,
    dir: &Path,
) -> Result<(), UsbBootError> {
    let mut message = [0u8; 260];
    let mut retries = 0;
    loop {
        match ep_read(handle, &mut message) {
            Ok(n) if n == message.len() => retries = 0,
            Ok(n) => {
                return Err(UsbBootError::internal_error(format!(
                    "file server: a {n}-byte message"
                )))
            }
            // The module has moved on: it booted, or re-enumerated as the
            // next stage.
            Err(rusb::Error::NoDevice | rusb::Error::Io) => return Ok(()),
            Err(e) if retries < MAX_READ_RETRIES => {
                retries += 1;
                debug!("file server: read failed ({e}), retrying");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            Err(e) => return Err(e.into()),
        }

        match parse_request(&message)? {
            Request::Done => {
                let _ = ep_write(handle, out_ep, &[]);
                return Ok(());
            }
            Request::Metadata(m) => {
                debug!("BCM2712 metadata: {m}");
                ep_write(handle, out_ep, &[])?;
            }
            Request::GetFileSize(name) => match lookup(dir, name) {
                Some(path) => {
                    let size = std::fs::metadata(&path)?.len();
                    control_out(handle, size)?;
                }
                None => {
                    debug!("not in the payload: {name}");
                    ep_write(handle, out_ep, &[])?;
                }
            },
            Request::ReadFile(name) => match lookup(dir, name) {
                Some(path) => send_file(handle, out_ep, &path)?,
                None => ep_write(handle, out_ep, &[])?,
            },
        }
    }
}

/// Where a requested name is in the payload: the chip's own directory first,
/// then the top level. `None` for names that are not plain relative paths,
/// because the name is chosen by the device.
fn lookup(dir: &Path, name: &str) -> Option<PathBuf> {
    let relative = Path::new(name);
    if !relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        warn!("refusing a request for {name:?}");
        return None;
    }
    [dir.join(CHIP_DIR).join(relative), dir.join(relative)]
        .into_iter()
        .find(|p| p.is_file())
}

fn send_file(
    handle: &DeviceHandle<GlobalContext>,
    out_ep: u8,
    path: &Path,
) -> Result<(), UsbBootError> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    info!(
        "BCM2712: sending {} ({})",
        path.display(),
        humansize::format_size(size, humansize::DECIMAL)
    );
    control_out(handle, size)?;

    // Streamed in transfer-sized pieces; boot.img is megabytes and this board
    // has little RAM to spare.
    let mut buf = vec![0u8; MAX_TRANSFER];
    let mut sent = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bulk_all(handle, out_ep, &buf[..n])?;
        sent += n as u64;
    }
    if sent != size {
        return Err(UsbBootError::internal_error(format!(
            "{} changed while it was being sent",
            path.display()
        )));
    }
    Ok(())
}

/// Announce a transfer of `len` bytes: a vendor request with the length split
/// across value and index, and no data stage. On its own it is the reply to
/// a file size request.
fn control_out(handle: &DeviceHandle<GlobalContext>, len: u64) -> Result<(), UsbBootError> {
    let len = u32::try_from(len)
        .map_err(|_| UsbBootError::internal_error("a payload file over 4 GiB"))?;
    handle.write_control(
        request_type(Direction::Out, RequestType::Vendor, Recipient::Device),
        0,
        (len & 0xffff) as u16,
        (len >> 16) as u16,
        &[],
        CONTROL_TIMEOUT,
    )?;
    Ok(())
}

/// The announcement, then the bytes. An empty write is how a file that does
/// not exist is answered.
fn ep_write(
    handle: &DeviceHandle<GlobalContext>,
    out_ep: u8,
    data: &[u8],
) -> Result<(), UsbBootError> {
    control_out(handle, data.len() as u64)?;
    for chunk in data.chunks(MAX_TRANSFER) {
        bulk_all(handle, out_ep, chunk)?;
    }
    Ok(())
}

fn bulk_all(
    handle: &DeviceHandle<GlobalContext>,
    out_ep: u8,
    mut data: &[u8],
) -> Result<(), UsbBootError> {
    while !data.is_empty() {
        let n = handle.write_bulk(out_ep, data, BULK_TIMEOUT)?;
        if n == 0 {
            return Err(UsbBootError::internal_error(
                "a bulk write that wrote nothing",
            ));
        }
        data = &data[n..];
    }
    Ok(())
}

fn ep_read(handle: &DeviceHandle<GlobalContext>, buf: &mut [u8]) -> rusb::Result<usize> {
    let len = buf.len();
    handle.read_control(
        request_type(Direction::In, RequestType::Vendor, Recipient::Device),
        0,
        (len & 0xffff) as u16,
        (len >> 16) as u16,
        buf,
        READ_TIMEOUT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(command: u32, name: &[u8]) -> [u8; 260] {
        let mut m = [0u8; 260];
        m[..4].copy_from_slice(&command.to_le_bytes());
        m[4..4 + name.len()].copy_from_slice(name);
        m
    }

    #[test]
    fn requests() {
        assert_eq!(
            parse_request(&message(0, b"config.txt")).unwrap(),
            Request::GetFileSize("config.txt")
        );
        assert_eq!(
            parse_request(&message(1, b"boot.img")).unwrap(),
            Request::ReadFile("boot.img")
        );
        assert_eq!(
            parse_request(&message(0, b"*SERIAL*0123abcd")).unwrap(),
            Request::Metadata("SERIAL*0123abcd")
        );
        assert_eq!(parse_request(&message(2, b"")).unwrap(), Request::Done);
        assert_eq!(parse_request(&message(0, b"")).unwrap(), Request::Done);
        assert!(parse_request(&message(7, b"x")).is_err());
    }

    #[test]
    fn a_name_filling_the_message_has_no_terminator() {
        let name = [b'a'; 256];
        let m = message(0, &name);
        assert_eq!(
            parse_request(&m).unwrap(),
            Request::GetFileSize(std::str::from_utf8(&name).unwrap())
        );
    }

    fn payload() -> tempdir::TempDir {
        let d = tempdir::TempDir::new("rpiboot-2712").unwrap();
        std::fs::create_dir(d.path().join("2712")).unwrap();
        std::fs::write(d.path().join("2712/bootmain"), b"chip").unwrap();
        std::fs::write(d.path().join("2712/config.txt"), b"chip config").unwrap();
        std::fs::write(d.path().join("config.txt"), b"top config").unwrap();
        std::fs::write(d.path().join("boot.img"), b"img").unwrap();
        d
    }

    #[test]
    fn chip_directory_first_then_top_level() {
        let d = payload();
        assert_eq!(
            lookup(d.path(), "config.txt").unwrap(),
            d.path().join("2712/config.txt")
        );
        assert_eq!(
            lookup(d.path(), "boot.img").unwrap(),
            d.path().join("boot.img")
        );
        assert_eq!(
            lookup(d.path(), "bootmain").unwrap(),
            d.path().join("2712/bootmain")
        );
        assert!(lookup(d.path(), "pieeprom.sig").is_none());
        assert!(
            lookup(d.path(), "2712").is_none(),
            "a directory is not a file"
        );
    }

    #[test]
    fn names_the_device_should_not_reach() {
        let d = payload();
        for name in [
            "../2712/bootmain",
            "/etc/shadow",
            "2712/../boot.img",
            "./boot.img",
        ] {
            assert!(lookup(d.path(), name).is_none(), "{name}");
        }
    }

    #[test]
    fn a_missing_payload_is_named() {
        let d = payload();
        let boot = RpiBoot2712 {
            dir: d.path().to_path_buf(),
        };
        let e = boot.check_payload().unwrap_err().to_string();
        assert!(e.contains("2712/bootcode5.bin"), "{e}");
        assert!(!e.contains("boot.img"), "{e}");

        std::fs::write(d.path().join("2712/bootcode5.bin"), b"code").unwrap();
        boot.check_payload().unwrap();
    }

    #[test]
    fn rom_by_serial_index() {
        assert!(is_rom(None));
        assert!(is_rom(Some(3)));
        assert!(!is_rom(Some(1)));
    }
}
