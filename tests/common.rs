//! Helpers shared by the integration tests.

use std::fs::File;
use std::path::{Path, PathBuf};

use libdada::{format, FileDevice, FormatOptions};
use tempfile::TempDir;

/// Creates a zero-filled image of `bytes` bytes in `dir`.
pub fn create_image(dir: &TempDir, name: &str, bytes: u64) -> PathBuf {
    let path = dir.path().join(name);
    File::create(&path)
        .and_then(|f| f.set_len(bytes))
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", path.display()));
    path
}

/// Formats the image at `path` with `opts`.
pub fn format_image(path: &Path, opts: &FormatOptions) {
    let mut dev = FileDevice::open(path, opts.block_size, true)
        .unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));
    format(&mut dev, opts).unwrap_or_else(|e| panic!("cannot format {}: {e}", path.display()));
}
