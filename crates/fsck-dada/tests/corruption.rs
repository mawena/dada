//! Milestone 5 criterion: images corrupted on purpose are detected and
//! repaired; after `--repair`, a second check is clean and the volume mounts.

use fsck_dada::{check, Options, Report, EXIT_CLEAN, EXIT_FIXED, EXIT_UNFIXED};
use libdada::dir::DirBlock;
use libdada::format::{INODE_SIZE, INO_ROOT};
use libdada::inode::Inode;
use libdada::superblock::Superblock;
use libdada::{format, FileKind, FormatOptions, Ino, MemDevice, Volume};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const BS: usize = 1024;

fn seed() -> u64 {
    let seed = std::env::var("DADA_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(rand::random);
    eprintln!("DADA_SEED={seed}");
    seed
}

fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
        .collect()
}

/// A volume with directories, files of various sizes, symlinks and hard links.
fn populated() -> MemDevice {
    let mut dev = MemDevice::new(BS as u32, 8192).unwrap();
    let opts = FormatOptions {
        block_size: BS as u32,
        ..FormatOptions::default()
    };
    format(&mut dev, &opts).unwrap();
    let mut vol = Volume::open(dev, false).unwrap();
    let docs = vol.mkdir(INO_ROOT, "docs", 0o755, 0, 0).unwrap().ino;
    let deep = vol.mkdir(docs, "deep", 0o755, 0, 0).unwrap().ino;
    for i in 0..40u8 {
        let parent = [INO_ROOT, docs, deep][i as usize % 3];
        let f = vol
            .create(parent, &format!("file-{i}"), 0o644, 0, 0)
            .unwrap()
            .ino;
        vol.write(f, 0, &pattern(i, (i as usize) * 997)).unwrap();
    }
    vol.symlink(docs, "link", "../file-0", 0, 0).unwrap();
    let f = vol.lookup(deep, "file-2").unwrap().ino;
    vol.link(f, INO_ROOT, "hard").unwrap();
    vol.close().unwrap()
}

fn run(dev: MemDevice, repair: bool) -> (Report, MemDevice) {
    check(dev, Options { repair }).unwrap()
}

/// Mounts the volume and reads everything; returns the number of entries.
fn walk(dev: MemDevice) -> (usize, MemDevice) {
    let mut vol = Volume::open(dev, true).unwrap();
    let mut stack = vec![INO_ROOT];
    let mut count = 0;
    while let Some(dir) = stack.pop() {
        for (_, e) in vol.readdir(dir, 0).unwrap() {
            if e.name == "." || e.name == ".." {
                continue;
            }
            count += 1;
            let attr = vol.getattr(e.ino).unwrap();
            assert_eq!(attr.kind, e.kind);
            match e.kind {
                FileKind::Directory => stack.push(e.ino),
                FileKind::Symlink => {
                    vol.readlink(e.ino).unwrap();
                }
                FileKind::RegularFile => {
                    let mut buf = vec![0u8; attr.size as usize];
                    assert_eq!(vol.read(e.ino, 0, &mut buf).unwrap(), buf.len());
                }
            }
        }
    }
    (count, vol.close().unwrap())
}

/// Repairs, then checks that a second pass is clean and that the volume mounts.
fn repair_and_verify(dev: MemDevice) -> (Report, MemDevice) {
    let (report, dev) = run(dev, true);
    let (second, dev) = run(dev, false);
    assert_eq!(
        second.exit_code(),
        EXIT_CLEAN,
        "after repair ({:?}), still: {:?}",
        report.problems,
        second.problems
    );
    let (_, dev) = walk(dev);
    (report, dev)
}

fn superblock(dev: &MemDevice) -> Superblock {
    Superblock::decode(dev.as_bytes()).unwrap()
}

fn mutate(dev: MemDevice, f: impl FnOnce(&mut Vec<u8>, &Superblock)) -> MemDevice {
    let sb = superblock(&dev);
    let mut bytes = dev.into_bytes();
    f(&mut bytes, &sb);
    MemDevice::from_bytes(BS as u32, bytes).unwrap()
}

fn inode_offset(sb: &Superblock, ino: Ino) -> usize {
    sb.inode_table_start as usize * BS + ino as usize * INODE_SIZE as usize
}

#[test]
fn clean_image_is_clean() {
    let (report, dev) = run(populated(), false);
    assert_eq!(report.exit_code(), EXIT_CLEAN, "{:?}", report.problems);
    // 2 directories, 40 files, 1 symlink, 1 hard link.
    let (count, _) = walk(dev);
    assert_eq!(count, 44);
}

#[test]
fn falsified_bitmaps() {
    let dev = mutate(populated(), |b, sb| {
        let blocks = sb.block_bitmap_start as usize * BS;
        b[blocks + 300] ^= 0b1010_0101; // data blocks marked free or used
        let inodes = sb.inode_bitmap_start as usize * BS;
        b[inodes + 2] ^= 0xFF; // user inodes 16..24
        b[inodes] &= !0b10; // root marked free
    });
    let (report, dev) = run(dev, false);
    assert_eq!(report.exit_code(), EXIT_UNFIXED);
    assert!(report.problems.iter().any(|p| p.contains("block bitmap")));
    assert!(report.problems.iter().any(|p| p.contains("inode bitmap")));
    let (report, _) = repair_and_verify(dev);
    assert_eq!(report.exit_code(), EXIT_FIXED);
}

#[test]
fn wrong_counters() {
    let dev = mutate(populated(), |b, sb| {
        let mut sb = sb.clone();
        sb.free_blocks -= 7;
        sb.free_inodes += 3;
        b[..1024].copy_from_slice(&sb.encode());
    });
    let (report, dev) = run(dev, false);
    assert_eq!(report.found, 2, "{:?}", report.problems);
    let (report, dev) = repair_and_verify(dev);
    assert_eq!(report.fixed, 2);
    let sb = superblock(&dev);
    let vol = Volume::open(dev, true).unwrap();
    assert_eq!(vol.statfs().free_blocks, sb.free_blocks);
}

#[test]
fn wrong_link_count() {
    let mut vol = Volume::open(populated(), true).unwrap();
    let f = vol.lookup(INO_ROOT, "hard").unwrap().ino;
    let dev = vol.close().unwrap();
    let dev = mutate(dev, |b, sb| {
        let off = inode_offset(sb, f);
        let mut inode = Inode::decode(f, &b[off..off + 256]).unwrap();
        assert_eq!(inode.links, 2);
        inode.links = 5;
        b[off..off + 256].copy_from_slice(&inode.encode(f));
    });
    let (report, dev) = repair_and_verify(dev);
    assert!(report.problems.iter().any(|p| p.contains("link count")));
    let mut vol = Volume::open(dev, true).unwrap();
    assert_eq!(vol.getattr(f).unwrap().links, 2);
}

/// Removes `name` from the first block of directory `dir` without touching the inode.
fn drop_entry(dev: MemDevice, dir: Ino, name: &str) -> MemDevice {
    let mut vol = Volume::open(dev, true).unwrap();
    let lba = vol.extents(dir).unwrap()[0].physical;
    let dev = vol.close().unwrap();
    mutate(dev, |b, _| {
        let range = lba as usize * BS..(lba as usize + 1) * BS;
        let mut block = DirBlock::parse(&b[range.clone()]).unwrap();
        let offset = block.entries().find(|s| s.name == name).unwrap().offset;
        block.remove(offset).unwrap();
        b[range].copy_from_slice(&block.encode());
    })
}

#[test]
fn orphans_go_to_lost_and_found() {
    let mut vol = Volume::open(populated(), true).unwrap();
    let file = vol.lookup(INO_ROOT, "file-3").unwrap().ino;
    let docs = vol.lookup(INO_ROOT, "docs").unwrap().ino;
    let dev = vol.close().unwrap();
    // A lone file and a whole subtree lose their entries.
    let dev = drop_entry(dev, INO_ROOT, "file-3");
    let dev = drop_entry(dev, INO_ROOT, "docs");

    let (report, dev) = run(dev, false);
    assert_eq!(report.exit_code(), EXIT_UNFIXED);
    let (report, dev) = repair_and_verify(dev);
    assert_eq!(report.exit_code(), EXIT_FIXED);

    let mut vol = Volume::open(dev, true).unwrap();
    let lf = vol.lookup(INO_ROOT, "lost+found").unwrap();
    assert_eq!(lf.kind, FileKind::Directory);
    let f = vol.lookup(lf.ino, &format!("#{file}")).unwrap();
    assert_eq!(f.links, 1);
    let mut buf = vec![0u8; f.size as usize];
    vol.read(f.ino, 0, &mut buf).unwrap();
    assert_eq!(buf, pattern(3, 3 * 997));
    let d = vol.lookup(lf.ino, &format!("#{docs}")).unwrap();
    assert_eq!(vol.lookup(d.ino, "..").unwrap().ino, lf.ino);
    assert!(vol.lookup(d.ino, "deep").is_ok());
    assert_eq!(vol.getattr(lf.ino).unwrap().links, 3);
}

#[test]
fn corrupted_primary_superblock_is_restored() {
    let dev = mutate(populated(), |b, _| b[40] ^= 0xFF);
    assert!(Volume::open(dev.clone(), true).is_err());
    let (report, dev) = repair_and_verify(dev);
    assert!(report.problems[0].contains("primary superblock"));

    // Without a valid backup there is nothing to repair from.
    let dev = mutate(dev, |b, sb| {
        b[40] ^= 0xFF;
        let backup = (sb.total_blocks as usize - 1) * BS;
        b[backup] ^= 0xFF;
    });
    let (report, _) = run(dev, true);
    assert_eq!(report.exit_code(), EXIT_UNFIXED);
}

#[test]
fn corrupted_root_is_rebuilt() {
    let dev = mutate(populated(), |b, sb| {
        let off = inode_offset(sb, INO_ROOT);
        b[off + 3] ^= 0xFF;
    });
    let (report, dev) = repair_and_verify(dev);
    assert!(report.problems.iter().any(|p| p.contains("root")));
    // Everything that hung from the root is now in lost+found.
    let (count, _) = walk(dev);
    // lost+found, plus everything but the root's own entry "hard" (its file
    // is still reachable as docs/deep/file-2).
    assert_eq!(count, 1 + 44 - 1);
}

/// Random bytes in the metadata: whatever happens, one repair must leave a
/// clean volume that mounts and can be read entirely.
#[test]
fn random_metadata_corruption() {
    let seed = seed();
    let mut rng = StdRng::seed_from_u64(seed);
    let pristine = populated();
    let sb = superblock(&pristine);
    // Directory blocks live in the data zone: collect them too.
    let mut vol = Volume::open(pristine.clone(), true).unwrap();
    let mut dir_blocks = Vec::new();
    let mut stack = vec![INO_ROOT];
    while let Some(dir) = stack.pop() {
        dir_blocks.extend(vol.extents(dir).unwrap().iter().map(|e| e.physical));
        for (_, e) in vol.readdir(dir, 0).unwrap() {
            if e.kind == FileKind::Directory && e.name != "." && e.name != ".." {
                stack.push(e.ino);
            }
        }
    }
    drop(vol);

    let iterations = std::env::var("DADA_FSCK_ITERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(150);
    for i in 0..iterations {
        let corruptions = rng.random_range(1..=12);
        let dev = mutate(pristine.clone(), |b, _| {
            for _ in 0..corruptions {
                // Bitmaps, inode table or directory blocks; never block 0
                // nor the backup superblock.
                let block = if rng.random_bool(0.3) {
                    dir_blocks[rng.random_range(0..dir_blocks.len())]
                } else {
                    rng.random_range(sb.block_bitmap_start..sb.data_start)
                };
                let byte = block as usize * BS + rng.random_range(0..BS);
                b[byte] ^= 1 << rng.random_range(0..8);
            }
        });
        let (report, dev) = run(dev, true);
        let (second, dev) = run(dev, false);
        assert_eq!(
            second.exit_code(),
            EXIT_CLEAN,
            "iteration {i}: repair {:?} left {:?}",
            report.problems,
            second.problems
        );
        walk(dev);
    }
}
