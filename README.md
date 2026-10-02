# dada

dada is a portable filesystem for USB keys, external drives and disk images,
readable and writable from Linux, macOS and Windows.

- One on-disk format, specified in [SPEC.md](SPEC.md), independent of the machine.
- One core library, `libdada`, written in Rust with no OS-specific code.
- A thin adapter per OS: FUSE on Linux and macOS, WinFsp on Windows.

Features: files, directories and symbolic links; UTF-8 names up to 255 bytes;
64-bit block addresses; nanosecond UTC timestamps; POSIX permissions plus
Windows attributes; CRC32C on all metadata; a metadata journal; optional
case-insensitive names.

## Status

| Platform | Adapter | State |
|---|---|---|
| Linux | `dada-fuse` (fuse3) | Tested: mount, `cp -r`, `git clone`, a Cargo build, `rm -rf`, then `fsck-dada` clean |
| macOS | `dada-fuse` (macFUSE or FUSE-T) | Builds in CI; not tested on a mounted volume yet |
| Windows | `dada-winfsp` (WinFsp) | Type-checked against the WinFsp bindings; not run on Windows yet |

Everything is done with one command, `dada`, which works on all three
systems (mounting is done by `dada-winfsp` on Windows). The separate tools
`mkfs-dada`, `fsck-dada`, `dadactl` and `dada-fuse` remain available.

## Quick start (Linux)

```sh
sudo apt install fuse3
cargo build --release -p dada --features fuse
sudo ./target/release/dada setup          # once: installs dada and automatic mounting

lsblk -o NAME,SIZE,MODEL,TRAN              # find the key, say /dev/sdb1
sudo dada format --label MYKEY /dev/sdb1   # asks before erasing
```

Unplug the key and plug it in again: it is mounted automatically under
`/media/<you>/MYKEY` and appears in the file manager, like any other key.
Eject it from the file manager, or with `sudo dada umount /dev/sdb1`.

By hand:

```sh
sudo dada mount /dev/sdb1                  # on /media/<you>/MYKEY, files owned by you
sudo dada umount /dev/sdb1                 # returns once everything is written
dada check /dev/sdb1                       # needs read access to the device
```

## The `dada` command

| Command | Does |
|---|---|
| `dada format [options] <target>` | format an image or a device (options below); asks before erasing data, `--force` skips the question |
| `dada mount [-o options] <source> [<dir>]` | mount in the background; without `<dir>`, as root: `/media/<user>/<label>` |
| `dada umount <dir or device>` | unmount and wait until everything is written |
| `dada check [--repair] <target>` | check (and repair) a volume (alias `fsck`) |
| `dada probe <device>` | tell whether a device holds a dada volume |
| `dada info`, `ls`, `cat`, `stat`, `put`, `get`, `mkdir`, `rm`, `dump` | inspect or change an unmounted volume, as `dadactl` |
| `sudo dada setup [--uninstall]` | install (remove) automatic mounting, Linux only |

`dada format` and `dada check` refuse a volume that is mounted.

## Installation

Build from source with a stable Rust toolchain (<https://rustup.rs>).

```sh
cargo build --release -p dada                      # without mounting
cargo build --release -p dada --features fuse      # Linux, with mounting
cargo build --release -p dada --features macos     # macOS, with mounting
```

The binaries land in `target/release/`. The separate tools are built with
`cargo build --release -p mkfs-dada -p fsck-dada -p dadactl`.

### Linux

```sh
sudo apt install fuse3            # or your distribution's fuse3 package
cargo build --release -p dada --features fuse
sudo ./target/release/dada setup
```

No libfuse headers are needed. `dada setup` installs:

- `/usr/local/bin/dada`;
- the `mount(8)` helpers `/sbin/mount.dada` and `/sbin/umount.fuseblk.dada`,
  so that `mount -t dada` and fstab entries work;
- a udev rule, `/etc/udev/rules.d/61-dada.rules`, which identifies dada
  volumes: `lsblk -f` shows their type, label and UUID, and
  `/dev/disk/by-label/<label>` exists;
- udisks options in `/etc/udisks2/mount_options.conf`, so that a key
  mounted automatically belongs to the user who plugged it in.

`sudo dada setup --uninstall` removes all of it.

As root, a device is mounted as `fuseblk`, the way NTFS keys are: the
kernel ties the mount to the device, and unmounting waits for dada to
write everything. GParted and `blkid` still show the volume as unknown:
they would need dada support upstream (util-linux and GParted).

### macOS

Install [macFUSE](https://osxfuse.github.io/) or [FUSE-T](https://www.fuse-t.org/), then:

```sh
cargo build --release -p dada --features macos
```

### Windows

Install [WinFsp](https://winfsp.dev/) with its "Developer" component, then:

```sh
cargo build --release -p dada-winfsp --features winfsp
```

## Formatting

```sh
truncate -s 1G key.img                 # or use an existing device
dada format --label MYKEY key.img      # or: mkfs-dada --label MYKEY key.img
```

Options:

| Option | Meaning | Default |
|---|---|---|
| `--block-size N` | 1024 to 65536, a power of two | 4096 |
| `--label L` | up to 32 bytes of UTF-8 | empty |
| `--casefold` | names compared without regard to case (recommended for Windows) | off |
| `--no-journal` | no metadata journal | journal on |
| `--inode-ratio N` | bytes of volume per inode | 16384 |
| `--root-owner UID:GID` | owner of the root directory | the user who ran sudo, else the owner of the target |
| `--force` | overwrite a non-empty target, or format a `/dev/...` or `\\.\...` path | off |

A target whose first MiB or last 64 KiB holds data, or any device path, is
formatted only with `--force`, or after confirmation with `dada format`.
**Formatting a device erases it.**

The tools are named `mkfs-dada` and `fsck-dada`, with a hyphen: Cargo does
not allow `.` in binary names, so `mkfs -t dada` and `fsck -t dada` do not
find them. Call them directly.

## Mounting

### Linux and macOS

```sh
dada mount key.img mnt              # in the background; creates mnt if needed
# ... use mnt ...
dada umount mnt                     # returns once everything is written
```

`dada mount --foreground` (or `dada-fuse key.img mnt`) stays in the
foreground and logs to the terminal; unmount it with `fusermount3 -u mnt`
(Linux) or `umount mnt` (macOS), then wait for it to exit. In the
background, logs go to the system journal: `journalctl -t dada`.

Options (`-o`, comma-separated):

- `ro`: read-only;
- `allow_other`: let other users access the mount (needs `user_allow_other`
  in `/etc/fuse.conf`; always on when mounted by root);
- `uid=N,gid=N`: show every file as owned by this user ("USB key" mode),
  without changing what is on disk; `sudo dada mount` uses the user who
  ran sudo;
- `noexec`, `suid`, `dev`: as for any filesystem (default: exec, nosuid,
  nodev).

Permissions are checked by the kernel (`default_permissions`). Unmounting
writes everything and marks the volume clean: wait for `dada umount` to
return before removing the key.

With `dada setup` done, `/etc/fstab` entries work too:

```
/dev/disk/by-label/MYKEY  /mnt/key  dada  uid=1000,gid=1000,nofail  0  0
```

### Windows

```sh
dada-winfsp key.img X:               # or a directory as mount point
```

Press Enter in the console to unmount. Without a console (service, script),
the volume stays mounted until the process is stopped. `--read-only` mounts
read-only.

- A volume formatted without `--casefold` is exposed as case-sensitive, which
  some Windows programs do not expect; a warning is logged.
- Characters Windows forbids in names (`\ : * ? " < > |` and control
  characters) are shown as private-use characters U+F000 + code, like
  Cygwin does, and mapped back on write. Reserved names (`CON`, `NUL`,
  `COM1`...) cannot be created from Windows; existing ones are shown with
  their first character escaped the same way.
- The read-only attribute follows the owner's write permission; hidden,
  system and archive are stored in the inode.
- Every file appears as owned by the current user, with full access.
- Symbolic links appear as symlink reparse points. Creating symbolic links
  from Windows is not supported yet.

## Checking and repairing

```sh
dada check key.img               # check only, writes nothing
dada check --repair key.img      # fix what can be fixed
```

(`fsck-dada` takes the same options.)

`fsck-dada` replays the journal, checks the superblocks, every inode and
directory block checksum, the tree from the root, extents (bounds and
overlaps), bitmaps, link counts and free counters, and attaches unreferenced
inodes to `/lost+found`. It never needs the volume to be mounted.

Exit codes: 0 clean, 1 errors fixed, 4 errors left, 8 runtime error.

## Inspecting an image

```sh
dadactl info  key.img
dadactl ls    key.img /docs
dadactl stat  key.img /docs/notes.txt
dadactl cat   key.img /docs/notes.txt
dadactl put   key.img notes.txt /docs/notes.txt
dadactl get   key.img /docs/notes.txt copy.txt
dadactl mkdir key.img /docs
dadactl rm    key.img /docs/notes.txt
dadactl dump  key.img superblock | inode N | block N
```

## Known limits

- Not in version 1: encryption, compression, snapshots, deduplication,
  full NTFS ACLs, data checksums (only metadata is checksummed), resizing,
  kernel drivers, extended attributes.
- Directories are searched linearly: very large directories (tens of
  thousands of entries) are slow.
- The adapters serve one request at a time.
- A write that does not fit is refused as a whole rather than written in part.
- A file deleted while open is moved to a hidden root directory,
  `.dada-unlinked`, and removed on its last close, or at the next mount after
  a crash.
- Names must be valid UTF-8; they are stored in Unicode NFC form.
- Only metadata goes through the journal: after a crash, the content of a
  file being overwritten in place may mix old and new data. Everything
  validated by a `sync` (or `fsync`, or a clean unmount) is durable.
- `pjdfstest` has not been run yet.
- `blkid` and GParted do not recognise dada volumes; `lsblk`, udisks and
  file managers do once `dada setup` is done.

## Development

```sh
cargo test --workspace                                 # fast tests
cargo test --release --workspace -- --ignored          # long tests (several GiB of I/O)
tests/fuse/run.sh                                      # FUSE end-to-end test (Linux)
cd fuzz && cargo +nightly fuzz run open_image          # fuzzing (cargo install cargo-fuzz)
```

Random tests print their seed; replay a failure with `DADA_SEED=<seed>`.
`cargo fmt` and `cargo clippy --all-targets -- -D warnings` must pass before
every commit.

## License

Copyright (C) 2026 mawena.

dada is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License, version 3, as published by the Free
Software Foundation. It is distributed without any warranty; see
[LICENSE](LICENSE) for the full text.

The Windows adapter links the WinFsp bindings (`winfsp` crate, GPL-3.0) and
needs [WinFsp](https://winfsp.dev/) at run time. The macOS adapter needs
macFUSE or FUSE-T, which have their own licenses.
