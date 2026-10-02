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

Status: under development (milestone 0).

## Build

```sh
cargo build --workspace
cargo test --workspace
```
