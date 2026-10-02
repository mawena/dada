//! Checking and repairing dada volumes.
//!
//! The check works on the raw device: it reads the superblocks, the whole
//! inode table, then walks the tree from the root. Bitmaps, free counters
//! and link counts are rebuilt from what is actually referenced and
//! compared with what is on disk. Inodes that no directory references are
//! attached to `/lost+found`.

pub mod cli;

use std::collections::{BTreeSet, HashMap, HashSet};

use libdada::bitmap::Bitmap;
use libdada::dir::{DirBlock, Slot};
use libdada::extent::{map_block, Extent, ExtentBlock};
use libdada::format::{
    extent_block_capacity, FIRST_USER_INO, INCOMPAT_CASEFOLD, INCOMPAT_JOURNAL,
    INODE_INLINE_EXTENTS, INODE_SIZE, INO_JOURNAL, INO_ROOT, RESERVED_INODES, STATE_CLEAN,
};
use libdada::inode::{FileKind, Inode, InodeData};
use libdada::journal::{self, Overlay};
use libdada::name::fold;
use libdada::superblock::Superblock;
use libdada::{BlockDevice, DadaError, Volume};

pub const EXIT_CLEAN: i32 = 0;
pub const EXIT_FIXED: i32 = 1;
pub const EXIT_UNFIXED: i32 = 4;
pub const EXIT_ERROR: i32 = 8;

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Fix what can be fixed; otherwise the device is only read.
    pub repair: bool,
}

/// Outcome of a check.
#[derive(Debug, Default)]
pub struct Report {
    /// Problems found.
    pub found: usize,
    /// Problems fixed (only with `repair`).
    pub fixed: usize,
    /// One line per problem.
    pub problems: Vec<String>,
    /// Informational notes.
    pub notes: Vec<String>,
}

impl Report {
    pub fn exit_code(&self) -> i32 {
        if self.found == 0 {
            EXIT_CLEAN
        } else if self.fixed >= self.found {
            EXIT_FIXED
        } else {
            EXIT_UNFIXED
        }
    }
}

/// Checks the volume on `dev` and repairs it if asked. Returns the report
/// and the device. An `Err` is a runtime error (I/O, unsupported feature).
pub fn check<D: BlockDevice>(mut dev: D, opts: Options) -> Result<(Report, D), DadaError> {
    let mut report = Report::default();
    let Some(mut sb) = load_superblock(&mut dev, opts, &mut report)? else {
        return Ok((report, dev));
    };
    let mut pending = None;
    if sb.features_incompat & INCOMPAT_JOURNAL != 0 {
        match journal::scan(&mut dev, &sb) {
            Ok(replay) if sb.state != STATE_CLEAN && replay.transactions > 0 => {
                report.notes.push(format!(
                    "journal: {} committed transactions replayed",
                    replay.transactions
                ));
                if opts.repair {
                    sb = journal::apply(&mut dev, &sb, &replay)?;
                } else {
                    pending = Some(replay.latest());
                }
            }
            Ok(_) => {}
            Err(DadaError::Io(e)) => return Err(DadaError::Io(e)),
            Err(e) => {
                if problem(&mut report, opts, format!("{e}; resetting the journal")) {
                    journal::initialize(&mut dev, &sb)?;
                    dev.flush()?;
                }
            }
        }
    }
    match pending {
        // Checking without repairing: look at the volume as the replay
        // would leave it, without writing anything.
        Some(blocks) => {
            let mut overlay = Overlay::new(dev, blocks);
            let sb = read_sb(&mut overlay, 0)?;
            let (report, overlay) = run_checker(overlay, sb, opts, report)?;
            Ok((report, overlay.into_inner()))
        }
        None => run_checker(dev, sb, opts, report),
    }
}

fn run_checker<D: BlockDevice>(
    dev: D,
    sb: Superblock,
    opts: Options,
    report: Report,
) -> Result<(Report, D), DadaError> {
    let mut checker = Checker::new(dev, sb, opts, report)?;
    checker.run()?;
    checker.finish()
}

fn read_sb<D: BlockDevice>(dev: &mut D, lba: u64) -> Result<Superblock, DadaError> {
    let mut buf = vec![0u8; dev.block_size() as usize];
    dev.read_block(lba, &mut buf)?;
    let sb = Superblock::decode(&buf)?;
    sb.validate()?;
    Ok(sb)
}

fn superblock_block(sb: &Superblock) -> Vec<u8> {
    let mut block = vec![0u8; sb.block_size as usize];
    block[..sb.encode().len()].copy_from_slice(&sb.encode());
    block
}

/// Fields of the superblock that never change after formatting.
fn same_geometry(a: &Superblock, b: &Superblock) -> bool {
    (
        a.block_size,
        a.total_blocks,
        a.inode_count,
        a.block_bitmap_start,
        a.inode_bitmap_start,
        a.inode_table_start,
        a.data_start,
        a.journal_start,
        a.journal_blocks,
        a.uuid,
    ) == (
        b.block_size,
        b.total_blocks,
        b.inode_count,
        b.block_bitmap_start,
        b.inode_bitmap_start,
        b.inode_table_start,
        b.data_start,
        b.journal_start,
        b.journal_blocks,
        b.uuid,
    )
}

fn problem(report: &mut Report, opts: Options, msg: String) -> bool {
    report.found += 1;
    if opts.repair {
        report.fixed += 1;
        report.problems.push(format!("{msg} (fixed)"));
    } else {
        report.problems.push(msg);
    }
    opts.repair
}

fn unfixable(report: &mut Report, msg: String) {
    report.found += 1;
    report.problems.push(format!("{msg} (not fixable)"));
}

/// Primary superblock, restored from the backup if needed. `None` when no
/// usable superblock exists.
fn load_superblock<D: BlockDevice>(
    dev: &mut D,
    opts: Options,
    report: &mut Report,
) -> Result<Option<Superblock>, DadaError> {
    let primary = match read_sb(dev, 0) {
        Ok(sb) => sb,
        Err(e @ DadaError::Unsupported(_)) | Err(e @ DadaError::Io(_)) => return Err(e),
        Err(primary_err) => {
            let backup = dev
                .block_count()
                .checked_sub(1)
                .filter(|&b| b > 0)
                .and_then(|b| read_sb(dev, b).ok())
                .filter(|sb| sb.block_size == dev.block_size());
            let Some(backup) = backup else {
                unfixable(report, format!("no valid superblock: {primary_err}"));
                return Ok(None);
            };
            if problem(
                report,
                opts,
                format!("primary superblock is invalid ({primary_err}); restoring the backup"),
            ) {
                dev.write_block(0, &superblock_block(&backup))?;
                dev.flush()?;
            }
            backup
        }
    };
    if primary.block_size != dev.block_size() {
        return Err(DadaError::Invalid);
    }
    if primary.total_blocks > dev.block_count() {
        unfixable(
            report,
            format!(
                "volume has {} blocks but the device only {}",
                primary.total_blocks,
                dev.block_count()
            ),
        );
        return Ok(None);
    }
    Ok(Some(primary))
}

/// What to do with a directory entry.
enum Verdict {
    Keep,
    Remove(String),
    Retype(FileKind, String),
}

struct Checker<D: BlockDevice> {
    dev: D,
    sb: Superblock,
    opts: Options,
    report: Report,
    bs: u64,
    /// Valid allocated inodes.
    inodes: HashMap<u64, Inode>,
    /// Blocks referenced by metadata zones and inodes.
    claimed: Bitmap,
    /// Inodes whose blocks are already claimed.
    claimed_inodes: HashSet<u64>,
    /// Inodes reached from the root or attached as orphans.
    reached: HashSet<u64>,
    /// Entries pointing at each non-directory inode.
    refs: HashMap<u64, u32>,
    /// Subdirectories of each directory.
    subdirs: HashMap<u64, u32>,
    /// Inodes to attach to /lost+found.
    orphans: Vec<u64>,
    /// Directories left without any block, to be given an empty one.
    blockless_dirs: Vec<(u64, u64)>,
    root_ok: bool,
    backup_ok: bool,
}

impl<D: BlockDevice> Checker<D> {
    fn new(mut dev: D, sb: Superblock, opts: Options, report: Report) -> Result<Self, DadaError> {
        let backup_ok = read_sb(&mut dev, sb.total_blocks - 1)
            .map(|b| same_geometry(&b, &sb))
            .unwrap_or(false);
        let bs = u64::from(sb.block_size);
        let zone = |bits: u64| (bits.div_ceil(bs * 8) * bs) as usize;
        let mut claimed = Bitmap::new(sb.total_blocks, zone(sb.total_blocks))?;
        claimed.set_range(0, sb.data_start, true)?;
        claimed.set(sb.total_blocks - 1, true)?;
        Ok(Checker {
            dev,
            opts,
            report,
            bs,
            inodes: HashMap::new(),
            claimed,
            claimed_inodes: HashSet::new(),
            reached: HashSet::new(),
            refs: HashMap::new(),
            subdirs: HashMap::new(),
            orphans: Vec::new(),
            blockless_dirs: Vec::new(),
            root_ok: true,
            backup_ok,
            sb,
        })
    }

    fn problem(&mut self, msg: String) -> bool {
        problem(&mut self.report, self.opts, msg)
    }

    fn read(&mut self, lba: u64) -> Result<Vec<u8>, DadaError> {
        let mut buf = vec![0u8; self.bs as usize];
        self.dev.read_block(lba, &mut buf)?;
        Ok(buf)
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), DadaError> {
        debug_assert!(self.opts.repair);
        self.dev.write_block(lba, data)
    }

    fn inode_location(&self, ino: u64) -> (u64, usize) {
        let byte = ino * u64::from(INODE_SIZE);
        (
            self.sb.inode_table_start + byte / self.bs,
            (byte % self.bs) as usize,
        )
    }

    fn write_inode(&mut self, ino: u64, inode: &Inode) -> Result<(), DadaError> {
        let (lba, offset) = self.inode_location(ino);
        let mut block = self.read(lba)?;
        block[offset..offset + INODE_SIZE as usize].copy_from_slice(&inode.encode(ino));
        self.write(lba, &block)?;
        self.inodes.insert(ino, inode.clone());
        Ok(())
    }

    fn clear_inode(&mut self, ino: u64) -> Result<(), DadaError> {
        let (lba, offset) = self.inode_location(ino);
        let mut block = self.read(lba)?;
        block[offset..offset + INODE_SIZE as usize].fill(0);
        self.write(lba, &block)?;
        self.inodes.remove(&ino);
        Ok(())
    }

    fn in_data_zone(&self, start: u64, len: u64) -> bool {
        start >= self.sb.data_start
            && start
                .checked_add(len)
                .is_some_and(|end| end < self.sb.total_blocks)
    }

    fn kind(&self, ino: u64) -> Option<FileKind> {
        self.inodes.get(&ino).and_then(|i| i.kind().ok())
    }

    fn run(&mut self) -> Result<(), DadaError> {
        if self.sb.state != STATE_CLEAN {
            self.report
                .notes
                .push("volume was not cleanly unmounted".into());
        }
        if !self.backup_ok {
            self.problem("backup superblock is invalid or differs from the primary".into());
        }
        self.scan_inodes()?;
        self.check_journal_inode()?;
        match self.kind(INO_ROOT) {
            Some(FileKind::Directory) => self.walk(INO_ROOT, Some(INO_ROOT))?,
            _ => {
                self.root_ok = false;
                if self.problem("root inode is not a valid directory".into()) {
                    // Rebuilt after every other block is claimed.
                    self.inodes.remove(&INO_ROOT);
                }
            }
        }
        self.find_orphans()?;
        self.give_blocks_to_empty_dirs()?;
        self.check_links()?;
        self.check_bitmaps_and_counters()
    }

    // -----------------------------------------------------------------------
    // Inodes
    // -----------------------------------------------------------------------

    fn scan_inodes(&mut self) -> Result<(), DadaError> {
        let per_block = self.bs / u64::from(INODE_SIZE);
        let table_blocks = self.sb.inode_count / per_block;
        for b in 0..table_blocks {
            let lba = self.sb.inode_table_start + b;
            let mut block = self.read(lba)?;
            let mut changed = false;
            for slot in 0..per_block {
                let ino = b * per_block + slot;
                if ino == 0 {
                    continue;
                }
                let range = (slot * u64::from(INODE_SIZE)) as usize
                    ..((slot + 1) * u64::from(INODE_SIZE)) as usize;
                match Inode::decode(ino, &block[range.clone()]) {
                    Ok(inode) if inode.is_free() => {}
                    Ok(inode) => {
                        if (3..RESERVED_INODES).contains(&ino) {
                            if self.problem(format!("reserved inode {ino} is in use")) {
                                block[range].fill(0);
                                changed = true;
                            }
                        } else {
                            self.inodes.insert(ino, inode);
                        }
                    }
                    Err(e) => {
                        if self.problem(format!("{e}")) {
                            block[range].fill(0);
                            changed = true;
                        }
                    }
                }
            }
            if changed {
                self.write(lba, &block)?;
            }
        }
        Ok(())
    }

    fn check_journal_inode(&mut self) -> Result<(), DadaError> {
        if self.sb.features_incompat & INCOMPAT_JOURNAL == 0 {
            if self.inodes.contains_key(&INO_JOURNAL)
                && self.problem("journal inode in use on a volume without journal".into())
            {
                self.clear_inode(INO_JOURNAL)?;
            }
            return Ok(());
        }
        let expected_extent = Extent {
            logical: 0,
            physical: self.sb.journal_start,
            length: self.sb.journal_blocks,
        };
        let size = self.sb.journal_blocks * self.bs;
        let ok = self.inodes.get(&INO_JOURNAL).is_some_and(|i| {
            i.kind().ok() == Some(FileKind::RegularFile)
                && i.links == 1
                && i.size == size
                && i.extent_block == 0
                && i.extents() == [expected_extent]
        });
        if !ok && self.problem("journal inode does not describe the journal".into()) {
            let journal = Inode {
                mode: FileKind::RegularFile.mode_bits() | 0o600,
                links: 1,
                size,
                data: InodeData::Extents(vec![expected_extent]),
                ..Inode::default()
            };
            self.write_inode(INO_JOURNAL, &journal)?;
        }
        if self.inodes.contains_key(&INO_JOURNAL) {
            self.reached.insert(INO_JOURNAL);
            self.claimed_inodes.insert(INO_JOURNAL);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Extents
    // -----------------------------------------------------------------------

    fn claim(&mut self, start: u64, len: u64, used: bool) -> Result<(), DadaError> {
        self.claimed.set_range(start, len, used)
    }

    fn range_free(&self, start: u64, len: u64) -> bool {
        (start..start + len).all(|b| !self.claimed.get(b).unwrap_or(true))
    }

    /// Writes `list` into `inode`, reusing the first blocks of `chain` and
    /// releasing the others.
    fn store_list(
        &mut self,
        ino: u64,
        inode: &mut Inode,
        list: &mut Vec<Extent>,
        mut chain: Vec<u64>,
    ) -> Result<Vec<u64>, DadaError> {
        let cap = extent_block_capacity(self.sb.block_size);
        let max = INODE_INLINE_EXTENTS + chain.len() * cap;
        if list.len() > max {
            for e in list.drain(max..) {
                self.claim(e.physical, e.length, false)?;
            }
        }
        let split = list.len().min(INODE_INLINE_EXTENTS);
        let chunks: Vec<Vec<Extent>> = list[split..].chunks(cap).map(<[Extent]>::to_vec).collect();
        while chain.len() > chunks.len() {
            if let Some(b) = chain.pop() {
                self.claim(b, 1, false)?;
            }
        }
        for (i, chunk) in chunks.into_iter().enumerate() {
            let block = ExtentBlock {
                next: chain.get(i + 1).copied().unwrap_or(0),
                owner: ino,
                extents: chunk,
            };
            self.write(chain[i], &block.encode(self.sb.block_size))?;
        }
        inode.data = InodeData::Extents(list[..split].to_vec());
        inode.extent_block = chain.first().copied().unwrap_or(0);
        Ok(chain)
    }

    /// Claims the blocks of `ino` the first time it is seen. Invalid or
    /// shared extents and broken extent block chains are dropped.
    fn claim_content(&mut self, ino: u64) -> Result<(Vec<Extent>, Vec<u64>), DadaError> {
        if !self.claimed_inodes.insert(ino) {
            return Ok((Vec::new(), Vec::new()));
        }
        let Some(mut inode) = self.inodes.get(&ino).cloned() else {
            return Ok((Vec::new(), Vec::new()));
        };
        if let InodeData::Inline(_) = inode.data {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut changed = false;
        let mut list = inode.extents().to_vec();
        let mut chain = Vec::new();
        let mut next = inode.extent_block;
        while next != 0 {
            let usable = self.in_data_zone(next, 1) && self.range_free(next, 1);
            let block = if usable {
                ExtentBlock::decode(&self.read(next)?)
                    .ok()
                    .filter(|b| b.owner == ino)
            } else {
                None
            };
            let Some(block) = block else {
                changed |=
                    self.problem(format!("inode {ino}: broken extent block chain at {next}"));
                break;
            };
            self.claim(next, 1, true)?;
            chain.push(next);
            list.extend(block.extents);
            next = block.next;
        }

        let mut kept = Vec::with_capacity(list.len());
        let mut next_logical = 0u64;
        for e in list {
            let valid = e.length > 0
                && e.logical >= next_logical
                && e.logical.checked_add(e.length).is_some()
                && self.in_data_zone(e.physical, e.length)
                && self.range_free(e.physical, e.length);
            if valid {
                self.claim(e.physical, e.length, true)?;
                next_logical = e.logical + e.length;
                kept.push(e);
            } else {
                changed |= self.problem(format!(
                    "inode {ino}: invalid or shared extent (logical {}, physical {}, length {})",
                    e.logical, e.physical, e.length
                ));
            }
        }
        if changed {
            chain = self.store_list(ino, &mut inode, &mut kept, chain)?;
            self.write_inode(ino, &inode)?;
        }
        Ok((kept, chain))
    }

    // -----------------------------------------------------------------------
    // Directories
    // -----------------------------------------------------------------------

    /// Walks the tree below `start`. `parent` is the expected target of
    /// `..` (`None` for an orphan, fixed when it is attached).
    fn walk(&mut self, start: u64, parent: Option<u64>) -> Result<(), DadaError> {
        self.reached.insert(start);
        let mut stack = vec![(start, parent)];
        while let Some((dir, parent)) = stack.pop() {
            for child in self.check_dir(dir, parent)? {
                stack.push((child, Some(dir)));
            }
        }
        Ok(())
    }

    /// Checks one directory; returns its subdirectories.
    fn check_dir(&mut self, dir: u64, parent: Option<u64>) -> Result<Vec<u64>, DadaError> {
        let (mut list, chain) = self.claim_content(dir)?;
        let Some(mut inode) = self.inodes.get(&dir).cloned() else {
            return Ok(Vec::new());
        };
        let bs = self.bs;
        let size_blocks = inode.size / bs;
        let mapped = (0..).take_while(|&i| map_block(&list, i).is_some()).count() as u64;
        let blocks = size_blocks.min(mapped);
        let beyond = list.iter().any(|e| e.logical + e.length > blocks);
        if (inode.size % bs != 0 || blocks != size_blocks || beyond)
            && self.problem(format!(
                "directory {dir}: size {} does not match its {mapped} mapped blocks",
                inode.size
            ))
        {
            let mut truncated: Vec<Extent> = Vec::new();
            for e in list.drain(..) {
                if e.logical >= blocks {
                    self.claim(e.physical, e.length, false)?;
                } else if e.logical + e.length > blocks {
                    let keep = blocks - e.logical;
                    self.claim(e.physical + keep, e.length - keep, false)?;
                    truncated.push(Extent { length: keep, ..e });
                } else {
                    truncated.push(e);
                }
            }
            list = truncated;
            self.store_list(dir, &mut inode, &mut list, chain)?;
            inode.size = blocks * bs;
            self.write_inode(dir, &inode)?;
        }
        if blocks == 0 {
            if self.opts.repair {
                self.blockless_dirs.push((dir, parent.unwrap_or(dir)));
            }
            return Ok(Vec::new());
        }

        let casefold = self.sb.features_incompat & INCOMPAT_CASEFOLD != 0;
        let mut names = HashSet::new();
        let mut children = Vec::new();
        for index in 0..blocks {
            let Some(lba) = map_block(&list, index) else {
                break;
            };
            let raw = self.read(lba)?;
            let mut changed = false;
            let mut block = match DirBlock::parse(&raw) {
                Ok(b) => b,
                Err(e) => {
                    if !self.problem(format!("directory {dir}, block {index}: {e}")) {
                        continue;
                    }
                    changed = true;
                    DirBlock::empty(self.sb.block_size)?
                }
            };
            if index == 0 {
                changed |= self.check_dots(dir, parent, &mut block)?;
            }
            let entries: Vec<Slot> = block.entries().cloned().collect();
            let skip = if index == 0 { 2 } else { 0 };
            for slot in entries.iter().skip(skip) {
                let key = if casefold {
                    fold(&slot.name)
                } else {
                    slot.name.clone()
                };
                let verdict = self.judge_entry(dir, slot, &mut names, key);
                let kind = match verdict {
                    Verdict::Keep => slot.kind,
                    Verdict::Remove(msg) => {
                        if self.problem(msg) {
                            block.remove(slot.offset)?;
                            changed = true;
                        }
                        continue;
                    }
                    Verdict::Retype(kind, msg) => {
                        if self.problem(msg) {
                            block.set_target(slot.offset, slot.ino, kind)?;
                            changed = true;
                        }
                        Some(kind)
                    }
                };
                if kind == Some(FileKind::Directory) {
                    self.reached.insert(slot.ino);
                    *self.subdirs.entry(dir).or_default() += 1;
                    children.push(slot.ino);
                } else {
                    self.reached.insert(slot.ino);
                    *self.refs.entry(slot.ino).or_default() += 1;
                    self.claim_content(slot.ino)?;
                }
            }
            if changed {
                self.write(lba, &block.encode())?;
            }
        }
        Ok(children)
    }

    /// `.` then `..` must open the first block. Returns whether the block changed.
    fn check_dots(
        &mut self,
        dir: u64,
        parent: Option<u64>,
        block: &mut DirBlock,
    ) -> Result<bool, DadaError> {
        let entries: Vec<Slot> = block.entries().cloned().collect();
        let dots_present = entries.len() >= 2
            && entries[0].name == "."
            && entries[0].offset == 0
            && entries[1].name == "..";
        if !dots_present {
            if !self.problem(format!("directory {dir}: missing . or .. entries")) {
                return Ok(false);
            }
            let mut rebuilt = DirBlock::empty(self.sb.block_size)?;
            rebuilt.insert(dir, FileKind::Directory, ".")?;
            rebuilt.insert(parent.unwrap_or(dir), FileKind::Directory, "..")?;
            for slot in entries.iter().filter(|s| s.name != "." && s.name != "..") {
                let Some(kind) = slot.kind else { continue };
                if !rebuilt.insert(slot.ino, kind, &slot.name)? {
                    self.problem(format!("directory {dir}: entry {:?} dropped", slot.name));
                }
            }
            *block = rebuilt;
            return Ok(true);
        }
        let mut changed = false;
        if (entries[0].ino != dir || entries[0].kind != Some(FileKind::Directory))
            && self.problem(format!("directory {dir}: . points to {}", entries[0].ino))
        {
            block.set_target(entries[0].offset, dir, FileKind::Directory)?;
            changed = true;
        }
        if let Some(parent) = parent {
            if (entries[1].ino != parent || entries[1].kind != Some(FileKind::Directory))
                && self.problem(format!(
                    "directory {dir}: .. points to {} instead of {parent}",
                    entries[1].ino
                ))
            {
                block.set_target(entries[1].offset, parent, FileKind::Directory)?;
                changed = true;
            }
        }
        Ok(changed)
    }

    fn judge_entry(
        &self,
        dir: u64,
        slot: &Slot,
        names: &mut HashSet<String>,
        key: String,
    ) -> Verdict {
        let name = &slot.name;
        let ino = slot.ino;
        if name == "." || name == ".." {
            return Verdict::Remove(format!("directory {dir}: stray {name:?} entry"));
        }
        if !names.insert(key) {
            return Verdict::Remove(format!("directory {dir}: duplicate name {name:?}"));
        }
        if ino < FIRST_USER_INO || ino >= self.sb.inode_count {
            return Verdict::Remove(format!(
                "directory {dir}: entry {name:?} points to invalid inode {ino}"
            ));
        }
        let Some(kind) = self.kind(ino) else {
            return Verdict::Remove(format!(
                "directory {dir}: entry {name:?} points to free inode {ino}"
            ));
        };
        if kind == FileKind::Directory && self.reached.contains(&ino) {
            return Verdict::Remove(format!(
                "directory {dir}: entry {name:?} links directory {ino} a second time"
            ));
        }
        if slot.kind != Some(kind) {
            return Verdict::Retype(
                kind,
                format!("directory {dir}: entry {name:?} has the wrong type"),
            );
        }
        Verdict::Keep
    }

    // -----------------------------------------------------------------------
    // Orphans, links, bitmaps
    // -----------------------------------------------------------------------

    /// `..` of directory `dir` as stored in its first block.
    fn stored_parent(&mut self, dir: u64) -> Option<u64> {
        let inode = self.inodes.get(&dir)?;
        let lba = map_block(inode.extents(), 0)?;
        if !self.in_data_zone(lba, 1) {
            return None;
        }
        let raw = self.read(lba).ok()?;
        let block = DirBlock::parse(&raw).ok()?;
        let parent = block.entries().find(|s| s.name == "..").map(|s| s.ino);
        parent
    }

    fn find_orphans(&mut self) -> Result<(), DadaError> {
        loop {
            let unreached: BTreeSet<u64> = self
                .inodes
                .keys()
                .copied()
                .filter(|i| *i >= FIRST_USER_INO && !self.reached.contains(i))
                .collect();
            if unreached.is_empty() {
                return Ok(());
            }
            // Prefer the top of an orphaned subtree: a directory whose parent
            // is not itself an unreached directory.
            let dirs: Vec<u64> = unreached
                .iter()
                .copied()
                .filter(|&i| self.kind(i) == Some(FileKind::Directory))
                .collect();
            let mut pick = None;
            for &d in &dirs {
                let parent = self.stored_parent(d);
                let parent_unreached = parent.is_some_and(|p| {
                    p != d && unreached.contains(&p) && self.kind(p) == Some(FileKind::Directory)
                });
                if !parent_unreached {
                    pick = Some(d);
                    break;
                }
            }
            let pick = pick
                .or_else(|| dirs.first().copied())
                .or_else(|| unreached.first().copied())
                .unwrap_or_default();
            self.problem(format!("inode {pick} is not referenced by any directory"));
            self.orphans.push(pick);
            if self.kind(pick) == Some(FileKind::Directory) {
                self.walk(pick, None)?;
            } else {
                self.reached.insert(pick);
                self.claim_content(pick)?;
            }
        }
    }

    fn alloc_block(&mut self) -> Result<u64, DadaError> {
        let (b, _) = self
            .claimed
            .find_run(
                self.sb.data_start,
                self.sb.data_start,
                self.sb.total_blocks - 1,
                1,
            )
            .ok_or(DadaError::NoSpace)?;
        self.claim(b, 1, true)?;
        Ok(b)
    }

    /// Gives an empty block to the root if it was lost and to directories
    /// left without blocks.
    fn give_blocks_to_empty_dirs(&mut self) -> Result<(), DadaError> {
        if !self.opts.repair {
            return Ok(());
        }
        let mut todo = std::mem::take(&mut self.blockless_dirs);
        if !self.root_ok {
            todo.push((INO_ROOT, INO_ROOT));
        }
        for (dir, parent) in todo {
            let lba = self.alloc_block()?;
            let mut block = DirBlock::empty(self.sb.block_size)?;
            block.insert(dir, FileKind::Directory, ".")?;
            block.insert(parent, FileKind::Directory, "..")?;
            self.write(lba, &block.encode())?;
            let mut inode = self.inodes.get(&dir).cloned().unwrap_or(Inode {
                mode: FileKind::Directory.mode_bits() | 0o755,
                links: 2,
                ..Inode::default()
            });
            inode.size = self.bs;
            inode.extent_block = 0;
            inode.data = InodeData::Extents(vec![Extent {
                logical: 0,
                physical: lba,
                length: 1,
            }]);
            self.write_inode(dir, &inode)?;
            self.reached.insert(dir);
        }
        Ok(())
    }

    fn check_links(&mut self) -> Result<(), DadaError> {
        let mut inos: Vec<u64> = self.reached.iter().copied().collect();
        inos.sort_unstable();
        for ino in inos {
            let Some(mut inode) = self.inodes.get(&ino).cloned() else {
                continue;
            };
            let expected = match inode.kind() {
                _ if ino == INO_JOURNAL => 1,
                Ok(FileKind::Directory) => 2 + self.subdirs.get(&ino).copied().unwrap_or(0),
                _ => {
                    self.refs.get(&ino).copied().unwrap_or(0)
                        + u32::from(self.orphans.contains(&ino))
                }
            };
            if inode.links != expected
                && self.problem(format!(
                    "inode {ino}: link count is {} instead of {expected}",
                    inode.links
                ))
            {
                inode.links = expected;
                self.write_inode(ino, &inode)?;
            }
        }
        Ok(())
    }

    fn read_zone(&mut self, start: u64, bits: u64) -> Result<Vec<u8>, DadaError> {
        let blocks = bits.div_ceil(self.bs * 8);
        let mut bytes = Vec::with_capacity((blocks * self.bs) as usize);
        for i in 0..blocks {
            bytes.extend(self.read(start + i)?);
        }
        Ok(bytes)
    }

    fn compare_bitmap(
        &mut self,
        what: &str,
        start: u64,
        expected: &Bitmap,
    ) -> Result<(), DadaError> {
        let on_disk = self.read_zone(start, expected.len())?;
        let wrong: u32 = on_disk
            .iter()
            .zip(expected.as_bytes())
            .map(|(a, b)| (a ^ b).count_ones())
            .sum();
        if wrong > 0 && self.problem(format!("{what} bitmap: {wrong} bits wrong")) {
            for (i, chunk) in expected.as_bytes().chunks(self.bs as usize).enumerate() {
                self.write(start + i as u64, chunk)?;
            }
        }
        Ok(())
    }

    fn check_bitmaps_and_counters(&mut self) -> Result<(), DadaError> {
        let zone = |bits: u64| (bits.div_ceil(self.bs * 8) * self.bs) as usize;
        let mut inodes = Bitmap::new(self.sb.inode_count, zone(self.sb.inode_count))?;
        inodes.set_range(0, RESERVED_INODES, true)?;
        for &ino in &self.reached {
            if ino < self.sb.inode_count && self.inodes.contains_key(&ino) {
                inodes.set(ino, true)?;
            }
        }
        let blocks = self.claimed.clone();
        self.compare_bitmap("block", self.sb.block_bitmap_start, &blocks)?;
        self.compare_bitmap("inode", self.sb.inode_bitmap_start, &inodes)?;

        let free_blocks = self.sb.total_blocks - blocks.count_used();
        let free_inodes = self.sb.inode_count - inodes.count_used();
        if self.sb.free_blocks != free_blocks
            && self.problem(format!(
                "free block count is {} instead of {free_blocks}",
                self.sb.free_blocks
            ))
        {
            self.sb.free_blocks = free_blocks;
        }
        if self.sb.free_inodes != free_inodes
            && self.problem(format!(
                "free inode count is {} instead of {free_inodes}",
                self.sb.free_inodes
            ))
        {
            self.sb.free_inodes = free_inodes;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(Report, D), DadaError> {
        if self.opts.repair && (self.report.found > 0 || self.sb.state != STATE_CLEAN) {
            self.sb.state = STATE_CLEAN;
            let block = superblock_block(&self.sb);
            self.dev.write_block(self.sb.total_blocks - 1, &block)?;
            self.dev.flush()?;
            self.dev.write_block(0, &block)?;
            self.dev.flush()?;
        }
        let mut dev = self.dev;
        if self.opts.repair && !self.orphans.is_empty() {
            let mut vol = Volume::open(dev, false)?;
            let lost_found = lost_and_found(&mut vol)?;
            for &ino in &self.orphans {
                let mut name = format!("#{ino}");
                let mut n = 1;
                loop {
                    match vol.attach_orphan(ino, lost_found, &name) {
                        Ok(()) => break,
                        Err(DadaError::Exists) => {
                            n += 1;
                            name = format!("#{ino}.{n}");
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            dev = vol.close()?;
        }
        Ok((self.report, dev))
    }
}

/// `/lost+found`, created if needed (or `/lost+found.N` if the name is taken
/// by something else).
fn lost_and_found<D: BlockDevice>(vol: &mut Volume<D>) -> Result<u64, DadaError> {
    let root = vol.root();
    let mut name = String::from("lost+found");
    for n in 1.. {
        match vol.lookup(root, &name) {
            Ok(attr) if attr.kind == FileKind::Directory => return Ok(attr.ino),
            Ok(_) => name = format!("lost+found.{n}"),
            Err(DadaError::NotFound) => return Ok(vol.mkdir(root, &name, 0o700, 0, 0)?.ino),
            Err(e) => return Err(e),
        }
    }
    Err(DadaError::Exists)
}
