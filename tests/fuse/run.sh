#!/usr/bin/env bash
# Milestone 7 criterion: mount an image with dada-fuse and use it like a
# normal filesystem (cp -r, git clone, a build, rm -rf), then check it.
#
# Usage: tests/fuse/run.sh [target-dir]   (default: target/release)
# Needs: fusermount3, git, cargo.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
bin=${1:-$repo/target/release}
work=$(mktemp -d)
img=$work/fuse.img
mnt=$work/mnt
pid=

cleanup() {
    if mountpoint -q "$mnt" 2>/dev/null; then
        fusermount3 -u "$mnt" || true
    fi
    if [ -n "$pid" ]; then
        wait "$pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

step() { printf '\n== %s\n' "$*"; }

step "format and mount"
truncate -s 1G "$img"
"$bin/mkfs-dada" --label FUSETEST "$img"
mkdir "$mnt"
"$bin/dada-fuse" "$img" "$mnt" 2>"$work/fuse.log" &
pid=$!
for _ in $(seq 50); do
    mountpoint -q "$mnt" && break
    sleep 0.1
done
mountpoint -q "$mnt"

step "cp -r"
cp -r "$repo/crates" "$mnt/crates"
diff -r "$repo/crates" "$mnt/crates"

step "git clone"
git clone --quiet "$repo" "$mnt/clone"
git -C "$mnt/clone" fsck --no-progress 2>&1 | grep -v "^Checking" || true
test -z "$(git -C "$mnt/clone" status --porcelain)"
git -C "$mnt/clone" log --oneline -3

step "build a small project"
mkdir "$mnt/hello"
cat >"$mnt/hello/Cargo.toml" <<'EOF'
[package]
name = "hello"
version = "0.1.0"
edition = "2021"
EOF
mkdir "$mnt/hello/src"
echo 'fn main() { println!("hello from dada"); }' >"$mnt/hello/src/main.rs"
(cd "$mnt/hello" && CARGO_TARGET_DIR="$mnt/hello/target" cargo run --quiet) | grep -q "hello from dada"

step "rename, links, truncate, sparse file"
mv "$mnt/crates" "$mnt/moved"
ln "$mnt/moved/libdada/Cargo.toml" "$mnt/hard"
ln -s moved/libdada "$mnt/sym"
test -f "$mnt/sym/Cargo.toml"
truncate -s 10M "$mnt/sparse"
test "$(stat -c %s "$mnt/sparse")" = 10485760
printf 'end' | dd of="$mnt/sparse" bs=1 seek=5000000 conv=notrunc status=none
test "$(head -c 3 "$mnt/sparse" | od -An -tx1 | tr -d ' ')" = 000000

step "open file survives unlink"
exec 3<"$mnt/hard"
rm "$mnt/hard" "$mnt/moved/libdada/Cargo.toml"
grep -q libdada <&3
exec 3<&-

step "rm -rf"
rm -rf "$mnt/moved" "$mnt/clone" "$mnt/hello" "$mnt/sparse" "$mnt/sym"
test -z "$(ls -A "$mnt")"
df -h "$mnt" | tail -1

step "unmount and check"
fusermount3 -u "$mnt"
wait "$pid"
pid=
"$bin/fsck-dada" --verbose "$img"
"$bin/dadactl" info "$img" | grep -E "state|blocks|inodes"
echo
echo "FUSE test passed"
