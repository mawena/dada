#!/usr/bin/env bash
# The dada command as a user: format, mount in the background, use, unmount,
# check, and the refusals that protect a mounted volume.
#
# Usage: tests/fuse/dada-cli.sh [target-dir]   (default: target/release)
# Needs: fusermount3, and dada built with `--features fuse`.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
bin=${1:-$repo/target/release}
dada=$bin/dada
work=$(mktemp -d)
img=$work/key.img
mnt=$work/mnt

cleanup() {
    if mountpoint -q "$mnt" 2>/dev/null; then
        "$dada" umount "$mnt" || fusermount3 -u "$mnt" || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

step "format and probe"
truncate -s 256M "$img"
"$dada" format --label "Ma clé" "$img"
"$dada" probe "$img"
"$dada" probe --udev "$img" | grep -qx 'ID_FS_TYPE=dada'
if "$dada" format "$img" </dev/null; then fail "formatted a volume holding data without --force"; fi

step "mount in the background"
"$dada" mount "$img" "$mnt"
mountpoint -q "$mnt"
grep -q " fuse.dada " /proc/self/mountinfo
cp -r "$repo/crates" "$mnt/"
head -c 30000000 /dev/urandom >"$mnt/random.bin"
sum=$(sha256sum <"$mnt/random.bin")

step "refusals while mounted"
if "$dada" mount "$img" "$work/other"; then fail "mounted twice"; fi
if "$dada" format --force "$img"; then fail "formatted a mounted volume"; fi
if "$dada" check "$img"; then fail "checked a mounted volume"; fi

step "unmount"
"$dada" umount "$mnt"
if mountpoint -q "$mnt"; then fail "still mounted"; fi
if pgrep -f "dada mount --foreground .*$mnt" >/dev/null; then fail "daemon still running"; fi

step "check and read back"
"$dada" check --verbose "$img"
"$dada" info "$img" | grep -q "state: *clean"
[ "$("$dada" cat "$img" /random.bin | sha256sum)" = "$sum" ] || fail "content differs"
"$dada" mount -o ro "$img" "$mnt"
diff -r "$repo/crates" "$mnt/crates"
"$dada" umount "$mnt"

echo
echo "dada command: OK"
