#!/bin/sh
# Copy a built payload to a BMC, replacing any previous one only once the new
# one is completely there.
#
#   install.sh root@turingpi.local [PAYLOAD_DIR]
set -eu
host=${1:?usage: install.sh root@BMC [PAYLOAD_DIR]}
here=$(cd "$(dirname "$0")" && pwd)
src=${2:-$here/out/rpiboot-2712}
dest=/mnt/overlay/rpiboot-2712
[ -f "$src/boot.img" ] || { echo "no payload in $src; run ./run.sh first" >&2; exit 1; }

need=$(du -sk "$src" | cut -f1)
free=$(ssh "$host" "df -k /mnt/overlay | awk 'NR==2 {print \$4}'")
[ "$free" -gt "$need" ] || { echo "the overlay has ${free} KiB free, the payload needs ${need} KiB" >&2; exit 1; }

ssh "$host" "rm -rf $dest.new"
scp -rq "$src" "$host:$dest.new"
ssh "$host" "cd $dest.new && sed -n '/^\$/,\$p' MANIFEST | grep . | sha256sum -c >/dev/null && rm -rf $dest && mv $dest.new $dest && sync"
echo "installed in $host:$dest"
