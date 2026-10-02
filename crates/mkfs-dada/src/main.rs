//! mkfs-dada: formats an image file or a device with the dada filesystem.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use libdada::format::{DEFAULT_BLOCK_SIZE, DEFAULT_INODE_RATIO, LABEL_LEN};
use libdada::{format, BlockDevice, DadaError, FileDevice, FormatOptions, Volume};

/// Bytes inspected at the start and at the end of the target to decide
/// whether it is empty.
const PROBE_HEAD: u64 = 1 << 20;
const PROBE_TAIL: u64 = 64 << 10;

#[derive(Parser)]
#[command(
    name = "mkfs-dada",
    version,
    about = "Format an image file or a device with the dada filesystem"
)]
struct Args {
    /// Block size in bytes: a power of two from 1024 to 65536
    #[arg(long, default_value_t = DEFAULT_BLOCK_SIZE)]
    block_size: u32,
    /// Volume label, at most 32 bytes of UTF-8
    #[arg(long, default_value = "")]
    label: String,
    /// Compare names without regard to case
    #[arg(long)]
    casefold: bool,
    /// Do not create a metadata journal
    #[arg(long)]
    no_journal: bool,
    /// Bytes of volume per inode
    #[arg(long, default_value_t = DEFAULT_INODE_RATIO)]
    inode_ratio: u64,
    /// Overwrite a non-empty target, or format a device path
    #[arg(long)]
    force: bool,
    /// Image file or device to format (an image file must already exist with its final size)
    target: PathBuf,
}

fn main() -> ExitCode {
    match run(&Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mkfs-dada: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Paths that name a raw device rather than an image file.
fn is_device_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with("/dev/") || s.starts_with(r"\\.\")
}

/// Whether the first `PROBE_HEAD` and last `PROBE_TAIL` bytes are all zero.
/// Existing filesystems and partition tables keep their signatures there.
fn looks_empty(file: &mut File) -> std::io::Result<bool> {
    let len = file.seek(SeekFrom::End(0))?;
    let ranges = [
        (0, len.min(PROBE_HEAD)),
        (len.saturating_sub(PROBE_TAIL), len.min(PROBE_TAIL)),
    ];
    let mut buf = vec![0u8; 64 << 10];
    for (start, size) in ranges {
        file.seek(SeekFrom::Start(start))?;
        let mut left = size;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            let chunk = &mut buf[..n];
            file.read_exact(chunk)?;
            if chunk.iter().any(|&b| b != 0) {
                return Ok(false);
            }
            left -= n as u64;
        }
    }
    Ok(true)
}

fn run(args: &Args) -> Result<(), String> {
    let target = args.target.display();
    if is_device_path(&args.target) && !args.force {
        return Err(format!(
            "{target} looks like a device; use --force to format it"
        ));
    }
    if args.label.len() > LABEL_LEN || args.label.contains('\0') {
        return Err(format!(
            "label must be at most {LABEL_LEN} bytes without NUL"
        ));
    }
    let opts = FormatOptions {
        block_size: args.block_size,
        inode_ratio: args.inode_ratio,
        label: args.label.clone(),
        casefold: args.casefold,
        journal: !args.no_journal,
    };
    if !libdada::format::is_valid_block_size(opts.block_size) {
        return Err(format!(
            "invalid block size {}: use a power of two from 1024 to 65536",
            opts.block_size
        ));
    }
    if opts.inode_ratio == 0 {
        return Err("inode ratio must be positive".into());
    }

    let mut file = File::options()
        .read(true)
        .write(true)
        .open(&args.target)
        .map_err(|e| format!("cannot open {target}: {e}"))?;
    if !args.force && !looks_empty(&mut file).map_err(|e| format!("cannot read {target}: {e}"))? {
        return Err(format!(
            "{target} is not empty; use --force to overwrite its content"
        ));
    }

    let mut dev = FileDevice::from_file(file, opts.block_size, true)
        .map_err(|e| format!("cannot use {target}: {e}"))?;
    format(&mut dev, &opts).map_err(|e| match e {
        DadaError::NoSpace => format!(
            "{target} is too small ({} blocks of {} bytes)",
            dev.block_count(),
            opts.block_size
        ),
        e => format!("formatting {target} failed: {e}"),
    })?;

    let vol = Volume::open(dev, true).map_err(|e| format!("cannot reopen {target}: {e}"))?;
    let sb = vol.superblock();
    let st = vol.statfs();
    println!("formatted {target}");
    println!("  label        {}", sb.label().unwrap_or("?"));
    println!("  block size   {}", st.block_size);
    println!(
        "  blocks       {} ({} free)",
        st.total_blocks, st.free_blocks
    );
    println!(
        "  inodes       {} ({} free)",
        st.total_inodes, st.free_inodes
    );
    if sb.journal_blocks > 0 {
        println!("  journal      {} blocks", sb.journal_blocks);
    }
    if args.casefold {
        println!("  casefold     on");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn device_paths() {
        assert!(is_device_path(Path::new("/dev/sdb")));
        assert!(is_device_path(Path::new(r"\\.\PhysicalDrive1")));
        assert!(!is_device_path(Path::new("t.img")));
        assert!(!is_device_path(Path::new("/tmp/dev/t.img")));
    }

    #[test]
    fn emptiness() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.img");
        let f = File::create(&path).unwrap();
        f.set_len(4 << 20).unwrap();
        let mut f = File::options().read(true).write(true).open(&path).unwrap();
        assert!(looks_empty(&mut f).unwrap());

        f.seek(SeekFrom::Start((4 << 20) - 10)).unwrap();
        f.write_all(&[1]).unwrap();
        assert!(!looks_empty(&mut f).unwrap());

        let mut small = File::create(dir.path().join("s.img")).unwrap();
        small.write_all(&[0; 100]).unwrap();
        let mut small = File::open(dir.path().join("s.img")).unwrap();
        assert!(looks_empty(&mut small).unwrap());
    }
}
