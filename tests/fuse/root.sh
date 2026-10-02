#!/usr/bin/env bash
# The dada command as root, on a loop device backed by an image in a
# temporary directory (never a real disk): fuseblk mounts, `sudo dada mount`
# ownership, and with --with-setup the system integration (udev rule,
# `mount -t dada`, `umount` helper), which is removed at the end.
#
# Usage: sudo tests/fuse/root.sh [--with-setup] [target-dir]
# Needs: root, losetup, fuse3, and dada built with `--features fuse`.
set -euo pipefail

with_setup=
if [ "${1:-}" = --with-setup ]; then
    with_setup=1
    shift
fi
repo=$(cd "$(dirname "$0")/../.." && pwd)
bin=${1:-$repo/target/release}
dada=$bin/dada
[ "$(id -u)" = 0 ] || { echo "run as root: sudo $0" >&2; exit 1; }

work=$(mktemp -d)
img=$work/key.img
mnt=$work/mnt
loop=

cleanup() {
    if mountpoint -q "$mnt" 2>/dev/null; then
        "$dada" umount "$mnt" || umount "$mnt" || true
    fi
    if [ -n "$loop" ]; then
        losetup -d "$loop" || true
    fi
    if [ -n "$with_setup" ]; then
        /usr/local/bin/dada setup --uninstall || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

truncate -s 256M "$img"
loop=$(losetup --find --show "$img")
case $loop in /dev/loop*) ;; *) fail "unexpected loop device $loop" ;; esac
echo "loop device: $loop"

step "format"
SUDO_UID=4242 SUDO_GID=4243 "$dada" format --force --label ROOTTEST "$loop"
"$dada" probe "$loop"

step "fuseblk mount"
SUDO_UID=4242 SUDO_GID=4243 "$dada" mount "$loop" "$mnt"
line=$(grep " $mnt " /proc/self/mountinfo) || fail "not mounted"
echo "$line"
case $line in *" - fuseblk.dada $loop "*) ;; *) fail "not a fuseblk.dada mount of $loop" ;; esac
majmin=$(printf '%d:%d' "0x$(stat -c %t "$loop")" "0x$(stat -c %T "$loop")")
[ "$(echo "$line" | cut -d' ' -f3)" = "$majmin" ] || fail "mount not tied to $majmin"
[ "$(stat -c %u:%g "$mnt")" = 4242:4243 ] || fail "root not shown as owned by the sudo user"
if "$dada" format --force "$loop"; then fail "formatted a mounted device"; fi
cp -r "$repo/crates" "$mnt/"
head -c 30000000 /dev/urandom >"$mnt/random.bin"
sum=$(sha256sum <"$mnt/random.bin")

step "unmount by device"
"$dada" umount "$loop"
if mountpoint -q "$mnt"; then fail "still mounted"; fi
"$dada" check "$loop"
[ "$("$dada" cat "$loop" /random.bin | sha256sum)" = "$sum" ] || fail "content differs"

if [ -n "$with_setup" ]; then
    step "dada setup"
    "$dada" setup
    udevadm trigger --action=change "$loop"
    udevadm settle
    lsblk -no FSTYPE,LABEL "$loop"
    [ "$(lsblk -no FSTYPE "$loop")" = dada ] || fail "lsblk does not see dada"
    [ -e /dev/disk/by-label/ROOTTEST ] || fail "no /dev/disk/by-label/ROOTTEST"

    step "mount -t dada and umount through the helpers"
    mount -t dada -o uid=1000,gid=1000,nosuid,nodev "$loop" "$mnt"
    grep -q " $mnt .* - fuseblk.dada " /proc/self/mountinfo || fail "mount -t dada failed"
    diff -r "$repo/crates" "$mnt/crates"
    umount "$mnt"
    if mountpoint -q "$mnt"; then fail "still mounted"; fi
    "$dada" check "$loop"
    "$dada" info "$loop" | grep -q "state: *clean" || fail "not clean after umount"
fi

echo
echo "dada as root: OK"
