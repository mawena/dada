#![no_main]

//! An arbitrary image is opened, then everything reachable is read.
//! Checksums of the superblock and of the inodes are recomputed first, so
//! that the fuzzer explores what lies behind them.

use libdada::format::{ino, sb, INODE_SIZE, SUPERBLOCK_SIZE};
use libdada::{crc, FileKind, MemDevice, Volume};
use libfuzzer_sys::fuzz_target;

const BS: usize = 1024;
/// Upper bound on the work done for one input.
const BUDGET: usize = 2000;

fn reseal(image: &mut [u8]) {
    let Some(head) = image.get_mut(..SUPERBLOCK_SIZE) else {
        return;
    };
    let crc = crc::crc32c(&head[..sb::CHECKSUM]);
    head[sb::CHECKSUM..].copy_from_slice(&crc.to_le_bytes());
    let Ok(sblock) = libdada::superblock::Superblock::decode(image) else {
        return;
    };
    let start = sblock.inode_table_start as usize * BS;
    let slots = (sblock.inode_count as usize).min(4096);
    for n in 1..slots {
        let at = start.saturating_add(n * INODE_SIZE as usize);
        let Some(slot) = image.get_mut(at..at + INODE_SIZE as usize) else {
            break;
        };
        if slot.iter().all(|&b| b == 0) {
            continue;
        }
        let crc = crc::crc32c_append(crc::crc32c(&(n as u64).to_le_bytes()), &slot[..ino::CHECKSUM]);
        slot[ino::CHECKSUM..].copy_from_slice(&crc.to_le_bytes());
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 * BS || data.len() > 4 << 20 {
        return;
    }
    let mut image = data[..data.len() / BS * BS].to_vec();
    reseal(&mut image);
    let Ok(dev) = MemDevice::from_bytes(BS as u32, image) else {
        return;
    };
    let Ok(mut vol) = Volume::open(dev, true) else {
        return;
    };
    let _ = vol.statfs();
    let mut budget = BUDGET;
    let mut stack = vec![vol.root()];
    let mut buf = vec![0u8; 64 * 1024];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = vol.readdir(dir, 0) else {
            continue;
        };
        for (_, entry) in entries {
            budget = match budget.checked_sub(1) {
                Some(b) => b,
                None => return,
            };
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let _ = vol.lookup(dir, &entry.name);
            let _ = vol.extents(entry.ino);
            match vol.getattr(entry.ino) {
                Ok(attr) if attr.kind == FileKind::Directory => stack.push(entry.ino),
                Ok(attr) if attr.kind == FileKind::Symlink => {
                    let _ = vol.readlink(entry.ino);
                }
                Ok(_) => {
                    let _ = vol.read(entry.ino, 0, &mut buf);
                    let _ = vol.read(entry.ino, 1 << 40, &mut buf);
                }
                Err(_) => {}
            }
        }
    }
});
