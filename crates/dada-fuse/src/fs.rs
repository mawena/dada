//! `fuser::Filesystem` on top of a dada `Volume`.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
    WriteFlags,
};
use libdada::{Attr, BlockDevice, DadaError, FileKind, Ino, SetAttr, Volume};
use log::{error, info, warn};

/// Hidden directory at the root holding files unlinked while still open.
pub const UNLINKED_DIR: &str = ".dada-unlinked";

const TTL: Duration = Duration::from_secs(1);
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;

/// How the volume is presented.
#[derive(Debug, Clone, Copy, Default)]
pub struct Presentation {
    /// Show every file as owned by this user and group ("USB key" mode).
    pub owner: Option<(u32, u32)>,
}

struct State<D: BlockDevice> {
    vol: Option<Volume<D>>,
    /// Open handles per inode.
    open: HashMap<Ino, u32>,
    /// Inodes unlinked while open, waiting in `UNLINKED_DIR`.
    unlinked: HashSet<Ino>,
}

pub struct DadaFs<D: BlockDevice> {
    state: Mutex<State<D>>,
    presentation: Presentation,
    block_size: u32,
}

fn errno(e: &DadaError) -> Errno {
    Errno::from_i32(e.to_errno())
}

/// Nanoseconds since the epoch to `SystemTime`.
pub fn to_system_time(ns: i64) -> SystemTime {
    let magnitude = Duration::from_nanos(ns.unsigned_abs());
    if ns >= 0 {
        UNIX_EPOCH + magnitude
    } else {
        UNIX_EPOCH - magnitude
    }
}

/// `SystemTime` to nanoseconds since the epoch (saturating).
pub fn to_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_nanos()).map_or(i64::MIN, |n| -n),
    }
}

fn file_type(kind: FileKind) -> FileType {
    match kind {
        FileKind::Directory => FileType::Directory,
        FileKind::RegularFile => FileType::RegularFile,
        FileKind::Symlink => FileType::Symlink,
    }
}

fn name_str(name: &OsStr) -> Result<&str, Errno> {
    // dada stores UTF-8 names only.
    name.to_str().ok_or(Errno::EINVAL)
}

impl<D: BlockDevice> DadaFs<D> {
    pub fn new(vol: Volume<D>, presentation: Presentation) -> Self {
        let block_size = vol.statfs().block_size;
        DadaFs {
            state: Mutex::new(State {
                vol: Some(vol),
                open: HashMap::new(),
                unlinked: HashSet::new(),
            }),
            presentation,
            block_size,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<D>> {
        // A panic while holding the lock leaves the state as it was; keep going.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn file_attr(&self, attr: &Attr, unlinked: bool) -> FileAttr {
        let (uid, gid) = self.presentation.owner.unwrap_or((attr.uid, attr.gid));
        FileAttr {
            ino: INodeNo(attr.ino),
            size: attr.size,
            blocks: attr.blocks,
            atime: to_system_time(attr.atime),
            mtime: to_system_time(attr.mtime),
            ctime: to_system_time(attr.ctime),
            crtime: to_system_time(attr.btime),
            kind: file_type(attr.kind),
            perm: attr.mode,
            nlink: if unlinked { 0 } else { attr.links },
            uid,
            gid,
            rdev: 0,
            blksize: self.block_size,
            flags: 0,
        }
    }

    /// Runs `op` on the volume, mapping errors to errno values.
    fn with<T>(
        &self,
        op: impl FnOnce(&mut Volume<D>, &mut State<D>) -> Result<T, DadaError>,
    ) -> Result<T, Errno> {
        let mut guard = self.lock();
        let state = &mut *guard;
        let Some(mut vol) = state.vol.take() else {
            return Err(Errno::EIO);
        };
        let result = op(&mut vol, state);
        state.vol = Some(vol);
        result.map_err(|e| {
            if matches!(e, DadaError::Corrupt(_) | DadaError::Io(_)) {
                error!("{e}");
            }
            errno(&e)
        })
    }

    fn is_hidden(parent: Ino, name: &str, root: Ino) -> bool {
        parent == root && name == UNLINKED_DIR
    }

    fn entry(&self, attr: &Attr, reply: ReplyEntry) {
        reply.entry(&TTL, &self.file_attr(attr, false), Generation(0));
    }
}

/// The hidden directory for open unlinked files, created when first needed.
fn unlinked_dir<D: BlockDevice>(vol: &mut Volume<D>) -> Result<Ino, DadaError> {
    let root = vol.root();
    match vol.lookup(root, UNLINKED_DIR) {
        Ok(attr) if attr.kind == FileKind::Directory => Ok(attr.ino),
        Ok(_) => Err(DadaError::Exists),
        Err(DadaError::NotFound) => Ok(vol.mkdir(root, UNLINKED_DIR, 0o700, 0, 0)?.ino),
        Err(e) => Err(e),
    }
}

/// Removes what a previous session left in the hidden directory.
pub fn purge_unlinked<D: BlockDevice>(vol: &mut Volume<D>) -> Result<usize, DadaError> {
    let root = vol.root();
    let dir = match vol.lookup(root, UNLINKED_DIR) {
        Ok(attr) if attr.kind == FileKind::Directory => attr.ino,
        _ => return Ok(0),
    };
    let mut removed = 0;
    for (_, entry) in vol.readdir(dir, 0)? {
        if entry.kind != FileKind::Directory {
            vol.unlink(dir, &entry.name)?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Unlinks `parent/name`, or parks it in the hidden directory if it is the
/// last link of a file that is still open.
fn remove_or_park<D: BlockDevice>(
    vol: &mut Volume<D>,
    state: &mut State<D>,
    parent: Ino,
    name: &str,
) -> Result<(), DadaError> {
    let attr = vol.lookup(parent, name)?;
    let open = state.open.get(&attr.ino).copied().unwrap_or(0) > 0;
    if attr.kind != FileKind::Directory && attr.links == 1 && open {
        let hidden = unlinked_dir(vol)?;
        vol.rename(parent, name, hidden, &attr.ino.to_string())?;
        state.unlinked.insert(attr.ino);
        Ok(())
    } else {
        vol.unlink(parent, name)
    }
}

impl<D: BlockDevice + 'static> Filesystem for DadaFs<D> {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
        let mut state = self.lock();
        if let Some(vol) = state.vol.as_mut() {
            if !vol.is_read_only() {
                match purge_unlinked(vol) {
                    Ok(0) => {}
                    Ok(n) => info!("removed {n} files left unlinked by a previous session"),
                    Err(e) => warn!("cannot clean {UNLINKED_DIR}: {e}"),
                }
            }
        }
        Ok(())
    }

    fn destroy(&mut self) {
        let mut state = self.lock();
        if let Some(vol) = state.vol.take() {
            match vol.close() {
                Ok(_) => info!("volume closed cleanly"),
                Err(e) => error!("closing the volume failed: {e}"),
            }
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = name_str(name).and_then(|name| {
            self.with(|vol, _| {
                if Self::is_hidden(parent.0, name, vol.root()) {
                    return Err(DadaError::NotFound);
                }
                vol.lookup(parent.0, name)
            })
        });
        match result {
            Ok(attr) => self.entry(&attr, reply),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.with(|vol, state| Ok((vol.getattr(ino.0)?, state.unlinked.contains(&ino.0)))) {
            Ok((attr, unlinked)) => reply.attr(&TTL, &self.file_attr(&attr, unlinked)),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let time = |t: TimeOrNow| match t {
            TimeOrNow::SpecificTime(t) => to_ns(t),
            TimeOrNow::Now => to_ns(SystemTime::now()),
        };
        let changes = SetAttr {
            mode: mode.map(|m| (m & 0o7777) as u16),
            uid,
            gid,
            size,
            atime: atime.map(time),
            mtime: mtime.map(time),
            win_attrs: None,
        };
        match self.with(|vol, state| {
            Ok((
                vol.setattr(ino.0, &changes)?,
                state.unlinked.contains(&ino.0),
            ))
        }) {
            Ok((attr, unlinked)) => reply.attr(&TTL, &self.file_attr(&attr, unlinked)),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.with(|vol, _| vol.readlink(ino.0)) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(e) => reply.error(e),
        }
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // Only regular files exist on dada.
        if mode & S_IFMT != S_IFREG && mode & S_IFMT != 0 {
            reply.error(Errno::EPERM);
            return;
        }
        let perm = (mode & !umask & 0o7777) as u16;
        let result = name_str(name).and_then(|name| {
            self.with(|vol, _| vol.create(parent.0, name, perm, req.uid(), req.gid()))
        });
        match result {
            Ok(attr) => self.entry(&attr, reply),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let perm = (mode & !umask & 0o7777) as u16;
        let result = name_str(name).and_then(|name| {
            self.with(|vol, _| vol.mkdir(parent.0, name, perm, req.uid(), req.gid()))
        });
        match result {
            Ok(attr) => self.entry(&attr, reply),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = name_str(name).and_then(|name| {
            self.with(|vol, state| {
                if Self::is_hidden(parent.0, name, vol.root()) {
                    return Err(DadaError::NotFound);
                }
                remove_or_park(vol, state, parent.0, name)
            })
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = name_str(name).and_then(|name| {
            self.with(|vol, _| {
                if Self::is_hidden(parent.0, name, vol.root()) {
                    return Err(DadaError::NotFound);
                }
                vol.rmdir(parent.0, name)
            })
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let result = name_str(link_name).and_then(|name| {
            let target = target.to_str().ok_or(Errno::EINVAL)?;
            self.with(|vol, _| vol.symlink(parent.0, name, target, req.uid(), req.gid()))
        });
        match result {
            Ok(attr) => self.entry(&attr, reply),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if flags.contains(RenameFlags::RENAME_EXCHANGE)
            || flags.contains(RenameFlags::RENAME_WHITEOUT)
        {
            reply.error(Errno::EINVAL);
            return;
        }
        let result = name_str(name).and_then(|name| {
            let newname = name_str(newname)?;
            self.with(|vol, state| {
                let root = vol.root();
                if Self::is_hidden(parent.0, name, root)
                    || Self::is_hidden(newparent.0, newname, root)
                {
                    return Err(DadaError::Invalid);
                }
                match vol.lookup(newparent.0, newname) {
                    Ok(_) if flags.contains(RenameFlags::RENAME_NOREPLACE) => {
                        return Err(DadaError::Exists)
                    }
                    // Replacing the last link of an open file: park it first.
                    Ok(dst) if dst.kind != FileKind::Directory => {
                        let src = vol.lookup(parent.0, name)?;
                        if src.ino != dst.ino && src.kind != FileKind::Directory {
                            remove_or_park(vol, state, newparent.0, newname)?;
                        }
                    }
                    _ => {}
                }
                vol.rename(parent.0, name, newparent.0, newname)
            })
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let result = name_str(newname)
            .and_then(|name| self.with(|vol, _| vol.link(ino.0, newparent.0, name)));
        match result {
            Ok(attr) => self.entry(&attr, reply),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let result = self.with(|vol, state| {
            let attr = vol.getattr(ino.0)?;
            if attr.kind == FileKind::Directory {
                return Err(DadaError::IsDir);
            }
            *state.open.entry(ino.0).or_default() += 1;
            Ok(())
        });
        match result {
            Ok(()) => reply.opened(FileHandle(ino.0), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let result = self.with(|vol, _| {
            let mut buf = vec![0u8; size as usize];
            let n = vol.read(ino.0, offset, &mut buf)?;
            buf.truncate(n);
            Ok(buf)
        });
        match result {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.with(|vol, _| vol.write(ino.0, offset, data)) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.with(|vol, _| vol.sync()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let result = self.with(|vol, state| {
            let count = state.open.entry(ino.0).or_default();
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.open.remove(&ino.0);
                if state.unlinked.remove(&ino.0) {
                    let hidden = unlinked_dir(vol)?;
                    vol.unlink(hidden, &ino.0.to_string())?;
                }
            }
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.with(|vol, _| vol.sync()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let result = self.with(|vol, _| {
            let root = vol.root();
            let mut entries = vol.readdir(ino.0, offset)?;
            entries.retain(|(_, e)| !Self::is_hidden(ino.0, &e.name, root));
            Ok(entries)
        });
        match result {
            Ok(entries) => {
                for (cookie, entry) in entries {
                    if reply.add(
                        INodeNo(entry.ino),
                        cookie,
                        file_type(entry.kind),
                        &entry.name,
                    ) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.with(|vol, _| vol.sync()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.with(|vol, _| Ok(vol.statfs())) {
            Ok(st) => reply.statfs(
                st.total_blocks,
                st.free_blocks,
                st.free_blocks,
                st.total_inodes,
                st.free_inodes,
                st.block_size,
                st.max_name_len,
                st.block_size,
            ),
            Err(e) => reply.error(e),
        }
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        // Permissions are checked by the kernel (default_permissions).
        match self.with(|vol, _| vol.getattr(ino.0)) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let perm = (mode & !umask & 0o7777) as u16;
        let result = name_str(name).and_then(|name| {
            self.with(|vol, state| {
                let attr = vol.create(parent.0, name, perm, req.uid(), req.gid())?;
                *state.open.entry(attr.ino).or_default() += 1;
                Ok(attr)
            })
        });
        match result {
            Ok(attr) => reply.created(
                &TTL,
                &self.file_attr(&attr, false),
                Generation(0),
                FileHandle(attr.ino),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libdada::{format, FormatOptions, MemDevice};

    #[test]
    fn time_conversion() {
        for ns in [0, 1, -1, 1_700_000_000_123_456_789, -86_400_000_000_123] {
            assert_eq!(to_ns(to_system_time(ns)), ns);
        }
    }

    fn volume() -> Volume<MemDevice> {
        let mut dev = MemDevice::new(4096, 2048).unwrap();
        format(&mut dev, &FormatOptions::default()).unwrap();
        Volume::open(dev, false).unwrap()
    }

    #[test]
    fn open_files_are_parked_then_purged() {
        let mut vol = volume();
        let root = vol.root();
        let f = vol.create(root, "f", 0o644, 0, 0).unwrap();
        vol.write(f.ino, 0, b"still readable").unwrap();
        let mut state = State {
            vol: None,
            open: HashMap::from([(f.ino, 1)]),
            unlinked: HashSet::new(),
        };
        remove_or_park(&mut vol, &mut state, root, "f").unwrap();
        assert!(matches!(vol.lookup(root, "f"), Err(DadaError::NotFound)));
        assert!(state.unlinked.contains(&f.ino));
        let mut buf = [0u8; 14];
        vol.read(f.ino, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"still readable");

        // A file that is not open is removed at once.
        vol.create(root, "g", 0o644, 0, 0).unwrap();
        remove_or_park(&mut vol, &mut state, root, "g").unwrap();
        assert_eq!(state.unlinked.len(), 1);

        // Next mount: whatever is still parked is removed.
        assert_eq!(purge_unlinked(&mut vol).unwrap(), 1);
        assert!(matches!(vol.getattr(f.ino), Err(DadaError::NotFound)));
    }
}
