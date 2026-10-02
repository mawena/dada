# dada

dada is a portable filesystem for USB keys, external drives and disk images,
readable and writable from Linux, macOS and Windows.

- `crates/libdada`: OS-independent core library (Rust).
- `crates/mkfs-dada`, `crates/fsck-dada`, `crates/dadactl`: command-line tools.
- `crates/dada-fuse`: FUSE adapter (Linux, macOS).
- `crates/dada-winfsp`: WinFsp adapter (Windows).

The on-disk format is specified in [SPEC.md](SPEC.md).

The tools are named `mkfs-dada` and `fsck-dada` (with a hyphen, not
`mkfs.dada` / `fsck.dada`), because Cargo does not allow `.` in binary names.
As a result, `mkfs -t dada` and `fsck -t dada` on Linux do not find them:
call them directly.

Status: under development (milestone 3: files and directories on images,
no FUSE or WinFsp mount yet).

## Build

```sh
cargo build --workspace
cargo test --workspace
```

Long tests (several GiB of I/O) are ignored by default:

```sh
cargo test --release --workspace -- --ignored
```

Random tests print their seed; replay a failure with `DADA_SEED=<seed>`.

## Try it on an image

```sh
truncate -s 100M t.img
mkfs-dada --label TEST t.img
dadactl mkdir t.img /docs
dadactl put   t.img notes.txt /docs/notes.txt
dadactl ls    t.img /docs
dadactl cat   t.img /docs/notes.txt
dadactl get   t.img /docs/notes.txt copy.txt
dadactl stat  t.img /docs/notes.txt
dadactl rm    t.img /docs/notes.txt
dadactl info  t.img
dadactl dump  t.img superblock
```
