# CM5 rpiboot payload

A CM5 (BCM2712) has no mass-storage firmware of its own. To flash one over
USB, bmcd's `rpiboot-2712` backend sends it Raspberry Pi's `bootcode5.bin`,
then serves the files the bootloader asks for: the chip's memory and RP1
blobs, `config.txt`, and `boot.img`, a ramdisk that exports the eMMC as a USB
mass-storage gadget. Those files are about 12 MB, which the firmware image has
no room for, so they are installed once on the BMC's overlay at
`/mnt/overlay/rpiboot-2712`. A CM4 does not need any of this.

## Build and install

```sh
./run.sh                          # builds out/rpiboot-2712 in a Debian container
./install.sh root@turingpi.local  # copies it to the BMC, checking every file
```

`run.sh` needs only Docker. On an arm64 host it also runs the ramdisk's shell
in a chroot and checks it has every applet `init` uses.

Without the payload, flashing a CM5 fails with a message that names the
missing files. Flashing a CM4 is not affected.

## What is in it

Everything comes from Raspberry Pi's
[usbboot](https://github.com/raspberrypi/usbboot) at the commit pinned in
`build.sh`, checked by sha256:

| file | from |
| -- | -- |
| `2712/*` | `firmware/bootfiles.bin`, unchanged |
| `config.txt` | `mass-storage-gadget64/config.txt`, unchanged |
| `boot.img` | rebuilt (below) |

`boot.img` carries upstream's kernel, BCM2712 device trees and overlays. Only
the ramdisk is new: upstream's is 55 MB of systemd, LVM and flashrom, and the
BMC has about 24 MB of overlay. Here the ramdisk is Debian's static busybox,
the three modules the gadget needs (`dwc2`, `libcomposite`,
`usb_f_mass_storage`, from upstream's ramdisk so they match its kernel), and
`init`. `init` exports `/dev/mmcblk0` (and `/dev/nvme0n1` if present) with
each LUN's inquiry string set to the device name, which is how bmcd finds the
eMMC: SCSI vendor `mmcblk0`.

`MANIFEST` records the sources and the sha256 of every file. `install.sh`
checks the files against it on the BMC before swapping the directory in.

## Debugging

`boot-config.txt` puts the kernel console on GPIO14/15, which the Turing Pi 2
wires to the node's UART, and `init` writes each step to the kernel log as
well, so it reaches the UART even if what follows panics:

```sh
tpi uart -n 1 get | grep cm5-payload
```

A CM5 that takes its files but never shows a gadget, with nothing new on the
UART, did not start the kernel: the bootloader rejected `boot.img`. That is
how a missing `overlays/bcm2712d0.dtbo` looked.
