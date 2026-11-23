#!/bin/sh
# Build the payload bmcd serves to a CM5 (BCM2712) over rpiboot.
#
# Everything comes from Raspberry Pi's usbboot at a pinned commit, checked by
# sha256: the chip files from firmware/bootfiles.bin, the rpiboot config.txt,
# and the kernel, device trees and gadget modules out of
# mass-storage-gadget64/boot.img. Only the ramdisk is rebuilt, around ./init
# and Debian's static busybox, because upstream's is 55 MB of systemd and the
# BMC has about 24 MB to spare.
#
# Needs curl, tar, xz, zstd, cpio, sfdisk, mkfs.fat, mtools, and apt with the
# arm64 architecture for busybox-static; run.sh runs this in a Debian
# container that has them.
#
#   build.sh [OUT_DIR]      default: ./out/rpiboot-2712
set -eu
export MTOOLS_SKIP_CHECK=1

USBBOOT_COMMIT=fdb604d018693465c9aa6ec69879df866d2820da
BASE="https://github.com/raspberrypi/usbboot/raw/$USBBOOT_COMMIT"
SHA_BOOTFILES=c51ef4d816bc2968ca5eef215ca58ce8e2cd4d68d75eae58b4d450f1b5d3d2a6
SHA_BOOT_IMG=e68d4614a0cbdb8404670e350b2ab261f4233dfba5e05e6d7dd691fd33bf254f
SHA_CONFIG=f74a9db07cd32f418e25754134840b138486e39cda2a4e536042b86c5c67b687

here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$here/out/rpiboot-2712}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fetch() { # path name sha256
    echo "fetching $1"
    curl -fsSL -o "$work/$2" "$BASE/$1"
    echo "$3  $work/$2" | sha256sum -c --quiet -
}
fetch firmware/bootfiles.bin bootfiles.bin "$SHA_BOOTFILES"
fetch mass-storage-gadget64/boot.img upstream.img "$SHA_BOOT_IMG"
fetch mass-storage-gadget64/config.txt config.txt "$SHA_CONFIG"

stage="$work/payload"
mkdir -p "$stage"
tar -xf "$work/bootfiles.bin" -C "$stage" 2712
cp "$work/config.txt" "$stage/config.txt"

# The FAT filesystem in upstream's image starts where its one MBR partition
# does.
start=$(od -An -t u4 -j 454 -N 4 "$work/upstream.img" | tr -d ' ')
up="$work/upstream.img@@$((start * 512))"
mkdir -p "$work/boot" "$work/upstream-root"
mcopy -n -i "$up" ::kernel8.img ::rootfs.cpio.zst "$work/"
mcopy -n -i "$up" "::bcm2712*.dtb" "$work/boot/"
# All of them, as upstream ships them. The firmware applies more than config.txt
# names: on a D0-stepping BCM2712, which a CM5 is, it adds bcm2712d0.dtbo to the
# base tree itself, and overlay_map sends many names to their -pi5 variants.
# With only dwc2.dtbo the CM5 bootloader never started the kernel.
mcopy -s -n -i "$up" ::overlays "$work/boot/"
mv "$work/kernel8.img" "$work/boot/"

# The ramdisk: a static busybox, the three modules the gadget needs, and
# ./init. Static, because PID 1 cannot report that its loader found no
# libraries: upstream's dynamic busybox ran in a chroot and died with 127 as
# init, its error lost in the panic that follows.
(cd "$work" && apt-get download -q=2 busybox-static:arm64)
busybox_deb=$(ls "$work"/busybox-static_*_arm64.deb)
dpkg-deb -x "$busybox_deb" "$work/busybox-static"
(cd "$work/upstream-root" && zstd -dc "$work/rootfs.cpio.zst" | cpio -idm --quiet)
kver=$(ls "$work/upstream-root/usr/lib/modules")
mods="$work/upstream-root/usr/lib/modules/$kver/kernel/drivers/usb"
r="$work/ramfs"
mkdir -p "$r/bin" "$r/lib/modules" "$r/dev" "$r/proc" "$r/sys"
install -m 0755 "$work/busybox-static/bin/busybox" "$r/bin/busybox"
for a in sh mount mkdir sleep ls ln insmod sed; do ln -s busybox "$r/bin/$a"; done
xz -dc "$mods/dwc2/dwc2.ko.xz" > "$r/lib/modules/dwc2.ko"
xz -dc "$mods/gadget/libcomposite.ko.xz" > "$r/lib/modules/libcomposite.ko"
xz -dc "$mods/gadget/function/usb_f_mass_storage.ko.xz" > "$r/lib/modules/usb_f_mass_storage.ko"
install -m 0755 "$here/init" "$r/init"

# Run the ramdisk's own shell in a chroot, and check the applets ./init calls
# are in this busybox. Only an arm64 host can.
if [ "$(uname -m)" = aarch64 ]; then
    have=$(chroot "$r" /bin/sh -c 'sh -n /init && busybox --list') || {
        echo "the ramdisk's shell does not run in a chroot" >&2
        exit 1
    }
    for a in sh mount mkdir sleep ls ln insmod sed; do
        echo "$have" | grep -qx "$a" || { echo "busybox has no $a" >&2; exit 1; }
    done
    echo "the ramdisk's shell runs, and busybox has every applet init uses"
fi
mknod -m 0600 "$r/dev/console" c 5 1
mknod -m 0644 "$r/dev/kmsg" c 1 11
(cd "$r" && find . | sort | cpio -o -H newc --owner 0:0 --quiet) |
    zstd -q -19 > "$work/boot/rootfs.cpio.zst"

cp "$here/boot-config.txt" "$work/boot/config.txt"
echo "rootwait console=tty1 console=serial0,115200 root=/dev/ram0 loglevel=7" \
    > "$work/boot/cmdline.txt"

# boot.img as upstream makes it: an MBR with one bootable type-0xC partition
# from sector 1, FAT16 inside with upstream's 2 KiB clusters and 4 reserved
# sectors, sized to what it holds plus room for the FAT. The geometry is not
# taste: the CM5 bootloader would not boot the same files from a FAT16 with
# 1 KiB clusters.
kib=$(du -sk "$work/boot" | cut -f1)
sectors=$(((kib + 2048) * 2))
img="$stage/boot.img"
truncate -s $(((sectors + 1) * 512)) "$img"
echo "1,$sectors,c,*" | sfdisk -q "$img"
mkfs.fat -F 16 -s 4 -R 4 --offset 1 -n CM5MSD "$img" "$((sectors / 2))" > /dev/null
mcopy -s -i "$img@@512" "$work/boot/"* ::

{
    echo "rpiboot-2712 payload for bmcd"
    echo "usbboot $USBBOOT_COMMIT"
    echo "busybox $(basename "$busybox_deb") $(sha256sum "$busybox_deb" | cut -d' ' -f1)"
    echo "kernel $kver"
    echo
    (cd "$stage" && find . -type f ! -name MANIFEST | sort | xargs sha256sum)
} > "$stage/MANIFEST"

rm -rf "$out"
mkdir -p "$(dirname "$out")"
cp -r "$stage" "$out"
echo
echo "payload in $out ($(du -sh "$out" | cut -f1)):"
(cd "$out" && find . -type f | sort | xargs ls -l | awk '{printf "  %9d  %s\n", $5, $9}')
