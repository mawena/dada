//! Formatting and high-level volume operations.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::bitmap::Bitmap;
use crate::device::BlockDevice;
use crate::dir::DirBlock;
use crate::extent::{map_block, Extent};
use crate::format::{
    INCOMPAT_EXTENT_BLOCKS, INODE_SIZE, INO_JOURNAL, INO_ROOT, MAX_NAME_LEN, RESERVED_INODES,
    STATE_CLEAN, STATE_DIRTY,
};
use crate::inode::{FileKind, Inode, InodeData};
use crate::layout::Layout;
use crate::le::put_bytes;
use crate::superblock::Superblock;
use crate::{DadaError, FormatOptions};

/// Inode number.
pub type Ino = u64;

/// Attributes of a file, directory or symbolic link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub ino: Ino,
    pub kind: FileKind,
    pub size: u64,
    /// Allocated space in 512-byte units (POSIX `st_blocks`).
    pub blocks: u64,
    /// POSIX permission bits (`mode & 0o7777`); the type is in `kind`.
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub links: u32,
    pub win_attrs: u32,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub btime: i64,
}

/// One directory entry returned by `readdir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    pub ino: Ino,
    pub kind: FileKind,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatFs {
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
    pub max_name_len: u32,
}

/// Current time in nanoseconds since the Unix epoch.
fn now_ns() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_nanos()).map_or(i64::MIN, |n| -n),
    }
}

/// A whole block holding `sb` in its first bytes, the rest zero.
fn superblock_block(sb: &Superblock) -> Vec<u8> {
    let mut block = vec![0u8; sb.block_size as usize];
    put_bytes(&mut block, 0, &sb.encode());
    block
}

/// Block and byte offset of inode `ino` in the inode table.
fn inode_location(sb: &Superblock, ino: Ino) -> Result<(u64, usize), DadaError> {
    if ino == 0 || ino >= sb.inode_count {
        return Err(DadaError::Invalid);
    }
    let bs = u64::from(sb.block_size);
    let byte = ino
        .checked_mul(u64::from(INODE_SIZE))
        .ok_or(DadaError::Invalid)?;
    let block = sb
        .inode_table_start
        .checked_add(byte / bs)
        .ok_or(DadaError::Invalid)?;
    Ok((block, (byte % bs) as usize))
}

fn write_inode_to<D: BlockDevice>(
    dev: &mut D,
    sb: &Superblock,
    ino: Ino,
    inode: &Inode,
) -> Result<(), DadaError> {
    let (block, offset) = inode_location(sb, ino)?;
    let mut buf = vec![0u8; sb.block_size as usize];
    dev.read_block(block, &mut buf)?;
    put_bytes(&mut buf, offset, &inode.encode(ino));
    dev.write_block(block, &buf)
}

fn write_zone<D: BlockDevice>(dev: &mut D, start: u64, bytes: &[u8]) -> Result<(), DadaError> {
    let bs = dev.block_size() as usize;
    for (i, chunk) in bytes.chunks(bs).enumerate() {
        dev.write_block(start + i as u64, chunk)?;
    }
    Ok(())
}

/// Formats `dev` as an empty dada volume. The device block size must equal
/// `opts.block_size`.
pub fn format<D: BlockDevice>(dev: &mut D, opts: &FormatOptions) -> Result<(), DadaError> {
    if dev.block_size() != opts.block_size {
        return Err(DadaError::Invalid);
    }
    let bs = u64::from(opts.block_size);
    let volume_bytes = dev
        .block_count()
        .checked_mul(bs)
        .ok_or(DadaError::Invalid)?;
    let layout = Layout::compute(volume_bytes, opts)?;
    let now = now_ns();
    let mut sb = Superblock::from_layout(&layout, opts, *uuid::Uuid::new_v4().as_bytes(), now)?;
    let zero = vec![0u8; opts.block_size as usize];

    // Destroy any previous superblock first, so that an interrupted format
    // is never mistaken for a valid volume.
    dev.write_block(0, &zero)?;
    dev.write_block(layout.backup_superblock(), &zero)?;
    dev.flush()?;

    for i in 0..layout.inode_table_blocks {
        dev.write_block(layout.inode_table_start + i, &zero)?;
    }

    // Root directory: one block, the first of the data zone.
    let root_block = layout.data_start;
    let mut dir = DirBlock::empty(opts.block_size)?;
    dir.insert(INO_ROOT, FileKind::Directory, ".")?;
    dir.insert(INO_ROOT, FileKind::Directory, "..")?;
    dev.write_block(root_block, &dir.encode())?;
    let root = Inode {
        mode: FileKind::Directory.mode_bits() | 0o755,
        links: 2,
        size: bs,
        atime: now,
        mtime: now,
        ctime: now,
        btime: now,
        data: InodeData::Extents(vec![Extent {
            logical: 0,
            physical: root_block,
            length: 1,
        }]),
        ..Inode::default()
    };
    write_inode_to(dev, &sb, INO_ROOT, &root)?;

    if opts.journal {
        let journal = Inode {
            mode: FileKind::RegularFile.mode_bits() | 0o600,
            links: 1,
            size: layout.journal_blocks * bs,
            atime: now,
            mtime: now,
            ctime: now,
            btime: now,
            data: InodeData::Extents(vec![Extent {
                logical: 0,
                physical: layout.journal_start,
                length: layout.journal_blocks,
            }]),
            ..Inode::default()
        };
        write_inode_to(dev, &sb, INO_JOURNAL, &journal)?;
    }

    let bitmap_bytes = |blocks: u64| usize::try_from(blocks * bs).map_err(|_| DadaError::Invalid);
    let mut blocks = Bitmap::new(
        layout.total_blocks,
        bitmap_bytes(layout.block_bitmap_blocks)?,
    )?;
    blocks.set_range(0, root_block + 1, true)?;
    blocks.set(layout.backup_superblock(), true)?;
    write_zone(dev, layout.block_bitmap_start, blocks.as_bytes())?;
    let mut inodes = Bitmap::new(
        layout.inode_count,
        bitmap_bytes(layout.inode_bitmap_blocks)?,
    )?;
    inodes.set_range(0, RESERVED_INODES, true)?;
    write_zone(dev, layout.inode_bitmap_start, inodes.as_bytes())?;

    sb.free_blocks = layout.total_blocks - blocks.count_used();
    sb.free_inodes = layout.inode_count - inodes.count_used();

    // Backup first, primary last: the volume becomes valid only at the end.
    dev.write_block(layout.backup_superblock(), &superblock_block(&sb))?;
    dev.flush()?;
    dev.write_block(0, &superblock_block(&sb))?;
    dev.flush()
}

/// An opened dada volume.
pub struct Volume<D: BlockDevice> {
    dev: D,
    sb: Superblock,
    read_only: bool,
}

fn read_superblock<D: BlockDevice>(dev: &mut D, lba: u64) -> Result<Superblock, DadaError> {
    let mut buf = vec![0u8; dev.block_size() as usize];
    dev.read_block(lba, &mut buf)?;
    let sb = Superblock::decode(&buf)?;
    sb.validate()?;
    Ok(sb)
}

impl<D: BlockDevice> Volume<D> {
    /// Opens a volume. In read-write mode the volume is marked dirty until `close`.
    pub fn open(mut dev: D, read_only: bool) -> Result<Self, DadaError> {
        let sb = match read_superblock(&mut dev, 0) {
            Ok(sb) => sb,
            Err(e @ DadaError::Unsupported(_)) => return Err(e),
            Err(primary) => {
                let backup = dev.block_count().checked_sub(1).filter(|&b| b > 0);
                if backup.is_some_and(|b| read_superblock(&mut dev, b).is_ok()) {
                    return Err(DadaError::Corrupt(format!(
                        "primary superblock is invalid ({primary}) but the backup is valid; \
                         run fsck-dada --repair"
                    )));
                }
                return Err(primary);
            }
        };
        if sb.block_size != dev.block_size() {
            return Err(DadaError::Invalid);
        }
        if sb.total_blocks > dev.block_count() {
            return Err(DadaError::Corrupt(format!(
                "volume has {} blocks but the device only {}",
                sb.total_blocks,
                dev.block_count()
            )));
        }

        let mut vol = Volume { dev, sb, read_only };
        let root = vol.read_inode(INO_ROOT)?;
        if root.is_free() || root.kind()? != FileKind::Directory {
            return Err(DadaError::Corrupt("root inode is not a directory".into()));
        }
        if !read_only {
            vol.sb.state = STATE_DIRTY;
            vol.sb.mount_count = vol.sb.mount_count.wrapping_add(1);
            vol.sb.last_mount_ns = now_ns();
            let block = superblock_block(&vol.sb);
            vol.dev.write_block(0, &block)?;
            vol.dev.flush()?;
        }
        Ok(vol)
    }

    /// Flushes everything, marks the volume clean and returns the device.
    pub fn close(mut self) -> Result<D, DadaError> {
        if !self.read_only {
            self.dev.flush()?;
            self.sb.state = STATE_CLEAN;
            let block = superblock_block(&self.sb);
            self.dev.write_block(self.sb.total_blocks - 1, &block)?;
            self.dev.write_block(0, &block)?;
            self.dev.flush()?;
        }
        Ok(self.dev)
    }

    pub fn sync(&mut self) -> Result<(), DadaError> {
        self.dev.flush()
    }

    pub fn root(&self) -> Ino {
        INO_ROOT
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    pub fn statfs(&self) -> StatFs {
        StatFs {
            block_size: self.sb.block_size,
            total_blocks: self.sb.total_blocks,
            free_blocks: self.sb.free_blocks,
            total_inodes: self.sb.inode_count,
            free_inodes: self.sb.free_inodes,
            max_name_len: MAX_NAME_LEN as u32,
        }
    }

    /// Raw inode `ino`, free or not. Fails with `Invalid` outside `1..inode_count`.
    pub fn read_inode(&mut self, ino: Ino) -> Result<Inode, DadaError> {
        let (block, offset) = inode_location(&self.sb, ino)?;
        let mut buf = vec![0u8; self.sb.block_size as usize];
        self.dev.read_block(block, &mut buf)?;
        Inode::decode(ino, buf.get(offset..).unwrap_or_default())
    }

    /// Raw block `lba` of the device.
    pub fn read_raw_block(&mut self, lba: u64) -> Result<Vec<u8>, DadaError> {
        let mut buf = vec![0u8; self.sb.block_size as usize];
        self.dev.read_block(lba, &mut buf)?;
        Ok(buf)
    }

    /// Allocated inode `ino`; `NotFound` if it is free or out of range.
    fn read_allocated(&mut self, ino: Ino) -> Result<Inode, DadaError> {
        if ino == 0 || ino >= self.sb.inode_count {
            return Err(DadaError::NotFound);
        }
        let inode = self.read_inode(ino)?;
        if inode.is_free() {
            return Err(DadaError::NotFound);
        }
        Ok(inode)
    }

    fn attr(&self, ino: Ino, inode: &Inode) -> Result<Attr, DadaError> {
        let fs_blocks = inode
            .extents()
            .iter()
            .try_fold(0u64, |acc, e| acc.checked_add(e.length))
            .ok_or_else(|| DadaError::Corrupt(format!("inode {ino}: extent lengths overflow")))?;
        Ok(Attr {
            ino,
            kind: inode.kind()?,
            size: inode.size,
            blocks: fs_blocks.saturating_mul(u64::from(self.sb.block_size / 512)),
            mode: inode.permissions() as u16,
            uid: inode.uid,
            gid: inode.gid,
            links: inode.links,
            win_attrs: inode.win_attrs,
            atime: inode.atime,
            mtime: inode.mtime,
            ctime: inode.ctime,
            btime: inode.btime,
        })
    }

    pub fn getattr(&mut self, ino: Ino) -> Result<Attr, DadaError> {
        let inode = self.read_allocated(ino)?;
        self.attr(ino, &inode)
    }

    /// Allocated directory `ino` and its number of blocks.
    fn read_dir_inode(&mut self, ino: Ino) -> Result<(Inode, u64), DadaError> {
        let inode = self.read_allocated(ino)?;
        if inode.kind()? != FileKind::Directory {
            return Err(DadaError::NotDir);
        }
        if inode.extent_block != 0 {
            // Chained extent blocks are implemented at milestone 4.
            return Err(DadaError::Unsupported(INCOMPAT_EXTENT_BLOCKS));
        }
        let bs = u64::from(self.sb.block_size);
        if inode.size == 0 || inode.size % bs != 0 {
            return Err(DadaError::Corrupt(format!(
                "directory {ino}: size {} is not a whole number of blocks",
                inode.size
            )));
        }
        let count = inode.size / bs;
        Ok((inode, count))
    }

    /// Block `index` of directory `ino`.
    fn read_dir_block(
        &mut self,
        ino: Ino,
        inode: &Inode,
        index: u64,
    ) -> Result<DirBlock, DadaError> {
        let physical = map_block(inode.extents(), index)
            .ok_or_else(|| DadaError::Corrupt(format!("directory {ino}: hole at block {index}")))?;
        self.check_data_block(physical)?;
        let buf = self.read_raw_block(physical)?;
        DirBlock::parse(&buf)
            .map_err(|e| DadaError::Corrupt(format!("directory {ino}, block {index}: {e}")))
    }

    fn check_data_block(&self, physical: u64) -> Result<(), DadaError> {
        if physical < self.sb.data_start || physical >= self.sb.total_blocks - 1 {
            return Err(DadaError::Corrupt(format!(
                "block {physical} is outside the data zone"
            )));
        }
        Ok(())
    }

    fn entry_ino(&self, dir: Ino, ino: Ino) -> Result<Ino, DadaError> {
        if ino >= self.sb.inode_count {
            return Err(DadaError::Corrupt(format!(
                "directory {dir}: entry points to inode {ino}, beyond inode_count"
            )));
        }
        Ok(ino)
    }

    /// Entries of directory `ino`, `.` and `..` included, after cookie `offset`
    /// (0 to start). Each entry comes with the cookie to resume after it.
    pub fn readdir(
        &mut self,
        ino: Ino,
        offset: u64,
    ) -> Result<Vec<(u64, DirEntryInfo)>, DadaError> {
        let (inode, count) = self.read_dir_inode(ino)?;
        let bs = u64::from(self.sb.block_size);
        let mut out = Vec::new();
        for index in offset / bs..count {
            let block = self.read_dir_block(ino, &inode, index)?;
            for slot in block.entries() {
                // Cookie = position of the entry in the directory + 1.
                let cookie = index * bs + slot.offset as u64 + 1;
                if cookie <= offset {
                    continue;
                }
                let kind = slot.kind.ok_or(DadaError::Invalid)?;
                let entry = DirEntryInfo {
                    ino: self.entry_ino(ino, slot.ino)?,
                    kind,
                    name: slot.name.clone(),
                };
                out.push((cookie, entry));
            }
        }
        Ok(out)
    }

    /// Looks up `name` in directory `parent`.
    pub fn lookup(&mut self, parent: Ino, name: &str) -> Result<Attr, DadaError> {
        if name.len() > MAX_NAME_LEN {
            return Err(DadaError::NameTooLong);
        }
        if name.is_empty() || name.contains(['/', '\0']) {
            return Err(DadaError::InvalidName);
        }
        let (inode, count) = self.read_dir_inode(parent)?;
        for index in 0..count {
            let block = self.read_dir_block(parent, &inode, index)?;
            let found = block.entries().find(|s| s.name == name).map(|s| s.ino);
            if let Some(ino) = found {
                let ino = self.entry_ino(parent, ino)?;
                return self.getattr(ino).map_err(|e| match e {
                    DadaError::NotFound => DadaError::Corrupt(format!(
                        "directory {parent}: entry {name:?} points to free inode {ino}"
                    )),
                    e => e,
                });
            }
        }
        Err(DadaError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{INCOMPAT_CASEFOLD, INCOMPAT_JOURNAL};
    use crate::MemDevice;

    const MIB: u64 = 1024 * 1024;

    fn formatted(bs: u32, bytes: u64, journal: bool) -> MemDevice {
        let mut dev = MemDevice::new(bs, bytes / u64::from(bs)).unwrap();
        let opts = FormatOptions {
            block_size: bs,
            journal,
            label: "TEST".into(),
            ..FormatOptions::default()
        };
        format(&mut dev, &opts).unwrap();
        dev
    }

    fn names(entries: &[(u64, DirEntryInfo)]) -> Vec<&str> {
        entries.iter().map(|(_, e)| e.name.as_str()).collect()
    }

    #[test]
    fn format_then_open() {
        for (bs, journal) in [(1024, true), (4096, true), (4096, false), (65536, false)] {
            let dev = formatted(bs, 8 * MIB, journal);
            let mut vol = Volume::open(dev, true).unwrap();
            let sb = vol.superblock().clone();
            assert_eq!(sb.label().unwrap(), "TEST");
            assert_eq!(sb.state, STATE_CLEAN);
            assert_eq!(sb.features_incompat & INCOMPAT_JOURNAL != 0, journal);
            assert_ne!(sb.uuid, [0; 16]);

            let st = vol.statfs();
            assert_eq!(st.free_blocks, sb.total_blocks - sb.data_start - 2);
            assert_eq!(st.free_inodes, sb.inode_count - 16);
            assert_eq!(st.max_name_len, 255);

            let root = vol.getattr(vol.root()).unwrap();
            assert_eq!(root.kind, FileKind::Directory);
            assert_eq!(
                (root.links, root.mode, root.size),
                (2, 0o755, u64::from(bs))
            );
            assert_eq!(root.blocks, u64::from(bs) / 512);

            let entries = vol.readdir(INO_ROOT, 0).unwrap();
            assert_eq!(names(&entries), [".", ".."]);
            assert!(entries.iter().all(|(_, e)| e.ino == INO_ROOT));

            let journal_attr = vol.getattr(INO_JOURNAL);
            if journal {
                let j = journal_attr.unwrap();
                assert_eq!(j.kind, FileKind::RegularFile);
                assert_eq!(j.size, sb.journal_blocks * u64::from(bs));
            } else {
                assert!(matches!(journal_attr, Err(DadaError::NotFound)));
            }
        }
    }

    #[test]
    fn on_disk_bitmaps_match_counters() {
        let mut dev = formatted(1024, 4 * MIB, true);
        let mut vol = Volume::open(dev.clone(), true).unwrap();
        let sb = vol.superblock().clone();
        let bs = 1024usize;
        let zone = |vol: &mut Volume<MemDevice>, start: u64, n: u64| {
            (0..n)
                .flat_map(|i| vol.read_raw_block(start + i).unwrap())
                .collect::<Vec<u8>>()
        };
        let blocks = Bitmap::from_bytes(
            zone(&mut vol, sb.block_bitmap_start, sb.inode_bitmap_start - 1),
            sb.total_blocks,
        )
        .unwrap();
        assert_eq!(blocks.count_used(), sb.total_blocks - sb.free_blocks);
        assert!(blocks.padding_is_set());
        for b in 0..=sb.data_start {
            assert!(blocks.get(b).unwrap(), "block {b}");
        }
        assert!(!blocks.get(sb.data_start + 1).unwrap());
        assert!(blocks.get(sb.total_blocks - 1).unwrap());

        let inodes = Bitmap::from_bytes(
            zone(
                &mut vol,
                sb.inode_bitmap_start,
                sb.inode_table_start - sb.inode_bitmap_start,
            ),
            sb.inode_count,
        )
        .unwrap();
        assert_eq!(inodes.count_used(), 16);
        assert!(inodes.padding_is_set());

        // Rest of block 0 and of the backup block is zero.
        dev.read_block(0, &mut vec![0; bs]).unwrap();
        let raw = dev.as_bytes();
        assert!(raw[1024..bs].iter().all(|&b| b == 0));
        let backup = (sb.total_blocks as usize - 1) * bs;
        assert_eq!(&raw[backup..backup + 1024], &raw[..1024]);
    }

    #[test]
    fn readdir_cookies_resume() {
        let mut vol = Volume::open(formatted(4096, 8 * MIB, true), true).unwrap();
        let all = vol.readdir(INO_ROOT, 0).unwrap();
        assert_eq!(all[0].0, 1);
        assert_eq!(all[1].0, 17);
        assert_eq!(names(&vol.readdir(INO_ROOT, 1).unwrap()), [".."]);
        assert!(vol.readdir(INO_ROOT, 17).unwrap().is_empty());
        assert!(vol.readdir(INO_ROOT, u64::MAX).unwrap().is_empty());
    }

    #[test]
    fn lookup_and_errors() {
        let mut vol = Volume::open(formatted(4096, 8 * MIB, true), true).unwrap();
        assert_eq!(vol.lookup(INO_ROOT, ".").unwrap().ino, INO_ROOT);
        assert_eq!(vol.lookup(INO_ROOT, "..").unwrap().ino, INO_ROOT);
        assert!(matches!(
            vol.lookup(INO_ROOT, "absent"),
            Err(DadaError::NotFound)
        ));
        assert!(matches!(
            vol.lookup(INO_ROOT, "a/b"),
            Err(DadaError::InvalidName)
        ));
        assert!(matches!(
            vol.lookup(INO_ROOT, ""),
            Err(DadaError::InvalidName)
        ));
        assert!(matches!(
            vol.lookup(INO_ROOT, &"x".repeat(256)),
            Err(DadaError::NameTooLong)
        ));
        assert!(matches!(
            vol.readdir(INO_JOURNAL, 0),
            Err(DadaError::NotDir)
        ));
        assert!(matches!(
            vol.lookup(INO_JOURNAL, "x"),
            Err(DadaError::NotDir)
        ));
        for ino in [0, 3, 16, u64::MAX] {
            assert!(
                matches!(vol.getattr(ino), Err(DadaError::NotFound)),
                "{ino}"
            );
            assert!(
                matches!(vol.readdir(ino, 0), Err(DadaError::NotFound)),
                "{ino}"
            );
        }
    }

    #[test]
    fn open_marks_dirty_and_close_marks_clean() {
        let vol = Volume::open(formatted(4096, 8 * MIB, true), false).unwrap();
        assert_eq!(vol.superblock().state, STATE_DIRTY);
        let dev = vol.close().unwrap();
        let sb = Superblock::decode(dev.as_bytes()).unwrap();
        assert_eq!((sb.state, sb.mount_count), (STATE_CLEAN, 1));
        assert!(sb.last_mount_ns > 0);
        let backup = dev.as_bytes().len() - 4096;
        assert_eq!(
            &dev.as_bytes()[backup..backup + 1024],
            &dev.as_bytes()[..1024]
        );

        let dev = Volume::open(dev, false).unwrap().close().unwrap();
        let sb = Superblock::decode(dev.as_bytes()).unwrap();
        assert_eq!((sb.state, sb.mount_count), (STATE_CLEAN, 2));
    }

    #[test]
    fn read_only_open_writes_nothing() {
        let dev = formatted(4096, 8 * MIB, true);
        let before = dev.as_bytes().to_vec();
        let mut vol = Volume::open(dev, true).unwrap();
        vol.readdir(INO_ROOT, 0).unwrap();
        vol.sync().unwrap();
        let dev = vol.close().unwrap();
        assert_eq!(dev.as_bytes(), &before[..]);
    }

    #[test]
    fn corrupt_primary_suggests_fsck() {
        let mut bytes = formatted(4096, 8 * MIB, true).into_bytes();
        bytes[100] ^= 0xFF;
        let err = Volume::open(MemDevice::from_bytes(4096, bytes.clone()).unwrap(), true)
            .err()
            .unwrap();
        assert!(err.to_string().contains("fsck-dada"), "{err}");

        // Both superblocks broken: the primary error is reported.
        let backup = bytes.len() - 4096;
        bytes[backup] = 0;
        let err = Volume::open(MemDevice::from_bytes(4096, bytes).unwrap(), true)
            .err()
            .unwrap();
        assert!(err.to_string().contains("checksum"), "{err}");
    }

    #[test]
    fn open_rejects_mismatches() {
        let bytes = formatted(4096, 8 * MIB, true).into_bytes();
        // Device block size differs from the volume.
        let dev = MemDevice::from_bytes(1024, bytes.clone()).unwrap();
        assert!(Volume::open(dev, true).is_err());
        // Device smaller than the volume.
        let dev = MemDevice::from_bytes(4096, bytes[..bytes.len() - 4096].to_vec()).unwrap();
        assert!(Volume::open(dev, true).is_err());
        // Empty device.
        assert!(Volume::open(MemDevice::new(4096, 0).unwrap(), true).is_err());
    }

    #[test]
    fn unknown_incompat_feature_is_refused() {
        let dev = formatted(4096, 8 * MIB, true);
        let mut sb = Superblock::decode(dev.as_bytes()).unwrap();
        sb.features_incompat |= 1 << 10;
        let mut bytes = dev.into_bytes();
        bytes[..1024].copy_from_slice(&sb.encode());
        let err = Volume::open(MemDevice::from_bytes(4096, bytes).unwrap(), true)
            .err()
            .unwrap();
        assert!(matches!(err, DadaError::Unsupported(0x400)));
    }

    #[test]
    fn corrupt_root_is_detected() {
        let dev = formatted(4096, 8 * MIB, true);
        let sb = Superblock::decode(dev.as_bytes()).unwrap();
        // Damage the root directory block.
        let mut bytes = dev.clone().into_bytes();
        bytes[sb.data_start as usize * 4096 + 20] ^= 1;
        let mut vol = Volume::open(MemDevice::from_bytes(4096, bytes).unwrap(), true).unwrap();
        assert!(matches!(
            vol.readdir(INO_ROOT, 0),
            Err(DadaError::Corrupt(_))
        ));
        // Damage the root inode.
        let mut bytes = dev.into_bytes();
        bytes[sb.inode_table_start as usize * 4096 + 256 + 4] ^= 1;
        let err = Volume::open(MemDevice::from_bytes(4096, bytes).unwrap(), true)
            .err()
            .unwrap();
        assert!(matches!(err, DadaError::Corrupt(_)));
    }

    #[test]
    fn format_over_garbage() {
        let mut dev = MemDevice::from_bytes(4096, vec![0xAB; 8 * MIB as usize]).unwrap();
        format(&mut dev, &FormatOptions::default()).unwrap();
        let mut vol = Volume::open(dev, true).unwrap();
        assert_eq!(names(&vol.readdir(INO_ROOT, 0).unwrap()), [".", ".."]);
        for ino in 3..64 {
            assert!(vol.read_inode(ino).unwrap().is_free());
        }
    }

    #[test]
    fn format_rejects_bad_requests() {
        let mut dev = MemDevice::new(4096, 2048).unwrap();
        let opts = FormatOptions {
            block_size: 1024,
            ..FormatOptions::default()
        };
        assert!(matches!(format(&mut dev, &opts), Err(DadaError::Invalid)));
        let mut tiny = MemDevice::new(4096, 64).unwrap();
        assert!(matches!(
            format(&mut tiny, &FormatOptions::default()),
            Err(DadaError::NoSpace)
        ));
        let opts = FormatOptions {
            casefold: true,
            ..FormatOptions::default()
        };
        format(&mut dev, &opts).unwrap();
        let vol = Volume::open(dev, true).unwrap();
        assert_ne!(vol.superblock().features_incompat & INCOMPAT_CASEFOLD, 0);
    }
}
