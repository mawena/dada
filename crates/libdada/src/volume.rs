//! Formatting and high-level volume operations.

use std::collections::{BTreeSet, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bitmap::Bitmap;
use crate::cache::{BlockCache, DEFAULT_CACHE_BLOCKS};
use crate::device::BlockDevice;
use crate::dir::DirBlock;
use crate::extent::{
    holes, insert_extent, map_block, mapped_blocks, truncate_extents, validate_extents, Extent,
    ExtentBlock,
};
use crate::format::{
    extent_block_capacity, COMPAT_WIN_ATTRS, FIRST_USER_INO, INCOMPAT_CASEFOLD,
    INCOMPAT_EXTENT_BLOCKS, INLINE_DATA_MAX, INODE_INLINE_EXTENTS, INODE_SIZE, INO_JOURNAL,
    INO_ROOT, MAX_NAME_LEN, MODE_PERM_MASK, RESERVED_INODES, STATE_CLEAN, STATE_DIRTY,
};
use crate::inode::{FileKind, Inode, InodeData};
use crate::layout::Layout;
use crate::le::put_bytes;
use crate::name::{normalize, normalize_new, NameMatcher};
use crate::superblock::Superblock;
use crate::{DadaError, FormatOptions};

/// Inode number.
pub type Ino = u64;

/// Maximum length of a symbolic link target, in bytes.
pub const MAX_SYMLINK_LEN: usize = 4095;

/// Attributes of a file, directory or symbolic link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub ino: Ino,
    pub kind: FileKind,
    pub size: u64,
    /// Allocated space in 512-byte units (POSIX `st_blocks`), extent blocks included.
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

/// Attribute changes for `setattr`; `None` leaves a field unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetAttr {
    pub mode: Option<u16>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<i64>,
    pub mtime: Option<i64>,
    pub win_attrs: Option<u32>,
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

/// Number of blocks of a bitmap of `bits` bits.
fn bitmap_blocks(bits: u64, block_size: u32) -> u64 {
    bits.div_ceil(u64::from(block_size) * 8)
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

fn read_superblock<D: BlockDevice>(dev: &mut D, lba: u64) -> Result<Superblock, DadaError> {
    let mut buf = vec![0u8; dev.block_size() as usize];
    dev.read_block(lba, &mut buf)?;
    let sb = Superblock::decode(&buf)?;
    sb.validate()?;
    Ok(sb)
}

fn load_bitmap<D: BlockDevice>(dev: &mut D, start: u64, bits: u64) -> Result<Bitmap, DadaError> {
    let bs = dev.block_size() as usize;
    let blocks = bitmap_blocks(bits, dev.block_size());
    let mut bytes = Vec::new();
    let mut buf = vec![0u8; bs];
    for i in 0..blocks {
        dev.read_block(start + i, &mut buf)?;
        bytes.extend_from_slice(&buf);
    }
    Bitmap::from_bytes(bytes, bits)
}

/// A directory being read or modified: its inode and full extent list.
struct Dir {
    ino: Ino,
    inode: Inode,
    extents: Vec<Extent>,
    chain: Vec<u64>,
    blocks: u64,
}

/// Position of an entry found in a directory.
struct Found {
    index: u64,
    offset: usize,
    ino: Ino,
    kind: FileKind,
}

/// An opened dada volume.
pub struct Volume<D: BlockDevice> {
    dev: D,
    sb: Superblock,
    read_only: bool,
    cache: BlockCache,
    block_bitmap: Bitmap,
    inode_bitmap: Bitmap,
    /// Bitmap blocks (relative to their zone) changed since the last sync.
    dirty_block_bitmap: BTreeSet<u64>,
    dirty_inode_bitmap: BTreeSet<u64>,
    /// Where the next allocation without a better goal starts.
    block_hint: u64,
    inode_hint: u64,
}

impl<D: BlockDevice> Volume<D> {
    /// Opens a volume with the default cache size. In read-write mode the
    /// volume is marked dirty until `close`.
    pub fn open(dev: D, read_only: bool) -> Result<Self, DadaError> {
        Self::open_with_cache(dev, read_only, DEFAULT_CACHE_BLOCKS)
    }

    /// Opens a volume with a metadata cache of `cache_blocks` blocks.
    pub fn open_with_cache(
        mut dev: D,
        read_only: bool,
        cache_blocks: usize,
    ) -> Result<Self, DadaError> {
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
        let block_bitmap = load_bitmap(&mut dev, sb.block_bitmap_start, sb.total_blocks)?;
        let inode_bitmap = load_bitmap(&mut dev, sb.inode_bitmap_start, sb.inode_count)?;

        let mut vol = Volume {
            block_hint: sb.data_start,
            inode_hint: FIRST_USER_INO,
            dev,
            sb,
            read_only,
            cache: BlockCache::new(cache_blocks),
            block_bitmap,
            inode_bitmap,
            dirty_block_bitmap: BTreeSet::new(),
            dirty_inode_bitmap: BTreeSet::new(),
        };
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

    /// Writes all pending metadata, marks the volume clean and returns the device.
    pub fn close(mut self) -> Result<D, DadaError> {
        if !self.read_only {
            self.write_back()?;
            self.sb.state = STATE_CLEAN;
            let block = superblock_block(&self.sb);
            self.dev.write_block(self.sb.total_blocks - 1, &block)?;
            self.dev.write_block(0, &block)?;
            self.dev.flush()?;
        }
        Ok(self.dev)
    }

    /// Writes all pending metadata and flushes the device. The volume stays
    /// marked dirty until `close`.
    pub fn sync(&mut self) -> Result<(), DadaError> {
        if self.read_only {
            return Ok(());
        }
        self.write_back()?;
        let block = superblock_block(&self.sb);
        self.dev.write_block(0, &block)?;
        self.dev.flush()
    }

    /// Bitmaps and cached blocks to the device, then a device flush.
    fn write_back(&mut self) -> Result<(), DadaError> {
        let bs = self.sb.block_size as usize;
        for (dirty, start, which) in [
            (
                std::mem::take(&mut self.dirty_block_bitmap),
                self.sb.block_bitmap_start,
                true,
            ),
            (
                std::mem::take(&mut self.dirty_inode_bitmap),
                self.sb.inode_bitmap_start,
                false,
            ),
        ] {
            for b in dirty {
                let bitmap = if which {
                    &self.block_bitmap
                } else {
                    &self.inode_bitmap
                };
                let from = usize::try_from(b).map_err(|_| DadaError::Invalid)? * bs;
                let bytes = bitmap
                    .as_bytes()
                    .get(from..from + bs)
                    .ok_or(DadaError::Invalid)?
                    .to_vec();
                self.cache.write(&mut self.dev, start + b, bytes)?;
            }
        }
        self.cache.write_back(&mut self.dev)?;
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

    fn casefold(&self) -> bool {
        self.sb.features_incompat & INCOMPAT_CASEFOLD != 0
    }

    fn bs(&self) -> u64 {
        u64::from(self.sb.block_size)
    }

    fn writable(&self) -> Result<(), DadaError> {
        if self.read_only {
            Err(DadaError::ReadOnly)
        } else {
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Block and inode access
    // -----------------------------------------------------------------------

    fn meta_read(&mut self, lba: u64) -> Result<Vec<u8>, DadaError> {
        self.cache.read(&mut self.dev, lba)
    }

    fn meta_write(&mut self, lba: u64, data: Vec<u8>) -> Result<(), DadaError> {
        self.cache.write(&mut self.dev, lba, data)
    }

    /// Block `lba` as the volume currently sees it (cached changes included).
    pub fn read_raw_block(&mut self, lba: u64) -> Result<Vec<u8>, DadaError> {
        self.meta_read(lba)
    }

    /// Raw inode `ino`, free or not. Fails with `Invalid` outside `1..inode_count`.
    pub fn read_inode(&mut self, ino: Ino) -> Result<Inode, DadaError> {
        let (block, offset) = inode_location(&self.sb, ino)?;
        let buf = self.meta_read(block)?;
        Inode::decode(ino, buf.get(offset..).unwrap_or_default())
    }

    fn write_inode(&mut self, ino: Ino, inode: &Inode) -> Result<(), DadaError> {
        let (block, offset) = inode_location(&self.sb, ino)?;
        let mut buf = self.meta_read(block)?;
        put_bytes(&mut buf, offset, &inode.encode(ino));
        self.meta_write(block, buf)
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

    /// Allocated inode referenced by a directory entry: a free one is corruption.
    fn read_referenced(&mut self, dir: Ino, ino: Ino) -> Result<Inode, DadaError> {
        self.read_allocated(ino).map_err(|e| match e {
            DadaError::NotFound => {
                DadaError::Corrupt(format!("directory {dir}: entry points to free inode {ino}"))
            }
            e => e,
        })
    }

    // -----------------------------------------------------------------------
    // Allocation
    // -----------------------------------------------------------------------

    fn mark_blocks(&mut self, start: u64, len: u64, used: bool) -> Result<(), DadaError> {
        if len == 0 {
            return Ok(());
        }
        self.block_bitmap.set_range(start, len, used)?;
        let bits = self.bs() * 8;
        for b in start / bits..=(start + len - 1) / bits {
            self.dirty_block_bitmap.insert(b);
        }
        Ok(())
    }

    /// Allocates a run of 1 to `want` blocks, first fit from `goal`.
    fn alloc_blocks(&mut self, goal: u64, want: u64) -> Result<(u64, u64), DadaError> {
        let (lo, hi) = (self.sb.data_start, self.sb.total_blocks - 1);
        let (start, len) = self
            .block_bitmap
            .find_run(goal, lo, hi, want)
            .ok_or(DadaError::NoSpace)?;
        self.mark_blocks(start, len, true)?;
        self.sb.free_blocks = self.sb.free_blocks.saturating_sub(len);
        self.block_hint = start + len;
        Ok((start, len))
    }

    fn release_blocks(&mut self, start: u64, len: u64) -> Result<(), DadaError> {
        self.check_data_range(start, len)?;
        for b in start..start + len {
            if !self.block_bitmap.get(b)? {
                return Err(DadaError::Corrupt(format!("block {b} freed twice")));
            }
            self.cache.discard(b);
        }
        self.mark_blocks(start, len, false)?;
        self.sb.free_blocks = (self.sb.free_blocks + len).min(self.sb.total_blocks);
        Ok(())
    }

    /// Releases runs allocated by an operation that failed.
    fn roll_back(&mut self, runs: &[(u64, u64)]) {
        for &(start, len) in runs {
            let _ = self.release_blocks(start, len);
        }
    }

    /// Allocates an inode; returns its number and new generation.
    fn alloc_inode(&mut self) -> Result<(Ino, u32), DadaError> {
        let ino = self
            .inode_bitmap
            .find_clear(self.inode_hint)
            .or_else(|| self.inode_bitmap.find_clear(FIRST_USER_INO))
            .filter(|&i| i >= FIRST_USER_INO)
            .ok_or(DadaError::NoInodes)?;
        let generation = self
            .read_inode(ino)
            .map_or(1, |old| old.generation.wrapping_add(1));
        self.inode_bitmap.set(ino, true)?;
        self.dirty_inode_bitmap.insert(ino / (self.bs() * 8));
        self.sb.free_inodes = self.sb.free_inodes.saturating_sub(1);
        self.inode_hint = ino + 1;
        Ok((ino, generation))
    }

    /// Frees inode `ino` and everything it owns.
    fn release_inode(&mut self, ino: Ino, mut inode: Inode) -> Result<(), DadaError> {
        self.free_content(ino, &mut inode)?;
        let freed = Inode {
            generation: inode.generation,
            ..Inode::default()
        };
        self.write_inode(ino, &freed)?;
        self.inode_bitmap.set(ino, false)?;
        self.dirty_inode_bitmap.insert(ino / (self.bs() * 8));
        self.sb.free_inodes = (self.sb.free_inodes + 1).min(self.sb.inode_count);
        self.inode_hint = self.inode_hint.min(ino);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Extent lists
    // -----------------------------------------------------------------------

    fn check_data_range(&self, start: u64, len: u64) -> Result<(), DadaError> {
        let end = start.checked_add(len);
        if start < self.sb.data_start || end.is_none_or(|e| e > self.sb.total_blocks - 1) {
            return Err(DadaError::Corrupt(format!(
                "blocks {start}+{len} are outside the data zone"
            )));
        }
        Ok(())
    }

    fn check_extent(&self, ino: Ino, e: &Extent) -> Result<(), DadaError> {
        if ino == INO_JOURNAL {
            let journal = self.sb.journal_start..self.sb.journal_start + self.sb.journal_blocks;
            let end = e.physical.saturating_add(e.length);
            if e.physical < journal.start || end > journal.end {
                return Err(DadaError::Corrupt(
                    "journal inode outside the journal".into(),
                ));
            }
            return Ok(());
        }
        self.check_data_range(e.physical, e.length)
            .map_err(|err| DadaError::Corrupt(format!("inode {ino}: {err}")))
    }

    /// Full extent list of `inode` (inline then chained) and its extent blocks.
    fn load_extents(
        &mut self,
        ino: Ino,
        inode: &Inode,
    ) -> Result<(Vec<Extent>, Vec<u64>), DadaError> {
        let mut list = inode.extents().to_vec();
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut next = inode.extent_block;
        while next != 0 {
            self.check_data_range(next, 1)?;
            if !seen.insert(next) {
                return Err(DadaError::Corrupt(format!(
                    "inode {ino}: extent block loop"
                )));
            }
            let block = ExtentBlock::decode(&self.meta_read(next)?)
                .map_err(|e| DadaError::Corrupt(format!("inode {ino}: {e}")))?;
            if block.owner != ino {
                return Err(DadaError::Corrupt(format!(
                    "inode {ino}: extent block {next} belongs to inode {}",
                    block.owner
                )));
            }
            list.extend(block.extents);
            chain.push(next);
            next = block.next;
        }
        validate_extents(&list).map_err(|e| DadaError::Corrupt(format!("inode {ino}: {e}")))?;
        for e in &list {
            self.check_extent(ino, e)?;
        }
        Ok((list, chain))
    }

    /// Stores `list` in `inode`: four extents inline, the rest in extent
    /// blocks reusing `chain`. Returns the new chain.
    fn store_extents(
        &mut self,
        ino: Ino,
        inode: &mut Inode,
        list: &[Extent],
        mut chain: Vec<u64>,
    ) -> Result<Vec<u64>, DadaError> {
        let split = list.len().min(INODE_INLINE_EXTENTS);
        let (inline, rest) = list.split_at(split);
        let chunks: Vec<&[Extent]> = rest
            .chunks(extent_block_capacity(self.sb.block_size))
            .collect();
        let missing = chunks.len().saturating_sub(chain.len()) as u64;
        if missing > self.sb.free_blocks {
            return Err(DadaError::NoSpace);
        }
        while chain.len() > chunks.len() {
            if let Some(b) = chain.pop() {
                self.release_blocks(b, 1)?;
            }
        }
        while chain.len() < chunks.len() {
            let goal = chain
                .last()
                .map(|b| b + 1)
                .or_else(|| list.last().and_then(Extent::physical_end))
                .unwrap_or(self.block_hint);
            let (b, _) = self.alloc_blocks(goal, 1)?;
            chain.push(b);
        }
        for (i, chunk) in chunks.into_iter().enumerate() {
            let (Some(&lba), next) = (chain.get(i), chain.get(i + 1).copied().unwrap_or(0)) else {
                break;
            };
            let block = ExtentBlock {
                next,
                owner: ino,
                extents: chunk.to_vec(),
            };
            self.meta_write(lba, block.encode(self.sb.block_size))?;
        }
        inode.data = InodeData::Extents(inline.to_vec());
        inode.extent_block = chain.first().copied().unwrap_or(0);
        if !chain.is_empty() {
            self.sb.features_incompat |= INCOMPAT_EXTENT_BLOCKS;
        }
        Ok(chain)
    }

    /// Frees all blocks of `inode` (data and extent blocks).
    fn free_content(&mut self, ino: Ino, inode: &mut Inode) -> Result<(), DadaError> {
        if let InodeData::Extents(_) = inode.data {
            let (list, chain) = self.load_extents(ino, inode)?;
            for e in list {
                self.release_blocks(e.physical, e.length)?;
            }
            for b in chain {
                self.release_blocks(b, 1)?;
            }
        }
        inode.data = InodeData::Extents(Vec::new());
        inode.extent_block = 0;
        Ok(())
    }

    fn attr(&mut self, ino: Ino, inode: &Inode) -> Result<Attr, DadaError> {
        let fs_blocks = match inode.data {
            InodeData::Inline(_) => 0,
            InodeData::Extents(_) => {
                let (list, chain) = self.load_extents(ino, inode)?;
                mapped_blocks(&list).saturating_add(chain.len() as u64)
            }
        };
        Ok(Attr {
            ino,
            kind: inode.kind()?,
            size: inode.size,
            blocks: fs_blocks.saturating_mul(self.bs() / 512),
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

    // -----------------------------------------------------------------------
    // File content
    // -----------------------------------------------------------------------

    fn read_content(
        &mut self,
        ino: Ino,
        inode: &Inode,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, DadaError> {
        if offset >= inode.size {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(inode.size - offset) as usize;
        let out = buf.get_mut(..n).ok_or(DadaError::Invalid)?;
        match &inode.data {
            InodeData::Inline(data) => {
                let start = offset as usize;
                let src = data
                    .get(start..start + n)
                    .ok_or_else(|| DadaError::Corrupt(format!("inode {ino}: inline size")))?;
                out.copy_from_slice(src);
            }
            InodeData::Extents(_) => {
                let (list, _) = self.load_extents(ino, inode)?;
                let bs = self.bs();
                let mut block = vec![0u8; bs as usize];
                let mut done = 0usize;
                while done < n {
                    let pos = offset + done as u64;
                    let within = (pos % bs) as usize;
                    let take = (bs as usize - within).min(n - done);
                    let dst = out.get_mut(done..done + take).ok_or(DadaError::Invalid)?;
                    match map_block(&list, pos / bs) {
                        Some(p) => {
                            self.dev.read_block(p, &mut block)?;
                            let src = block.get(within..within + take).ok_or(DadaError::Invalid)?;
                            dst.copy_from_slice(src);
                        }
                        None => dst.fill(0),
                    }
                    done += take;
                }
            }
        }
        Ok(n)
    }

    /// Allocates blocks for every hole of the logical range `first..end`.
    fn allocate_holes(
        &mut self,
        list: &mut Vec<Extent>,
        first: u64,
        end: u64,
        new_runs: &mut Vec<(u64, u64)>,
    ) -> Result<(), DadaError> {
        for (start, len) in holes(list, first, end) {
            let mut logical = start;
            let mut remaining = len;
            while remaining > 0 {
                // Goal: right after the block preceding this one in the file.
                let goal = logical
                    .checked_sub(1)
                    .and_then(|prev| map_block(list, prev))
                    .map_or(self.block_hint, |p| p + 1);
                let (physical, n) = self.alloc_blocks(goal, remaining)?;
                new_runs.push((physical, n));
                insert_extent(
                    list,
                    Extent {
                        logical,
                        physical,
                        length: n,
                    },
                );
                logical += n;
                remaining -= n;
            }
        }
        Ok(())
    }

    /// Writes `data` at `offset` in an extent-mapped inode, allocating holes.
    fn write_mapped(
        &mut self,
        ino: Ino,
        inode: &mut Inode,
        offset: u64,
        data: &[u8],
    ) -> Result<(), DadaError> {
        if data.is_empty() {
            return Ok(());
        }
        let bs = self.bs();
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(DadaError::Invalid)?;
        let (first, last) = (offset / bs, (end - 1) / bs);
        let (mut list, chain) = self.load_extents(ino, inode)?;
        let mut new_runs = Vec::new();
        if let Err(e) = self.allocate_holes(&mut list, first, last + 1, &mut new_runs) {
            self.roll_back(&new_runs);
            return Err(e);
        }
        if !new_runs.is_empty() {
            let old = (inode.data.clone(), inode.extent_block);
            if let Err(e) = self.store_extents(ino, inode, &list, chain) {
                (inode.data, inode.extent_block) = old;
                self.roll_back(&new_runs);
                return Err(e);
            }
        }
        let is_new = |p: u64| new_runs.iter().any(|&(s, n)| p >= s && p < s + n);
        let mut block = vec![0u8; bs as usize];
        for logical in first..=last {
            let physical = map_block(&list, logical)
                .ok_or_else(|| DadaError::Corrupt(format!("inode {ino}: unmapped block")))?;
            let block_start = logical * bs;
            let from = offset.max(block_start) - block_start;
            let to = end.min(block_start + bs) - block_start;
            let src_start = (block_start + from - offset) as usize;
            let src = data
                .get(src_start..src_start + (to - from) as usize)
                .ok_or(DadaError::Invalid)?;
            if from == 0 && to == bs {
                self.dev.write_block(physical, src)?;
            } else {
                if is_new(physical) {
                    block.fill(0);
                } else {
                    self.dev.read_block(physical, &mut block)?;
                }
                block
                    .get_mut(from as usize..to as usize)
                    .ok_or(DadaError::Invalid)?
                    .copy_from_slice(src);
                self.dev.write_block(physical, &block)?;
            }
        }
        Ok(())
    }

    /// Moves inline content to blocks.
    fn uninline(&mut self, ino: Ino, inode: &mut Inode) -> Result<(), DadaError> {
        if let InodeData::Inline(content) = &inode.data {
            let content = content.clone();
            inode.data = InodeData::Extents(Vec::new());
            if let Err(e) = self.write_mapped(ino, inode, 0, &content) {
                inode.data = InodeData::Inline(content);
                return Err(e);
            }
        }
        Ok(())
    }

    /// Writes content; keeps it inline while the file fits in the inode.
    fn write_content(
        &mut self,
        ino: Ino,
        inode: &mut Inode,
        offset: u64,
        data: &[u8],
    ) -> Result<(), DadaError> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(DadaError::Invalid)?;
        let new_size = inode.size.max(end);
        if let InodeData::Inline(content) = &mut inode.data {
            if new_size <= INLINE_DATA_MAX as u64 {
                content.resize(new_size as usize, 0);
                let start = offset as usize;
                content
                    .get_mut(start..start + data.len())
                    .ok_or(DadaError::Invalid)?
                    .copy_from_slice(data);
                inode.size = new_size;
                return Ok(());
            }
        }
        let was_inline = match &inode.data {
            InodeData::Inline(content) => Some(content.clone()),
            InodeData::Extents(_) => None,
        };
        self.uninline(ino, inode)?;
        if let Err(e) = self.write_mapped(ino, inode, offset, data) {
            // Undo the conversion so the inode keeps its inline content.
            if let Some(content) = was_inline {
                let _ = self.free_content(ino, inode);
                inode.data = InodeData::Inline(content);
            }
            return Err(e);
        }
        inode.size = new_size;
        Ok(())
    }

    /// Truncates or extends to `new_size`; extension leaves a hole.
    fn set_size(&mut self, ino: Ino, inode: &mut Inode, new_size: u64) -> Result<(), DadaError> {
        let inline_max = INLINE_DATA_MAX as u64;
        match &mut inode.data {
            InodeData::Inline(content) if new_size <= inline_max => {
                content.resize(new_size as usize, 0);
            }
            InodeData::Inline(_) => self.uninline(ino, inode)?,
            InodeData::Extents(_) if new_size <= inline_max => {
                let mut content = vec![0u8; new_size as usize];
                self.read_content(ino, inode, 0, &mut content)?;
                self.free_content(ino, inode)?;
                inode.data = InodeData::Inline(content);
            }
            InodeData::Extents(_) if new_size < inode.size => {
                let bs = self.bs();
                let keep = new_size.div_ceil(bs);
                let (mut list, chain) = self.load_extents(ino, inode)?;
                let freed = truncate_extents(&mut list, keep);
                // Zero the end of the last block, so a later extension reads zeros.
                let tail = (new_size % bs) as usize;
                if let Some(p) = map_block(&list, keep - 1).filter(|_| tail != 0) {
                    let mut block = vec![0u8; bs as usize];
                    self.dev.read_block(p, &mut block)?;
                    if let Some(rest) = block.get_mut(tail..) {
                        rest.fill(0);
                    }
                    self.dev.write_block(p, &block)?;
                }
                self.store_extents(ino, inode, &list, chain)?;
                for (p, n) in freed {
                    self.release_blocks(p, n)?;
                }
            }
            InodeData::Extents(_) => {}
        }
        inode.size = new_size;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Directories
    // -----------------------------------------------------------------------

    fn open_dir(&mut self, ino: Ino) -> Result<Dir, DadaError> {
        let inode = self.read_allocated(ino)?;
        if inode.kind()? != FileKind::Directory {
            return Err(DadaError::NotDir);
        }
        let bs = self.bs();
        if inode.size == 0 || inode.size % bs != 0 {
            return Err(DadaError::Corrupt(format!(
                "directory {ino}: size {} is not a whole number of blocks",
                inode.size
            )));
        }
        let (extents, chain) = self.load_extents(ino, &inode)?;
        Ok(Dir {
            ino,
            blocks: inode.size / bs,
            inode,
            extents,
            chain,
        })
    }

    fn read_dir_block(&mut self, dir: &Dir, index: u64) -> Result<(u64, DirBlock), DadaError> {
        let ino = dir.ino;
        let lba = map_block(&dir.extents, index)
            .ok_or_else(|| DadaError::Corrupt(format!("directory {ino}: hole at block {index}")))?;
        let buf = self.meta_read(lba)?;
        let block = DirBlock::parse(&buf)
            .map_err(|e| DadaError::Corrupt(format!("directory {ino}, block {index}: {e}")))?;
        Ok((lba, block))
    }

    fn check_entry(&self, dir: Ino, ino: Ino) -> Result<Ino, DadaError> {
        if ino >= self.sb.inode_count {
            return Err(DadaError::Corrupt(format!(
                "directory {dir}: entry points to inode {ino}, beyond inode_count"
            )));
        }
        Ok(ino)
    }

    fn dir_find(&mut self, dir: &Dir, name: &str) -> Result<Option<Found>, DadaError> {
        let matcher = NameMatcher::new(name, self.casefold());
        for index in 0..dir.blocks {
            let (_, block) = self.read_dir_block(dir, index)?;
            let slot = block
                .entries()
                .find(|s| matcher.matches(&s.name))
                .map(|s| (s.offset, s.ino, s.kind));
            if let Some((offset, ino, kind)) = slot {
                return Ok(Some(Found {
                    index,
                    offset,
                    ino: self.check_entry(dir.ino, ino)?,
                    kind: kind.ok_or(DadaError::Invalid)?,
                }));
            }
        }
        Ok(None)
    }

    fn dir_is_empty(&mut self, dir: &Dir) -> Result<bool, DadaError> {
        for index in 0..dir.blocks {
            let (_, block) = self.read_dir_block(dir, index)?;
            if block.entries().any(|s| s.name != "." && s.name != "..") {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Adds an entry, growing the directory by one block if needed. The
    /// caller writes `dir.inode`.
    fn dir_add(
        &mut self,
        dir: &mut Dir,
        name: &str,
        ino: Ino,
        kind: FileKind,
    ) -> Result<(), DadaError> {
        for index in 0..dir.blocks {
            let (lba, mut block) = self.read_dir_block(dir, index)?;
            if block.insert(ino, kind, name)? {
                return self.meta_write(lba, block.encode());
            }
        }
        let goal = dir
            .extents
            .last()
            .and_then(Extent::physical_end)
            .unwrap_or(self.block_hint);
        let (lba, _) = self.alloc_blocks(goal, 1)?;
        let mut list = dir.extents.clone();
        insert_extent(
            &mut list,
            Extent {
                logical: dir.blocks,
                physical: lba,
                length: 1,
            },
        );
        let chain = match self.store_extents(dir.ino, &mut dir.inode, &list, dir.chain.clone()) {
            Ok(chain) => chain,
            Err(e) => {
                self.roll_back(&[(lba, 1)]);
                return Err(e);
            }
        };
        let mut block = DirBlock::empty(self.sb.block_size)?;
        block.insert(ino, kind, name)?;
        self.meta_write(lba, block.encode())?;
        dir.extents = list;
        dir.chain = chain;
        dir.blocks += 1;
        dir.inode.size += self.bs();
        Ok(())
    }

    /// Removes an entry and releases the empty blocks at the end of the
    /// directory. The caller writes `dir.inode`.
    fn dir_remove(&mut self, dir: &mut Dir, found: &Found) -> Result<(), DadaError> {
        let (lba, mut block) = self.read_dir_block(dir, found.index)?;
        block.remove(found.offset)?;
        self.meta_write(lba, block.encode())?;

        let mut blocks = dir.blocks;
        while blocks > 1 {
            let (_, last) = self.read_dir_block(dir, blocks - 1)?;
            if !last.is_empty() {
                break;
            }
            blocks -= 1;
        }
        if blocks < dir.blocks {
            let mut list = dir.extents.clone();
            let freed = truncate_extents(&mut list, blocks);
            dir.chain = self.store_extents(dir.ino, &mut dir.inode, &list, dir.chain.clone())?;
            for (p, n) in freed {
                self.release_blocks(p, n)?;
            }
            dir.extents = list;
            dir.blocks = blocks;
            dir.inode.size = blocks * self.bs();
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Public operations
    // -----------------------------------------------------------------------

    pub fn getattr(&mut self, ino: Ino) -> Result<Attr, DadaError> {
        let inode = self.read_allocated(ino)?;
        self.attr(ino, &inode)
    }

    pub fn setattr(&mut self, ino: Ino, changes: &SetAttr) -> Result<Attr, DadaError> {
        self.writable()?;
        let mut inode = self.read_allocated(ino)?;
        if let Some(size) = changes.size {
            match inode.kind()? {
                FileKind::RegularFile => self.set_size(ino, &mut inode, size)?,
                FileKind::Directory => return Err(DadaError::IsDir),
                FileKind::Symlink => return Err(DadaError::Invalid),
            }
            inode.mtime = now_ns();
        }
        if let Some(mode) = changes.mode {
            inode.mode = (inode.mode & !MODE_PERM_MASK) | (u32::from(mode) & MODE_PERM_MASK);
        }
        if let Some(uid) = changes.uid {
            inode.uid = uid;
        }
        if let Some(gid) = changes.gid {
            inode.gid = gid;
        }
        if let Some(atime) = changes.atime {
            inode.atime = atime;
        }
        if let Some(mtime) = changes.mtime {
            inode.mtime = mtime;
        }
        if let Some(win_attrs) = changes.win_attrs {
            inode.win_attrs = win_attrs;
            self.sb.features_compat |= COMPAT_WIN_ATTRS;
        }
        inode.ctime = now_ns();
        self.write_inode(ino, &inode)?;
        self.attr(ino, &inode)
    }

    /// Entries of directory `ino`, `.` and `..` included, after cookie `offset`
    /// (0 to start). Each entry comes with the cookie to resume after it.
    pub fn readdir(
        &mut self,
        ino: Ino,
        offset: u64,
    ) -> Result<Vec<(u64, DirEntryInfo)>, DadaError> {
        let dir = self.open_dir(ino)?;
        let bs = self.bs();
        let mut out = Vec::new();
        for index in offset / bs..dir.blocks {
            let (_, block) = self.read_dir_block(&dir, index)?;
            for slot in block.entries() {
                // Cookie = position of the entry in the directory + 1.
                let cookie = index * bs + slot.offset as u64 + 1;
                if cookie <= offset {
                    continue;
                }
                let entry = DirEntryInfo {
                    ino: self.check_entry(ino, slot.ino)?,
                    kind: slot.kind.ok_or(DadaError::Invalid)?,
                    name: slot.name.clone(),
                };
                out.push((cookie, entry));
            }
        }
        Ok(out)
    }

    /// Looks up `name` in directory `parent`.
    pub fn lookup(&mut self, parent: Ino, name: &str) -> Result<Attr, DadaError> {
        let name = normalize(name)?;
        let name = name.as_str();
        let dir = self.open_dir(parent)?;
        let found = self.dir_find(&dir, name)?.ok_or(DadaError::NotFound)?;
        let inode = self.read_referenced(parent, found.ino)?;
        self.attr(found.ino, &inode)
    }

    /// Creates a new inode and its entry in `parent`. `init` fills the
    /// content of the new inode (it may allocate blocks).
    fn create_node(
        &mut self,
        parent: Ino,
        name: &str,
        mut inode: Inode,
        init: impl FnOnce(&mut Self, Ino, &mut Inode) -> Result<(), DadaError>,
    ) -> Result<Attr, DadaError> {
        self.writable()?;
        let name = normalize_new(name)?;
        let name = name.as_str();
        let mut dir = self.open_dir(parent)?;
        if self.dir_find(&dir, name)?.is_some() {
            return Err(DadaError::Exists);
        }
        let kind = inode.kind()?;
        let (ino, generation) = self.alloc_inode()?;
        inode.generation = generation;
        let result = init(self, ino, &mut inode)
            .and_then(|()| self.write_inode(ino, &inode))
            .and_then(|()| self.dir_add(&mut dir, name, ino, kind));
        if let Err(e) = result {
            let _ = self.release_inode(ino, inode);
            return Err(e);
        }
        let now = now_ns();
        dir.inode.mtime = now;
        dir.inode.ctime = now;
        if kind == FileKind::Directory {
            dir.inode.links = dir.inode.links.saturating_add(1);
        }
        self.write_inode(parent, &dir.inode)?;
        self.attr(ino, &inode)
    }

    fn new_inode(kind: FileKind, mode: u16, uid: u32, gid: u32) -> Inode {
        let now = now_ns();
        Inode {
            mode: kind.mode_bits() | (u32::from(mode) & MODE_PERM_MASK),
            uid,
            gid,
            links: 1,
            atime: now,
            mtime: now,
            ctime: now,
            btime: now,
            data: InodeData::Inline(Vec::new()),
            ..Inode::default()
        }
    }

    pub fn create(
        &mut self,
        parent: Ino,
        name: &str,
        mode: u16,
        uid: u32,
        gid: u32,
    ) -> Result<Attr, DadaError> {
        let inode = Self::new_inode(FileKind::RegularFile, mode, uid, gid);
        self.create_node(parent, name, inode, |_, _, _| Ok(()))
    }

    pub fn mkdir(
        &mut self,
        parent: Ino,
        name: &str,
        mode: u16,
        uid: u32,
        gid: u32,
    ) -> Result<Attr, DadaError> {
        let mut inode = Self::new_inode(FileKind::Directory, mode, uid, gid);
        inode.links = 2;
        inode.data = InodeData::Extents(Vec::new());
        self.create_node(parent, name, inode, |vol, ino, inode| {
            let goal = vol.block_hint;
            let (lba, _) = vol.alloc_blocks(goal, 1)?;
            let mut block = DirBlock::empty(vol.sb.block_size)?;
            block.insert(ino, FileKind::Directory, ".")?;
            block.insert(parent, FileKind::Directory, "..")?;
            vol.meta_write(lba, block.encode())?;
            inode.data = InodeData::Extents(vec![Extent {
                logical: 0,
                physical: lba,
                length: 1,
            }]);
            inode.size = vol.bs();
            Ok(())
        })
    }

    pub fn symlink(
        &mut self,
        parent: Ino,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<Attr, DadaError> {
        if target.is_empty() || target.contains('\0') {
            return Err(DadaError::Invalid);
        }
        if target.len() > MAX_SYMLINK_LEN {
            return Err(DadaError::NameTooLong);
        }
        let inode = Self::new_inode(FileKind::Symlink, 0o777, uid, gid);
        self.create_node(parent, name, inode, |vol, ino, inode| {
            vol.write_content(ino, inode, 0, target.as_bytes())
        })
    }

    pub fn link(&mut self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Attr, DadaError> {
        self.writable()?;
        let new_name = normalize_new(new_name)?;
        let new_name = new_name.as_str();
        let mut inode = self.read_allocated(ino)?;
        let kind = inode.kind()?;
        if kind == FileKind::Directory {
            return Err(DadaError::IsDir);
        }
        let mut dir = self.open_dir(new_parent)?;
        if self.dir_find(&dir, new_name)?.is_some() {
            return Err(DadaError::Exists);
        }
        inode.links = inode.links.checked_add(1).ok_or(DadaError::Invalid)?;
        self.dir_add(&mut dir, new_name, ino, kind)?;
        let now = now_ns();
        inode.ctime = now;
        dir.inode.mtime = now;
        dir.inode.ctime = now;
        self.write_inode(ino, &inode)?;
        self.write_inode(new_parent, &dir.inode)?;
        self.attr(ino, &inode)
    }

    pub fn unlink(&mut self, parent: Ino, name: &str) -> Result<(), DadaError> {
        self.writable()?;
        let name = normalize(name)?;
        let name = name.as_str();
        let mut dir = self.open_dir(parent)?;
        let found = self.dir_find(&dir, name)?.ok_or(DadaError::NotFound)?;
        if found.kind == FileKind::Directory {
            return Err(DadaError::IsDir);
        }
        let mut inode = self.read_referenced(parent, found.ino)?;
        self.dir_remove(&mut dir, &found)?;
        let now = now_ns();
        dir.inode.mtime = now;
        dir.inode.ctime = now;
        self.write_inode(parent, &dir.inode)?;
        inode.links = inode.links.saturating_sub(1);
        if inode.links == 0 {
            self.release_inode(found.ino, inode)
        } else {
            inode.ctime = now;
            self.write_inode(found.ino, &inode)
        }
    }

    pub fn rmdir(&mut self, parent: Ino, name: &str) -> Result<(), DadaError> {
        self.writable()?;
        let name = normalize(name)?;
        let name = name.as_str();
        match name {
            "." => return Err(DadaError::Invalid),
            ".." => return Err(DadaError::NotEmpty),
            _ => {}
        }
        let mut dir = self.open_dir(parent)?;
        let found = self.dir_find(&dir, name)?.ok_or(DadaError::NotFound)?;
        if found.kind != FileKind::Directory {
            return Err(DadaError::NotDir);
        }
        let target = self.open_dir(found.ino).map_err(|e| match e {
            DadaError::NotFound | DadaError::NotDir => DadaError::Corrupt(format!(
                "directory {parent}: entry {name:?} is not a directory"
            )),
            e => e,
        })?;
        if !self.dir_is_empty(&target)? {
            return Err(DadaError::NotEmpty);
        }
        self.dir_remove(&mut dir, &found)?;
        let now = now_ns();
        dir.inode.mtime = now;
        dir.inode.ctime = now;
        dir.inode.links = dir.inode.links.saturating_sub(1).max(2);
        self.write_inode(parent, &dir.inode)?;
        self.release_inode(found.ino, target.inode)
    }

    /// Whether directory `dir` is `ancestor` or lies below it.
    fn is_within(&mut self, mut dir: Ino, ancestor: Ino) -> Result<bool, DadaError> {
        // Each step goes one level up; a longer walk means a loop on disk.
        for _ in 0..self.sb.inode_count {
            if dir == ancestor {
                return Ok(true);
            }
            if dir == INO_ROOT {
                return Ok(false);
            }
            let handle = self.open_dir(dir)?;
            let parent = self
                .dir_find(&handle, "..")?
                .ok_or_else(|| DadaError::Corrupt(format!("directory {dir} has no ..")))?;
            dir = parent.ino;
        }
        Err(DadaError::Corrupt("directory loop".into()))
    }

    /// Points an existing entry of `dir` to another inode.
    fn dir_set_target(
        &mut self,
        dir: &Dir,
        found: &Found,
        ino: Ino,
        kind: FileKind,
    ) -> Result<(), DadaError> {
        let (lba, mut block) = self.read_dir_block(dir, found.index)?;
        block.set_target(found.offset, ino, kind)?;
        self.meta_write(lba, block.encode())
    }

    /// Adds `delta` to the link count of directory `ino` and updates its times.
    fn touch_dir(&mut self, ino: Ino, delta: i64, now: i64) -> Result<(), DadaError> {
        let mut inode = self.read_allocated(ino)?;
        inode.links = u32::try_from(i64::from(inode.links) + delta)
            .unwrap_or(2)
            .max(2);
        inode.mtime = now;
        inode.ctime = now;
        self.write_inode(ino, &inode)
    }

    /// Renames `parent/name` to `new_parent/new_name`, replacing a compatible
    /// destination (POSIX semantics).
    pub fn rename(
        &mut self,
        parent: Ino,
        name: &str,
        new_parent: Ino,
        new_name: &str,
    ) -> Result<(), DadaError> {
        self.writable()?;
        let name = normalize(name)?;
        if name == "." || name == ".." {
            return Err(DadaError::Invalid);
        }
        let new_name = normalize_new(new_name)?;
        let src_dir = self.open_dir(parent)?;
        let src = self.dir_find(&src_dir, &name)?.ok_or(DadaError::NotFound)?;
        let dst_dir = self.open_dir(new_parent)?;
        let dst = self.dir_find(&dst_dir, &new_name)?;
        let moving_dir = src.kind == FileKind::Directory;
        if moving_dir && self.is_within(new_parent, src.ino)? {
            return Err(DadaError::Invalid);
        }
        let now = now_ns();

        if let Some(d) = &dst {
            if d.ino == src.ino {
                // Same inode. Only a case change of the same entry does anything.
                let same_entry =
                    parent == new_parent && d.index == src.index && d.offset == src.offset;
                if same_entry && name != new_name {
                    let mut dir = self.open_dir(parent)?;
                    self.dir_remove(&mut dir, &src)?;
                    self.dir_add(&mut dir, &new_name, src.ino, src.kind)?;
                    dir.inode.mtime = now;
                    dir.inode.ctime = now;
                    self.write_inode(parent, &dir.inode)?;
                }
                return Ok(());
            }
            match (moving_dir, d.kind == FileKind::Directory) {
                (true, false) => return Err(DadaError::NotDir),
                (false, true) => return Err(DadaError::IsDir),
                (true, true) => {
                    let target = self.open_dir(d.ino)?;
                    if !self.dir_is_empty(&target)? {
                        return Err(DadaError::NotEmpty);
                    }
                }
                (false, false) => {}
            }
        }

        // 1. The destination name now designates the source inode.
        match &dst {
            Some(d) => self.dir_set_target(&dst_dir, d, src.ino, src.kind)?,
            None => {
                let mut dir = self.open_dir(new_parent)?;
                self.dir_add(&mut dir, &new_name, src.ino, src.kind)?;
                self.write_inode(new_parent, &dir.inode)?;
            }
        }
        // 2. The source name disappears.
        let mut dir = self.open_dir(parent)?;
        let old = self
            .dir_find(&dir, &name)?
            .filter(|f| f.ino == src.ino)
            .ok_or_else(|| DadaError::Corrupt(format!("directory {parent}: entry vanished")))?;
        self.dir_remove(&mut dir, &old)?;
        self.write_inode(parent, &dir.inode)?;

        // 3. A moved directory points `..` at its new parent.
        if moving_dir && parent != new_parent {
            let moved = self.open_dir(src.ino)?;
            let dotdot = self
                .dir_find(&moved, "..")?
                .ok_or_else(|| DadaError::Corrupt(format!("directory {} has no ..", src.ino)))?;
            self.dir_set_target(&moved, &dotdot, new_parent, FileKind::Directory)?;
        }

        // 4. Link counts and times.
        let replaced_dir = dst.as_ref().is_some_and(|d| d.kind == FileKind::Directory);
        let moved_across = moving_dir && parent != new_parent;
        let parent_delta = if moved_across { -1 } else { 0 };
        let new_parent_delta = i64::from(moved_across) - i64::from(replaced_dir);
        if parent == new_parent {
            self.touch_dir(parent, parent_delta + new_parent_delta, now)?;
        } else {
            self.touch_dir(parent, parent_delta, now)?;
            self.touch_dir(new_parent, new_parent_delta, now)?;
        }
        let mut inode = self.read_referenced(new_parent, src.ino)?;
        inode.ctime = now;
        self.write_inode(src.ino, &inode)?;

        // 5. The replaced inode loses a link.
        if let Some(d) = dst {
            let mut old = self.read_referenced(new_parent, d.ino)?;
            if replaced_dir {
                self.release_inode(d.ino, old)?;
            } else {
                old.links = old.links.saturating_sub(1);
                if old.links == 0 {
                    self.release_inode(d.ino, old)?;
                } else {
                    old.ctime = now;
                    self.write_inode(d.ino, &old)?;
                }
            }
        }
        Ok(())
    }

    /// For fsck: adds an entry `parent/name` for an allocated inode that no
    /// directory references. Its link count is left as is; a directory gets
    /// its `..` pointed at `parent`, which gains a link.
    #[doc(hidden)]
    pub fn attach_orphan(&mut self, ino: Ino, parent: Ino, name: &str) -> Result<(), DadaError> {
        self.writable()?;
        let name = normalize_new(name)?;
        let inode = self.read_allocated(ino)?;
        let kind = inode.kind()?;
        let mut dir = self.open_dir(parent)?;
        if self.dir_find(&dir, &name)?.is_some() {
            return Err(DadaError::Exists);
        }
        self.dir_add(&mut dir, &name, ino, kind)?;
        if kind == FileKind::Directory {
            dir.inode.links = dir.inode.links.saturating_add(1);
            let child = self.open_dir(ino)?;
            let dotdot = self
                .dir_find(&child, "..")?
                .ok_or_else(|| DadaError::Corrupt(format!("directory {ino} has no ..")))?;
            self.dir_set_target(&child, &dotdot, parent, FileKind::Directory)?;
        }
        let now = now_ns();
        dir.inode.mtime = now;
        dir.inode.ctime = now;
        self.write_inode(parent, &dir.inode)
    }

    /// Full extent list of a file, directory or symbolic link (empty for
    /// inline content).
    pub fn extents(&mut self, ino: Ino) -> Result<Vec<Extent>, DadaError> {
        let inode = self.read_allocated(ino)?;
        Ok(self.load_extents(ino, &inode)?.0)
    }

    /// Reads up to `buf.len()` bytes at `offset`; returns 0 at end of file.
    pub fn read(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize, DadaError> {
        let inode = self.read_allocated(ino)?;
        match inode.kind()? {
            FileKind::RegularFile => self.read_content(ino, &inode, offset, buf),
            FileKind::Directory => Err(DadaError::IsDir),
            FileKind::Symlink => Err(DadaError::Invalid),
        }
    }

    /// Writes `data` at `offset`; writing past the end leaves a hole.
    pub fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> Result<usize, DadaError> {
        self.writable()?;
        let mut inode = self.read_allocated(ino)?;
        match inode.kind()? {
            FileKind::RegularFile => {}
            FileKind::Directory => return Err(DadaError::IsDir),
            FileKind::Symlink => return Err(DadaError::Invalid),
        }
        if data.is_empty() {
            return Ok(0);
        }
        self.write_content(ino, &mut inode, offset, data)?;
        let now = now_ns();
        inode.mtime = now;
        inode.ctime = now;
        self.write_inode(ino, &inode)?;
        Ok(data.len())
    }

    pub fn readlink(&mut self, ino: Ino) -> Result<String, DadaError> {
        let inode = self.read_allocated(ino)?;
        if inode.kind()? != FileKind::Symlink {
            return Err(DadaError::Invalid);
        }
        if inode.size > MAX_SYMLINK_LEN as u64 {
            return Err(DadaError::Corrupt(format!(
                "symlink {ino}: target too long"
            )));
        }
        let mut buf = vec![0u8; inode.size as usize];
        let n = self.read_content(ino, &inode, 0, &mut buf)?;
        buf.truncate(n);
        String::from_utf8(buf)
            .map_err(|_| DadaError::Corrupt(format!("symlink {ino}: target is not UTF-8")))
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

    // -----------------------------------------------------------------------
    // Write operations
    // -----------------------------------------------------------------------

    fn rw(bs: u32, bytes: u64) -> Volume<MemDevice> {
        Volume::open(formatted(bs, bytes, false), false).unwrap()
    }

    /// In-memory counters agree with the bitmaps.
    fn check_counters(vol: &Volume<MemDevice>) {
        assert_eq!(
            vol.block_bitmap.count_used(),
            vol.sb.total_blocks - vol.sb.free_blocks
        );
        assert_eq!(
            vol.inode_bitmap.count_used(),
            vol.sb.inode_count - vol.sb.free_inodes
        );
    }

    fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    fn read_all(vol: &mut Volume<MemDevice>, ino: Ino) -> Vec<u8> {
        let size = vol.getattr(ino).unwrap().size as usize;
        let mut buf = vec![0xEE; size + 10];
        let n = vol.read(ino, 0, &mut buf).unwrap();
        assert_eq!(n, size);
        buf.truncate(n);
        buf
    }

    #[test]
    fn create_write_read() {
        let mut vol = rw(4096, 8 * MIB);
        let f = vol.create(INO_ROOT, "f", 0o644, 1000, 100).unwrap();
        assert_eq!(
            (f.kind, f.size, f.links, f.mode),
            (FileKind::RegularFile, 0, 1, 0o644)
        );
        assert_eq!((f.uid, f.gid), (1000, 100));
        assert!(f.ino >= FIRST_USER_INO);
        assert_eq!(vol.lookup(INO_ROOT, "f").unwrap().ino, f.ino);

        // Small writes stay inline.
        vol.write(f.ino, 0, b"hello").unwrap();
        vol.write(f.ino, 10, b"world").unwrap();
        assert_eq!(read_all(&mut vol, f.ino), b"hello\0\0\0\0\0world");
        assert_eq!(vol.getattr(f.ino).unwrap().blocks, 0);
        assert!(matches!(
            vol.read_inode(f.ino).unwrap().data,
            InodeData::Inline(_)
        ));

        // Crossing 96 bytes moves the content to blocks.
        let big = pattern(1, 10_000);
        vol.write(f.ino, 15, &big).unwrap();
        let mut expected = b"hello\0\0\0\0\0world".to_vec();
        expected.extend_from_slice(&big);
        assert_eq!(read_all(&mut vol, f.ino), expected);
        assert_eq!(vol.getattr(f.ino).unwrap().blocks, 3 * 8);

        // Partial reads and reads past the end.
        let mut buf = [0u8; 4];
        assert_eq!(vol.read(f.ino, 1, &mut buf).unwrap(), 4);
        assert_eq!(&buf, b"ello");
        assert_eq!(
            vol.read(f.ino, expected.len() as u64 - 2, &mut buf)
                .unwrap(),
            2
        );
        assert_eq!(vol.read(f.ino, 1 << 40, &mut buf).unwrap(), 0);
        check_counters(&vol);
    }

    #[test]
    fn writes_past_the_end_leave_holes() {
        let mut vol = rw(1024, 8 * MIB);
        let f = vol.create(INO_ROOT, "sparse", 0o600, 0, 0).unwrap();
        vol.write(f.ino, 100 * 1024 + 10, b"tail").unwrap();
        let attr = vol.getattr(f.ino).unwrap();
        assert_eq!(attr.size, 100 * 1024 + 14);
        assert_eq!(attr.blocks, 2, "only the last block is allocated");
        let content = read_all(&mut vol, f.ino);
        assert!(content[..100 * 1024 + 10].iter().all(|&b| b == 0));
        assert_eq!(&content[100 * 1024 + 10..], b"tail");

        // Filling the hole later.
        vol.write(f.ino, 2048, &pattern(3, 1024)).unwrap();
        assert_eq!(
            &read_all(&mut vol, f.ino)[2048..3072],
            &pattern(3, 1024)[..]
        );
        assert_eq!(vol.getattr(f.ino).unwrap().blocks, 4);
        check_counters(&vol);
    }

    #[test]
    fn truncate_and_extend() {
        let mut vol = rw(1024, 8 * MIB);
        let base = vol.statfs();
        let f = vol.create(INO_ROOT, "t", 0o600, 0, 0).unwrap();
        vol.write(f.ino, 0, &pattern(7, 5000)).unwrap();

        let shrink = SetAttr {
            size: Some(1500),
            ..SetAttr::default()
        };
        assert_eq!(vol.setattr(f.ino, &shrink).unwrap().blocks, 4);
        // Extending again reads zeros past the old end, even inside the last block.
        let grow = SetAttr {
            size: Some(3000),
            ..SetAttr::default()
        };
        let attr = vol.setattr(f.ino, &grow).unwrap();
        assert_eq!((attr.size, attr.blocks), (3000, 4));
        let content = read_all(&mut vol, f.ino);
        assert_eq!(&content[..1500], &pattern(7, 5000)[..1500]);
        assert!(content[1500..].iter().all(|&b| b == 0));

        // Down to 96 bytes or less: back inline, all blocks released.
        let small = SetAttr {
            size: Some(50),
            ..SetAttr::default()
        };
        assert_eq!(vol.setattr(f.ino, &small).unwrap().blocks, 0);
        assert_eq!(read_all(&mut vol, f.ino), &pattern(7, 5000)[..50]);
        assert_eq!(vol.statfs().free_blocks, base.free_blocks);
        let zero = SetAttr {
            size: Some(0),
            ..SetAttr::default()
        };
        vol.setattr(f.ino, &zero).unwrap();
        assert!(read_all(&mut vol, f.ino).is_empty());
        // Inline file extended past 96 bytes: content kept, rest is a hole.
        vol.write(f.ino, 0, b"abc").unwrap();
        vol.setattr(f.ino, &grow).unwrap();
        let content = read_all(&mut vol, f.ino);
        assert_eq!(&content[..3], b"abc");
        assert!(content[3..].iter().all(|&b| b == 0));
        check_counters(&vol);
    }

    #[test]
    fn directories() {
        let mut vol = rw(4096, 8 * MIB);
        let d = vol.mkdir(INO_ROOT, "d", 0o750, 5, 6).unwrap();
        assert_eq!(
            (d.kind, d.links, d.size, d.mode),
            (FileKind::Directory, 2, 4096, 0o750)
        );
        assert_eq!(vol.getattr(INO_ROOT).unwrap().links, 3);
        let entries = vol.readdir(d.ino, 0).unwrap();
        assert_eq!(names(&entries), [".", ".."]);
        assert_eq!((entries[0].1.ino, entries[1].1.ino), (d.ino, INO_ROOT));

        let sub = vol.mkdir(d.ino, "sub", 0o755, 0, 0).unwrap();
        vol.create(sub.ino, "f", 0o644, 0, 0).unwrap();
        assert_eq!(vol.lookup(sub.ino, "..").unwrap().ino, d.ino);
        assert_eq!(vol.getattr(d.ino).unwrap().links, 3);

        assert!(matches!(vol.rmdir(d.ino, "sub"), Err(DadaError::NotEmpty)));
        assert!(matches!(
            vol.rmdir(INO_ROOT, "absent"),
            Err(DadaError::NotFound)
        ));
        assert!(matches!(vol.rmdir(sub.ino, "f"), Err(DadaError::NotDir)));
        assert!(matches!(vol.unlink(d.ino, "sub"), Err(DadaError::IsDir)));
        assert!(matches!(vol.rmdir(d.ino, "."), Err(DadaError::Invalid)));
        assert!(matches!(vol.rmdir(d.ino, ".."), Err(DadaError::NotEmpty)));

        vol.unlink(sub.ino, "f").unwrap();
        vol.rmdir(d.ino, "sub").unwrap();
        assert_eq!(vol.getattr(d.ino).unwrap().links, 2);
        assert!(matches!(vol.getattr(sub.ino), Err(DadaError::NotFound)));
        vol.rmdir(INO_ROOT, "d").unwrap();
        assert_eq!(vol.getattr(INO_ROOT).unwrap().links, 2);
        assert_eq!(names(&vol.readdir(INO_ROOT, 0).unwrap()), [".", ".."]);
        check_counters(&vol);
    }

    #[test]
    fn name_errors() {
        let mut vol = rw(4096, 8 * MIB);
        vol.create(INO_ROOT, "x", 0o644, 0, 0).unwrap();
        assert!(matches!(
            vol.create(INO_ROOT, "x", 0o644, 0, 0),
            Err(DadaError::Exists)
        ));
        assert!(matches!(
            vol.mkdir(INO_ROOT, "x", 0o755, 0, 0),
            Err(DadaError::Exists)
        ));
        for bad in [".", "..", "", "a/b", "nul\0"] {
            assert!(matches!(
                vol.create(INO_ROOT, bad, 0o644, 0, 0),
                Err(DadaError::InvalidName)
            ));
        }
        assert!(matches!(
            vol.create(INO_ROOT, &"n".repeat(256), 0o644, 0, 0),
            Err(DadaError::NameTooLong)
        ));
        let x = vol.lookup(INO_ROOT, "x").unwrap();
        assert!(matches!(
            vol.create(x.ino, "y", 0o644, 0, 0),
            Err(DadaError::NotDir)
        ));
        assert!(matches!(
            vol.unlink(INO_ROOT, "y"),
            Err(DadaError::NotFound)
        ));
        assert!(matches!(
            vol.read(INO_ROOT, 0, &mut [0; 4]),
            Err(DadaError::IsDir)
        ));
        assert!(matches!(
            vol.write(INO_ROOT, 0, b"x"),
            Err(DadaError::IsDir)
        ));
    }

    #[test]
    fn symlinks() {
        let mut vol = rw(1024, 8 * MIB);
        let short = vol.symlink(INO_ROOT, "s", "/etc/hosts", 0, 0).unwrap();
        assert_eq!(
            (short.kind, short.size, short.mode),
            (FileKind::Symlink, 10, 0o777)
        );
        assert_eq!(vol.readlink(short.ino).unwrap(), "/etc/hosts");
        let target = "a/".repeat(1000) + "end";
        let long = vol.symlink(INO_ROOT, "l", &target, 0, 0).unwrap();
        assert_eq!(long.blocks, 2 * 2);
        assert_eq!(vol.readlink(long.ino).unwrap(), target);
        assert!(matches!(vol.readlink(INO_ROOT), Err(DadaError::Invalid)));
        assert!(matches!(
            vol.read(short.ino, 0, &mut [0; 4]),
            Err(DadaError::Invalid)
        ));
        assert!(matches!(
            vol.symlink(INO_ROOT, "e", "", 0, 0),
            Err(DadaError::Invalid)
        ));
        assert!(matches!(
            vol.symlink(INO_ROOT, "e", &"x".repeat(4096), 0, 0),
            Err(DadaError::NameTooLong)
        ));
        vol.unlink(INO_ROOT, "l").unwrap();
        vol.unlink(INO_ROOT, "s").unwrap();
        check_counters(&vol);
    }

    #[test]
    fn hard_links_and_release() {
        let mut vol = rw(4096, 8 * MIB);
        let base = vol.statfs();
        let d = vol.mkdir(INO_ROOT, "d", 0o755, 0, 0).unwrap();
        let f = vol.create(INO_ROOT, "f", 0o644, 0, 0).unwrap();
        vol.write(f.ino, 0, &pattern(9, 20_000)).unwrap();
        assert_eq!(vol.link(f.ino, d.ino, "g").unwrap().links, 2);
        assert!(matches!(
            vol.link(d.ino, INO_ROOT, "dd"),
            Err(DadaError::IsDir)
        ));
        assert!(matches!(
            vol.link(f.ino, d.ino, "g"),
            Err(DadaError::Exists)
        ));

        vol.unlink(INO_ROOT, "f").unwrap();
        assert_eq!(vol.getattr(f.ino).unwrap().links, 1);
        assert_eq!(read_all(&mut vol, f.ino), pattern(9, 20_000));
        vol.unlink(d.ino, "g").unwrap();
        assert!(matches!(vol.getattr(f.ino), Err(DadaError::NotFound)));
        vol.rmdir(INO_ROOT, "d").unwrap();
        let after = vol.statfs();
        assert_eq!(
            (after.free_blocks, after.free_inodes),
            (base.free_blocks, base.free_inodes)
        );

        // The freed inode is reused with a new generation.
        let gen_before = vol.read_inode(f.ino).unwrap().generation;
        let again = vol.create(INO_ROOT, "again", 0o644, 0, 0).unwrap();
        assert_eq!(again.ino, d.ino.min(f.ino));
        assert!(vol.read_inode(again.ino).unwrap().generation >= 1);
        let _ = gen_before;
        check_counters(&vol);
    }

    #[test]
    fn setattr_fields() {
        let mut vol = rw(4096, 8 * MIB);
        let f = vol.create(INO_ROOT, "f", 0o644, 0, 0).unwrap();
        let changes = SetAttr {
            mode: Some(0o4711),
            uid: Some(42),
            gid: Some(43),
            atime: Some(-1_000),
            mtime: Some(123),
            win_attrs: Some(0x22),
            size: None,
        };
        let a = vol.setattr(f.ino, &changes).unwrap();
        assert_eq!((a.mode, a.uid, a.gid), (0o4711, 42, 43));
        assert_eq!((a.atime, a.mtime, a.win_attrs), (-1_000, 123, 0x22));
        assert!(a.ctime >= f.ctime);
        assert_eq!(a.kind, FileKind::RegularFile);
        assert_ne!(vol.superblock().features_compat & COMPAT_WIN_ATTRS, 0);
        let d = vol.mkdir(INO_ROOT, "d", 0o755, 0, 0).unwrap();
        let resize = SetAttr {
            size: Some(1),
            ..SetAttr::default()
        };
        assert!(matches!(vol.setattr(d.ino, &resize), Err(DadaError::IsDir)));
    }

    #[test]
    fn read_only_volume_refuses_writes() {
        let mut vol = Volume::open(formatted(4096, 8 * MIB, true), true).unwrap();
        assert!(matches!(
            vol.create(INO_ROOT, "f", 0o644, 0, 0),
            Err(DadaError::ReadOnly)
        ));
        assert!(matches!(
            vol.mkdir(INO_ROOT, "d", 0o755, 0, 0),
            Err(DadaError::ReadOnly)
        ));
        assert!(matches!(
            vol.unlink(INO_ROOT, "f"),
            Err(DadaError::ReadOnly)
        ));
        assert!(matches!(
            vol.setattr(INO_ROOT, &SetAttr::default()),
            Err(DadaError::ReadOnly)
        ));
        assert!(matches!(
            vol.write(INO_JOURNAL, 0, b"x"),
            Err(DadaError::ReadOnly)
        ));
    }

    #[test]
    fn persistence_across_remount() {
        let mut vol = rw(4096, 8 * MIB);
        let d = vol.mkdir(INO_ROOT, "docs", 0o755, 0, 0).unwrap();
        let f = vol.create(d.ino, "a.txt", 0o644, 0, 0).unwrap();
        vol.write(f.ino, 0, &pattern(5, 70_000)).unwrap();
        vol.symlink(d.ino, "link", "a.txt", 0, 0).unwrap();
        let before = vol.statfs();
        let dev = vol.close().unwrap();

        let mut vol = Volume::open(dev, true).unwrap();
        assert_eq!(vol.statfs(), before);
        let d2 = vol.lookup(INO_ROOT, "docs").unwrap();
        let f2 = vol.lookup(d2.ino, "a.txt").unwrap();
        assert_eq!(read_all(&mut vol, f2.ino), pattern(5, 70_000));
        let l = vol.lookup(d2.ino, "link").unwrap();
        assert_eq!(vol.readlink(l.ino).unwrap(), "a.txt");
        check_counters(&vol);

        // On-disk bitmaps match the counters too.
        let sb = vol.superblock().clone();
        let bitmap = load_bitmap(&mut vol.dev, sb.block_bitmap_start, sb.total_blocks).unwrap();
        assert_eq!(bitmap.count_used(), sb.total_blocks - sb.free_blocks);
        assert!(bitmap.padding_is_set());
    }

    #[test]
    fn large_directory_with_chained_extents_shrinks_back() {
        // 1 KiB blocks and files written between directory growths fragment
        // the directory well past four extents.
        let mut vol = Volume::open_with_cache(formatted(1024, 16 * MIB, false), false, 8).unwrap();
        let base = vol.statfs();
        let n = 600;
        for i in 0..n {
            let f = vol
                .create(
                    INO_ROOT,
                    &format!("file-with-a-long-name-{i:05}"),
                    0o644,
                    0,
                    0,
                )
                .unwrap();
            vol.write(f.ino, 0, &pattern(i as u8, 1500)).unwrap();
        }
        let root = vol.read_inode(INO_ROOT).unwrap();
        assert_ne!(root.extent_block, 0, "root uses an extent block");
        assert_ne!(
            vol.superblock().features_incompat & INCOMPAT_EXTENT_BLOCKS,
            0
        );
        let listed = vol.readdir(INO_ROOT, 0).unwrap();
        assert_eq!(listed.len(), n + 2);

        // Remount through a tiny cache, check, then delete everything.
        let dev = vol.close().unwrap();
        let mut vol = Volume::open_with_cache(dev, false, 4).unwrap();
        for i in (0..n).rev() {
            let name = format!("file-with-a-long-name-{i:05}");
            let f = vol.lookup(INO_ROOT, &name).unwrap();
            assert_eq!(read_all(&mut vol, f.ino), pattern(i as u8, 1500));
            vol.unlink(INO_ROOT, &name).unwrap();
        }
        let root = vol.getattr(INO_ROOT).unwrap();
        assert_eq!(root.size, 1024);
        let after = vol.statfs();
        assert_eq!(
            (after.free_blocks, after.free_inodes),
            (base.free_blocks, base.free_inodes)
        );
        check_counters(&vol);
    }

    #[test]
    fn no_space_is_reported_and_recovered() {
        let mut vol = rw(4096, 2 * MIB);
        let base = vol.statfs();
        let f = vol.create(INO_ROOT, "big", 0o644, 0, 0).unwrap();
        let chunk = pattern(1, 64 * 1024);
        let mut offset = 0;
        let err = loop {
            match vol.write(f.ino, offset, &chunk) {
                Ok(n) => offset += n as u64,
                Err(e) => break e,
            }
        };
        assert!(matches!(err, DadaError::NoSpace));
        // A failed write is all or nothing: its partial allocation is released.
        assert!(vol.statfs().free_blocks < 16);
        assert_eq!(vol.getattr(f.ino).unwrap().size, offset);
        check_counters(&vol);
        // Other files can no longer grow past the inline size...
        let g = vol.create(INO_ROOT, "small", 0o644, 0, 0).unwrap();
        vol.write(g.ino, 0, b"fits inline").unwrap();
        assert!(matches!(
            vol.write(g.ino, 0, &chunk),
            Err(DadaError::NoSpace)
        ));
        assert_eq!(read_all(&mut vol, g.ino), b"fits inline");
        // ...until space is released.
        vol.unlink(INO_ROOT, "big").unwrap();
        vol.unlink(INO_ROOT, "small").unwrap();
        assert_eq!(vol.statfs(), base);
        check_counters(&vol);
    }

    #[test]
    fn inode_exhaustion() {
        let mut vol = rw(4096, 4 * MIB);
        let free = vol.statfs().free_inodes;
        for i in 0..free {
            vol.create(INO_ROOT, &format!("f{i}"), 0o644, 0, 0).unwrap();
        }
        assert!(matches!(
            vol.create(INO_ROOT, "one-more", 0o644, 0, 0),
            Err(DadaError::NoInodes)
        ));
        vol.unlink(INO_ROOT, "f7").unwrap();
        vol.create(INO_ROOT, "one-more", 0o644, 0, 0).unwrap();
        check_counters(&vol);
    }

    // -----------------------------------------------------------------------
    // Rename and names
    // -----------------------------------------------------------------------

    fn ino_of(vol: &mut Volume<MemDevice>, parent: Ino, name: &str) -> Ino {
        vol.lookup(parent, name).unwrap().ino
    }

    #[test]
    fn rename_files() {
        let mut vol = rw(4096, 8 * MIB);
        let base = vol.statfs();
        let a = vol.mkdir(INO_ROOT, "a", 0o755, 0, 0).unwrap().ino;
        let f = vol.create(INO_ROOT, "f", 0o644, 0, 0).unwrap().ino;
        vol.write(f, 0, &pattern(1, 9000)).unwrap();

        // Same directory, then across directories.
        vol.rename(INO_ROOT, "f", INO_ROOT, "g").unwrap();
        assert!(matches!(
            vol.lookup(INO_ROOT, "f"),
            Err(DadaError::NotFound)
        ));
        assert_eq!(ino_of(&mut vol, INO_ROOT, "g"), f);
        vol.rename(INO_ROOT, "g", a, "h").unwrap();
        assert_eq!(ino_of(&mut vol, a, "h"), f);
        assert_eq!(read_all(&mut vol, f), pattern(1, 9000));

        // Replacing an existing file releases it.
        let other = vol.create(a, "other", 0o644, 0, 0).unwrap().ino;
        vol.write(other, 0, &pattern(2, 9000)).unwrap();
        vol.rename(a, "h", a, "other").unwrap();
        assert_eq!(ino_of(&mut vol, a, "other"), f);
        assert!(matches!(vol.getattr(other), Err(DadaError::NotFound)));
        assert_eq!(names(&vol.readdir(a, 0).unwrap()), [".", "..", "other"]);

        // Renaming onto a hard link of the same inode does nothing.
        vol.link(f, INO_ROOT, "hard").unwrap();
        vol.rename(INO_ROOT, "hard", a, "other").unwrap();
        assert_eq!(vol.getattr(f).unwrap().links, 2);
        assert_eq!(ino_of(&mut vol, INO_ROOT, "hard"), f);

        // A replaced file that still has another link survives.
        let x = vol.create(INO_ROOT, "x", 0o644, 0, 0).unwrap().ino;
        vol.rename(INO_ROOT, "x", INO_ROOT, "hard").unwrap();
        assert_eq!(vol.getattr(f).unwrap().links, 1);
        assert_eq!(ino_of(&mut vol, INO_ROOT, "hard"), x);

        vol.unlink(INO_ROOT, "hard").unwrap();
        vol.unlink(a, "other").unwrap();
        vol.rmdir(INO_ROOT, "a").unwrap();
        assert_eq!(vol.statfs(), base);
        check_counters(&vol);
    }

    #[test]
    fn rename_directories() {
        let mut vol = rw(4096, 8 * MIB);
        let a = vol.mkdir(INO_ROOT, "a", 0o755, 0, 0).unwrap().ino;
        let b = vol.mkdir(INO_ROOT, "b", 0o755, 0, 0).unwrap().ino;
        let sub = vol.mkdir(a, "sub", 0o755, 0, 0).unwrap().ino;
        vol.create(sub, "file", 0o644, 0, 0).unwrap();
        assert_eq!(vol.getattr(INO_ROOT).unwrap().links, 4);

        // Moving a directory updates `..` and both parents' link counts.
        vol.rename(a, "sub", b, "moved").unwrap();
        assert_eq!(ino_of(&mut vol, sub, ".."), b);
        assert_eq!(vol.getattr(a).unwrap().links, 2);
        assert_eq!(vol.getattr(b).unwrap().links, 3);
        assert!(vol.lookup(sub, "file").is_ok());

        // Cycles are refused.
        assert!(matches!(
            vol.rename(INO_ROOT, "b", sub, "x"),
            Err(DadaError::Invalid)
        ));
        assert!(matches!(
            vol.rename(INO_ROOT, "b", b, "x"),
            Err(DadaError::Invalid)
        ));

        // Type mismatches and non-empty targets.
        let f = vol.create(INO_ROOT, "f", 0o644, 0, 0).unwrap().ino;
        assert!(matches!(
            vol.rename(INO_ROOT, "a", INO_ROOT, "f"),
            Err(DadaError::NotDir)
        ));
        assert!(matches!(
            vol.rename(INO_ROOT, "f", INO_ROOT, "a"),
            Err(DadaError::IsDir)
        ));
        assert!(matches!(
            vol.rename(INO_ROOT, "a", INO_ROOT, "b"),
            Err(DadaError::NotEmpty)
        ));
        assert!(matches!(
            vol.rename(INO_ROOT, ".", INO_ROOT, "z"),
            Err(DadaError::Invalid)
        ));
        assert!(matches!(
            vol.rename(INO_ROOT, "f", INO_ROOT, ".."),
            Err(DadaError::InvalidName)
        ));
        assert!(matches!(
            vol.rename(INO_ROOT, "nope", INO_ROOT, "z"),
            Err(DadaError::NotFound)
        ));

        // Replacing an empty directory.
        vol.rename(b, "moved", INO_ROOT, "a").unwrap();
        assert!(matches!(vol.getattr(a), Err(DadaError::NotFound)));
        assert_eq!(ino_of(&mut vol, INO_ROOT, "a"), sub);
        assert_eq!(ino_of(&mut vol, sub, ".."), INO_ROOT);
        assert_eq!(vol.getattr(b).unwrap().links, 2);
        assert_eq!(vol.getattr(INO_ROOT).unwrap().links, 4);
        let _ = f;
        check_counters(&vol);
    }

    fn casefold_volume() -> Volume<MemDevice> {
        let mut dev = MemDevice::new(4096, 2048).unwrap();
        let opts = FormatOptions {
            casefold: true,
            journal: false,
            ..FormatOptions::default()
        };
        format(&mut dev, &opts).unwrap();
        Volume::open(dev, false).unwrap()
    }

    #[test]
    fn casefold_lookup_preserves_case() {
        let mut vol = casefold_volume();
        let f = vol.create(INO_ROOT, "Readme.TXT", 0o644, 0, 0).unwrap().ino;
        assert_eq!(ino_of(&mut vol, INO_ROOT, "README.txt"), f);
        assert_eq!(ino_of(&mut vol, INO_ROOT, "readme.txt"), f);
        assert!(matches!(
            vol.create(INO_ROOT, "README.txt", 0o644, 0, 0),
            Err(DadaError::Exists)
        ));
        assert_eq!(
            names(&vol.readdir(INO_ROOT, 0).unwrap()),
            [".", "..", "Readme.TXT"]
        );

        // Changing only the case renames the entry in place.
        vol.rename(INO_ROOT, "readme.txt", INO_ROOT, "README.TXT")
            .unwrap();
        assert_eq!(
            names(&vol.readdir(INO_ROOT, 0).unwrap()),
            [".", "..", "README.TXT"]
        );
        assert_eq!(ino_of(&mut vol, INO_ROOT, "Readme.txt"), f);
        vol.unlink(INO_ROOT, "readme.TXT").unwrap();
        assert_eq!(names(&vol.readdir(INO_ROOT, 0).unwrap()), [".", ".."]);
    }

    #[test]
    fn names_are_stored_in_nfc() {
        let mut vol = rw(4096, 8 * MIB);
        let decomposed = "cafe\u{301}";
        let f = vol.create(INO_ROOT, decomposed, 0o644, 0, 0).unwrap().ino;
        assert_eq!(names(&vol.readdir(INO_ROOT, 0).unwrap())[2], "caf\u{e9}");
        assert_eq!(ino_of(&mut vol, INO_ROOT, "caf\u{e9}"), f);
        assert_eq!(ino_of(&mut vol, INO_ROOT, decomposed), f);
        // Without CASEFOLD, case matters.
        assert!(matches!(
            vol.lookup(INO_ROOT, "CAFÉ"),
            Err(DadaError::NotFound)
        ));
        vol.create(INO_ROOT, "CAFÉ", 0o644, 0, 0).unwrap();
        assert!(matches!(
            vol.create(INO_ROOT, decomposed, 0o644, 0, 0),
            Err(DadaError::Exists)
        ));
    }
}
