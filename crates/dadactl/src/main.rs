//! dadactl: inspect and manipulate dada images.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use libdada::format::{
    COMPAT_WIN_ATTRS, COMPAT_XATTR, FORMAT_VERSION, INCOMPAT_CASEFOLD, INCOMPAT_EXTENT_BLOCKS,
    INCOMPAT_JOURNAL, STATE_CLEAN, SUPERBLOCK_SIZE,
};
use libdada::superblock::Superblock;
use libdada::{Attr, BlockDevice, DadaError, FileDevice, FileKind, Ino, SetAttr, Volume};

/// Size of the chunks copied between the image and local files.
const CHUNK: usize = 1 << 20;

#[derive(Parser)]
#[command(
    name = "dadactl",
    version,
    about = "Inspect and manipulate dada images"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the volume summary
    Info { image: PathBuf },
    /// List a directory
    Ls { image: PathBuf, path: String },
    /// Print a file to standard output
    Cat { image: PathBuf, path: String },
    /// Show the attributes of a file, directory or symbolic link
    Stat { image: PathBuf, path: String },
    /// Copy a local file into the image, replacing an existing file
    Put {
        image: PathBuf,
        local: PathBuf,
        path: String,
    },
    /// Copy a file of the image to a local file
    Get {
        image: PathBuf,
        path: String,
        local: PathBuf,
    },
    /// Create a directory
    Mkdir { image: PathBuf, path: String },
    /// Remove a file, a symbolic link or an empty directory
    Rm { image: PathBuf, path: String },
    /// Dump an on-disk structure
    Dump {
        image: PathBuf,
        #[command(subcommand)]
        what: DumpTarget,
    },
}

#[derive(Subcommand)]
enum DumpTarget {
    /// Primary superblock, decoded even if invalid
    Superblock,
    /// Decoded inode
    Inode { n: u64 },
    /// Raw block, in hexadecimal
    Block { n: u64 },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Info { image } => info(&image),
        Command::Ls { image, path } => ls(&image, &path),
        Command::Cat { image, path } => cat(&image, &path),
        Command::Stat { image, path } => stat(&image, &path),
        Command::Put { image, local, path } => put(&image, &local, &path),
        Command::Get { image, path, local } => get(&image, &path, &local),
        Command::Mkdir { image, path } => mkdir(&image, &path),
        Command::Rm { image, path } => rm(&image, &path),
        Command::Dump { image, what } => dump(&image, what),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dadactl: {e}");
            ExitCode::FAILURE
        }
    }
}

fn open_device(image: &Path) -> Result<FileDevice, String> {
    FileDevice::open_image(image, false).map_err(|e| format!("{}: {e}", image.display()))
}

fn open_volume(image: &Path) -> Result<Volume<FileDevice>, String> {
    Volume::open(open_device(image)?, true).map_err(|e| format!("{}: {e}", image.display()))
}

/// Opens the image read-write, runs `op`, then closes the volume cleanly.
fn with_writable<T>(
    image: &Path,
    op: impl FnOnce(&mut Volume<FileDevice>) -> Result<T, String>,
) -> Result<T, String> {
    let dev =
        FileDevice::open_image(image, true).map_err(|e| format!("{}: {e}", image.display()))?;
    let mut vol = Volume::open(dev, false).map_err(|e| format!("{}: {e}", image.display()))?;
    let result = op(&mut vol);
    vol.close()
        .map_err(|e| format!("{}: closing failed: {e}", image.display()))?;
    result
}

/// Splits `/a/b/c` into the attributes of `/a/b` and the name `c`.
fn resolve_parent<'p>(
    vol: &mut Volume<FileDevice>,
    path: &'p str,
) -> Result<(Ino, &'p str), String> {
    let trimmed = path.trim_end_matches('/');
    let (parent, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
    if name.is_empty() {
        return Err(format!("{path}: invalid path"));
    }
    let parent = resolve(vol, parent)?;
    if parent.kind != FileKind::Directory {
        return Err(format!("{path}: not a directory"));
    }
    Ok((parent.ino, name))
}

/// Resolves an absolute path from the root, following no symbolic links.
fn resolve(vol: &mut Volume<FileDevice>, path: &str) -> Result<Attr, String> {
    let mut attr = vol.getattr(vol.root()).map_err(|e| e.to_string())?;
    for name in path.split('/').filter(|c| !c.is_empty()) {
        attr = vol.lookup(attr.ino, name).map_err(|e| match e {
            DadaError::NotFound => format!("{path}: no such file or directory"),
            DadaError::NotDir => format!("{path}: not a directory"),
            e => format!("{path}: {e}"),
        })?;
    }
    Ok(attr)
}

fn kind_char(kind: FileKind) -> char {
    match kind {
        FileKind::Directory => 'd',
        FileKind::RegularFile => '-',
        FileKind::Symlink => 'l',
    }
}

/// `2026-10-02T12:34:56.123456789Z` from nanoseconds since the epoch (UTC).
fn format_time(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let nanos = ns.rem_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (proleptic Gregorian calendar).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{nanos:09}Z",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

fn feature_names(bits: u64, names: &[(u64, &str)]) -> String {
    let mut out: Vec<String> = names
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| (*name).to_owned())
        .collect();
    let unknown = bits & !names.iter().fold(0, |acc, (bit, _)| acc | bit);
    if unknown != 0 {
        out.push(format!("unknown({unknown:#x})"));
    }
    if out.is_empty() {
        "-".into()
    } else {
        out.join(" ")
    }
}

fn info(image: &Path) -> Result<(), String> {
    let vol = open_volume(image)?;
    let sb = vol.superblock();
    let st = vol.statfs();
    let uuid = uuid::Uuid::from_bytes(sb.uuid);
    let state = if sb.state == STATE_CLEAN {
        "clean"
    } else {
        "dirty"
    };
    println!("label:           {}", sb.label().unwrap_or("?"));
    println!("uuid:            {uuid}");
    println!("format version:  {FORMAT_VERSION}");
    println!("state:           {state}");
    println!("block size:      {}", st.block_size);
    println!(
        "blocks:          {} total, {} used, {} free",
        st.total_blocks,
        st.total_blocks - st.free_blocks,
        st.free_blocks
    );
    println!(
        "inodes:          {} total, {} used, {} free",
        st.total_inodes,
        st.total_inodes - st.free_inodes,
        st.free_inodes
    );
    println!(
        "incompat:        {}",
        feature_names(
            sb.features_incompat,
            &[
                (INCOMPAT_CASEFOLD, "casefold"),
                (INCOMPAT_JOURNAL, "journal"),
                (INCOMPAT_EXTENT_BLOCKS, "extent_blocks"),
            ]
        )
    );
    println!(
        "compat:          {}",
        feature_names(
            sb.features_compat,
            &[(COMPAT_XATTR, "xattr"), (COMPAT_WIN_ATTRS, "win_attrs")]
        )
    );
    println!("block bitmap:    block {}", sb.block_bitmap_start);
    println!("inode bitmap:    block {}", sb.inode_bitmap_start);
    println!("inode table:     block {}", sb.inode_table_start);
    if sb.journal_blocks > 0 {
        println!(
            "journal:         block {} ({} blocks)",
            sb.journal_start, sb.journal_blocks
        );
    }
    println!("data:            block {}", sb.data_start);
    println!("created:         {}", format_time(sb.created_ns));
    if sb.mount_count > 0 {
        println!("last mount:      {}", format_time(sb.last_mount_ns));
    }
    println!("mount count:     {}", sb.mount_count);
    Ok(())
}

fn print_entry(attr: &Attr, name: &str) {
    println!(
        "{}{:04o} {:>3} {:>5} {:>5} {:>12} {:>8}  {name}",
        kind_char(attr.kind),
        attr.mode,
        attr.links,
        attr.uid,
        attr.gid,
        attr.size,
        attr.ino,
    );
}

fn ls(image: &Path, path: &str) -> Result<(), String> {
    let mut vol = open_volume(image)?;
    let attr = resolve(&mut vol, path)?;
    if attr.kind != FileKind::Directory {
        print_entry(&attr, path.rsplit('/').next().unwrap_or(path));
        return Ok(());
    }
    let entries = vol
        .readdir(attr.ino, 0)
        .map_err(|e| format!("{path}: {e}"))?;
    for (_, entry) in entries {
        let attr = vol
            .getattr(entry.ino)
            .map_err(|e| format!("{path}/{}: {e}", entry.name))?;
        print_entry(&attr, &entry.name);
    }
    Ok(())
}

/// Copies the content of a regular file to `out`.
fn copy_out(vol: &mut Volume<FileDevice>, path: &str, out: &mut dyn Write) -> Result<(), String> {
    let attr = resolve(vol, path)?;
    if attr.kind != FileKind::RegularFile {
        return Err(format!("{path}: not a regular file"));
    }
    let mut buf = vec![0u8; CHUNK];
    let mut offset = 0u64;
    loop {
        let n = vol
            .read(attr.ino, offset, &mut buf)
            .map_err(|e| format!("{path}: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        offset += n as u64;
    }
}

fn cat(image: &Path, path: &str) -> Result<(), String> {
    let mut vol = open_volume(image)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    copy_out(&mut vol, path, &mut out)?;
    out.flush().map_err(|e| e.to_string())
}

fn get(image: &Path, path: &str, local: &Path) -> Result<(), String> {
    let mut vol = open_volume(image)?;
    let mut file =
        File::create(local).map_err(|e| format!("cannot create {}: {e}", local.display()))?;
    copy_out(&mut vol, path, &mut file)
}

fn put(image: &Path, local: &Path, path: &str) -> Result<(), String> {
    let mut file =
        File::open(local).map_err(|e| format!("cannot open {}: {e}", local.display()))?;
    with_writable(image, |vol| {
        let (parent, name) = resolve_parent(vol, path)?;
        let ino = match vol.lookup(parent, name) {
            Ok(attr) if attr.kind == FileKind::RegularFile => {
                let truncate = SetAttr {
                    size: Some(0),
                    ..SetAttr::default()
                };
                vol.setattr(attr.ino, &truncate)
                    .map_err(|e| format!("{path}: {e}"))?;
                attr.ino
            }
            Ok(_) => return Err(format!("{path}: exists and is not a regular file")),
            Err(DadaError::NotFound) => {
                vol.create(parent, name, 0o644, 0, 0)
                    .map_err(|e| format!("{path}: {e}"))?
                    .ino
            }
            Err(e) => return Err(format!("{path}: {e}")),
        };
        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0u64;
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("cannot read {}: {e}", local.display()))?;
            if n == 0 {
                return Ok(());
            }
            vol.write(ino, offset, &buf[..n])
                .map_err(|e| format!("{path}: {e}"))?;
            offset += n as u64;
        }
    })
}

fn mkdir(image: &Path, path: &str) -> Result<(), String> {
    with_writable(image, |vol| {
        let (parent, name) = resolve_parent(vol, path)?;
        vol.mkdir(parent, name, 0o755, 0, 0)
            .map(drop)
            .map_err(|e| format!("{path}: {e}"))
    })
}

fn rm(image: &Path, path: &str) -> Result<(), String> {
    with_writable(image, |vol| {
        let (parent, name) = resolve_parent(vol, path)?;
        let attr = vol
            .lookup(parent, name)
            .map_err(|e| format!("{path}: {e}"))?;
        let result = if attr.kind == FileKind::Directory {
            vol.rmdir(parent, name)
        } else {
            vol.unlink(parent, name)
        };
        result.map_err(|e| format!("{path}: {e}"))
    })
}

fn stat(image: &Path, path: &str) -> Result<(), String> {
    let mut vol = open_volume(image)?;
    let a = resolve(&mut vol, path)?;
    let kind = match a.kind {
        FileKind::Directory => "directory",
        FileKind::RegularFile => "regular file",
        FileKind::Symlink => "symbolic link",
    };
    println!("path:      {path}");
    println!("type:      {kind}");
    if a.kind == FileKind::Symlink {
        let target = vol.readlink(a.ino).map_err(|e| format!("{path}: {e}"))?;
        println!("target:    {target}");
    }
    println!("inode:     {}", a.ino);
    println!("size:      {}", a.size);
    println!("blocks:    {} (512-byte units)", a.blocks);
    println!("mode:      {:04o}", a.mode);
    println!("owner:     {}:{}", a.uid, a.gid);
    println!("links:     {}", a.links);
    println!("win attrs: {:#x}", a.win_attrs);
    println!("accessed:  {}", format_time(a.atime));
    println!("modified:  {}", format_time(a.mtime));
    println!("changed:   {}", format_time(a.ctime));
    println!("born:      {}", format_time(a.btime));
    Ok(())
}

fn hexdump(data: &[u8]) {
    let mut previous: Option<&[u8]> = None;
    let mut skipping = false;
    for (i, line) in data.chunks(16).enumerate() {
        if previous == Some(line) {
            if !skipping {
                println!("*");
                skipping = true;
            }
            continue;
        }
        skipping = false;
        previous = Some(line);
        let hex: Vec<String> = line.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = line
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("{:08x}  {:<47}  |{ascii}|", i * 16, hex.join(" "));
    }
    println!("{:08x}", data.len());
}

fn dump(image: &Path, what: DumpTarget) -> Result<(), String> {
    match what {
        DumpTarget::Superblock => {
            let mut dev = open_device(image)?;
            let mut buf = vec![0u8; dev.block_size() as usize];
            dev.read_block(0, &mut buf).map_err(|e| e.to_string())?;
            match Superblock::decode(&buf) {
                Ok(sb) => {
                    println!("{sb:#?}");
                    match sb.validate() {
                        Ok(()) => println!("valid"),
                        Err(e) => println!("INVALID: {e}"),
                    }
                }
                Err(e) => {
                    println!("cannot decode superblock: {e}");
                    hexdump(buf.get(..SUPERBLOCK_SIZE).unwrap_or(&buf));
                }
            }
        }
        DumpTarget::Inode { n } => {
            let mut vol = open_volume(image)?;
            let inode = vol.read_inode(n).map_err(|e| format!("inode {n}: {e}"))?;
            println!("{inode:#?}");
        }
        DumpTarget::Block { n } => {
            let mut dev = open_device(image)?;
            let mut buf = vec![0u8; dev.block_size() as usize];
            dev.read_block(n, &mut buf).map_err(|e| match e {
                DadaError::Invalid => format!("block {n} is beyond the end of the image"),
                e => e.to_string(),
            })?;
            hexdump(&buf);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(format_time(0), "1970-01-01T00:00:00.000000000Z");
        assert_eq!(format_time(-1), "1969-12-31T23:59:59.999999999Z");
        assert_eq!(
            format_time(1_709_210_096_000_000_123),
            "2024-02-29T12:34:56.000000123Z"
        );
        // Extreme values must not panic.
        let _ = format_time(i64::MIN);
        let _ = format_time(i64::MAX);
    }

    #[test]
    fn features() {
        let names = [(1, "a"), (2, "b")];
        assert_eq!(feature_names(0, &names), "-");
        assert_eq!(feature_names(3, &names), "a b");
        assert_eq!(feature_names(5, &names), "a unknown(0x4)");
    }
}
