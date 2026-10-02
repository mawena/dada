//! Inodes (SPEC 4.5).

use crate::crc::{crc32c, crc32c_append};
use crate::extent::{validate_extents, Extent};
use crate::format::{
    ino, EXTENT_SIZE, FT_DIR, FT_REGULAR, FT_SYMLINK, INLINE_DATA_MAX, INODE_FLAGS_SUPPORTED,
    INODE_FLAG_INLINE_DATA, INODE_INLINE_EXTENTS, INODE_SIZE, MODE_PERM_MASK, MODE_TYPE_DIR,
    MODE_TYPE_REGULAR, MODE_TYPE_SHIFT, MODE_TYPE_SYMLINK,
};
use crate::le::{
    get_i64, get_u16, get_u32, get_u64, put_bytes, put_i64, put_u16, put_u32, put_u64,
};
use crate::DadaError;

const SIZE: usize = INODE_SIZE as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Directory,
    RegularFile,
    Symlink,
}

impl FileKind {
    /// Kind encoded in the type bits of an inode mode.
    pub fn from_mode(mode: u32) -> Result<Self, DadaError> {
        match mode >> MODE_TYPE_SHIFT {
            MODE_TYPE_DIR => Ok(FileKind::Directory),
            MODE_TYPE_REGULAR => Ok(FileKind::RegularFile),
            MODE_TYPE_SYMLINK => Ok(FileKind::Symlink),
            t => Err(DadaError::Corrupt(format!("unknown inode type {t:#x}"))),
        }
    }

    /// Type bits of the inode mode.
    pub fn mode_bits(self) -> u32 {
        let t = match self {
            FileKind::Directory => MODE_TYPE_DIR,
            FileKind::RegularFile => MODE_TYPE_REGULAR,
            FileKind::Symlink => MODE_TYPE_SYMLINK,
        };
        t << MODE_TYPE_SHIFT
    }

    /// Kind encoded in a directory entry `file_type`.
    pub fn from_dir_type(t: u8) -> Result<Self, DadaError> {
        match t {
            FT_DIR => Ok(FileKind::Directory),
            FT_REGULAR => Ok(FileKind::RegularFile),
            FT_SYMLINK => Ok(FileKind::Symlink),
            t => Err(DadaError::Corrupt(format!(
                "unknown directory entry type {t}"
            ))),
        }
    }

    pub fn dir_type(self) -> u8 {
        match self {
            FileKind::Directory => FT_DIR,
            FileKind::RegularFile => FT_REGULAR,
            FileKind::Symlink => FT_SYMLINK,
        }
    }
}

/// Content location of an inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InodeData {
    /// Up to `INODE_INLINE_EXTENTS` extents; more are chained from `extent_block`.
    Extents(Vec<Extent>),
    /// Content stored in the inode itself (`size` bytes, at most `INLINE_DATA_MAX`).
    Inline(Vec<u8>),
}

impl Default for InodeData {
    fn default() -> Self {
        InodeData::Extents(Vec::new())
    }
}

/// Decoded inode. A free inode has `links == 0`; only its `generation` is meaningful.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inode {
    /// Type bits and POSIX permissions.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub win_attrs: u32,
    pub size: u64,
    pub links: u32,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub btime: i64,
    pub data: InodeData,
    pub extent_block: u64,
    pub xattr_block: u64,
    pub generation: u32,
}

fn corrupt(n: u64, msg: &str) -> DadaError {
    DadaError::Corrupt(format!("inode {n}: {msg}"))
}

fn checksum(n: u64, bytes: &[u8]) -> u32 {
    crc32c_append(crc32c(&n.to_le_bytes()), bytes)
}

impl Inode {
    pub fn is_free(&self) -> bool {
        self.links == 0
    }

    pub fn kind(&self) -> Result<FileKind, DadaError> {
        FileKind::from_mode(self.mode)
    }

    pub fn permissions(&self) -> u32 {
        self.mode & MODE_PERM_MASK
    }

    /// Inline extents (empty for inline data).
    pub fn extents(&self) -> &[Extent] {
        match &self.data {
            InodeData::Extents(e) => e,
            InodeData::Inline(_) => &[],
        }
    }

    /// Encodes inode number `n`. Inline data longer than `INLINE_DATA_MAX`
    /// and extents beyond `INODE_INLINE_EXTENTS` are truncated; callers keep
    /// within those limits.
    pub fn encode(&self, n: u64) -> [u8; SIZE] {
        let mut b = [0u8; SIZE];
        put_u32(&mut b, ino::MODE, self.mode);
        put_u32(&mut b, ino::UID, self.uid);
        put_u32(&mut b, ino::GID, self.gid);
        put_u32(&mut b, ino::WIN_ATTRS, self.win_attrs);
        put_u64(&mut b, ino::SIZE, self.size);
        put_u32(&mut b, ino::LINKS, self.links);
        put_i64(&mut b, ino::ATIME, self.atime);
        put_i64(&mut b, ino::MTIME, self.mtime);
        put_i64(&mut b, ino::CTIME, self.ctime);
        put_i64(&mut b, ino::BTIME, self.btime);
        match &self.data {
            InodeData::Inline(data) => {
                put_u32(&mut b, ino::FLAGS, INODE_FLAG_INLINE_DATA);
                let data = data.get(..INLINE_DATA_MAX).unwrap_or(data);
                put_bytes(&mut b, ino::EXTENTS, data);
            }
            InodeData::Extents(extents) => {
                let extents = extents.get(..INODE_INLINE_EXTENTS).unwrap_or(extents);
                put_u16(&mut b, ino::EXTENT_COUNT, extents.len() as u16);
                for (i, e) in extents.iter().enumerate() {
                    put_bytes(&mut b, ino::EXTENTS + i * EXTENT_SIZE, &e.encode());
                }
            }
        }
        put_u64(&mut b, ino::EXTENT_BLOCK, self.extent_block);
        put_u64(&mut b, ino::XATTR_BLOCK, self.xattr_block);
        put_u32(&mut b, ino::GENERATION, self.generation);
        let crc = checksum(n, b.get(..ino::CHECKSUM).unwrap_or_default());
        put_u32(&mut b, ino::CHECKSUM, crc);
        b
    }

    /// Decodes inode number `n` from its 256-byte slot.
    ///
    /// An all-zero slot is a never-used free inode. Otherwise the checksum
    /// must match; a free inode (`links == 0`) is returned without further
    /// checks, an allocated one must have a valid type and content.
    pub fn decode(n: u64, buf: &[u8]) -> Result<Self, DadaError> {
        let b = buf.get(..SIZE).ok_or_else(|| corrupt(n, "truncated"))?;
        if b.iter().all(|&x| x == 0) {
            return Ok(Inode::default());
        }
        let stored = get_u32(b, ino::CHECKSUM)?;
        if stored != checksum(n, b.get(..ino::CHECKSUM).unwrap_or_default()) {
            return Err(corrupt(n, "bad checksum"));
        }

        let mut inode = Inode {
            mode: get_u32(b, ino::MODE)?,
            uid: get_u32(b, ino::UID)?,
            gid: get_u32(b, ino::GID)?,
            win_attrs: get_u32(b, ino::WIN_ATTRS)?,
            size: get_u64(b, ino::SIZE)?,
            links: get_u32(b, ino::LINKS)?,
            atime: get_i64(b, ino::ATIME)?,
            mtime: get_i64(b, ino::MTIME)?,
            ctime: get_i64(b, ino::CTIME)?,
            btime: get_i64(b, ino::BTIME)?,
            data: InodeData::default(),
            extent_block: get_u64(b, ino::EXTENT_BLOCK)?,
            xattr_block: get_u64(b, ino::XATTR_BLOCK)?,
            generation: get_u32(b, ino::GENERATION)?,
        };
        if inode.is_free() {
            return Ok(inode);
        }

        let kind = inode.kind().map_err(|_| corrupt(n, "unknown type"))?;
        let flags = get_u32(b, ino::FLAGS)?;
        if flags & !INODE_FLAGS_SUPPORTED != 0 {
            return Err(corrupt(n, "unknown flags"));
        }
        let extent_count = usize::from(get_u16(b, ino::EXTENT_COUNT)?);
        if flags & INODE_FLAG_INLINE_DATA != 0 {
            let len = usize::try_from(inode.size).unwrap_or(usize::MAX);
            if kind == FileKind::Directory {
                return Err(corrupt(n, "inline directory"));
            }
            if len > INLINE_DATA_MAX || extent_count != 0 || inode.extent_block != 0 {
                return Err(corrupt(n, "invalid inline data"));
            }
            let data = b
                .get(ino::EXTENTS..ino::EXTENTS + len)
                .ok_or_else(|| corrupt(n, "invalid inline data"))?;
            inode.data = InodeData::Inline(data.to_vec());
        } else {
            if extent_count > INODE_INLINE_EXTENTS {
                return Err(corrupt(n, "too many inline extents"));
            }
            let mut extents = Vec::with_capacity(extent_count);
            for i in 0..extent_count {
                extents.push(Extent::decode(
                    b.get(ino::EXTENTS + i * EXTENT_SIZE..).unwrap_or_default(),
                )?);
            }
            validate_extents(&extents).map_err(|e| corrupt(n, &e.to_string()))?;
            inode.data = InodeData::Extents(extents);
        }
        Ok(inode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn file(n_extents: u64) -> Inode {
        Inode {
            mode: FileKind::RegularFile.mode_bits() | 0o644,
            uid: 1000,
            gid: 100,
            win_attrs: 0x21,
            size: 3 * 4096,
            links: 1,
            atime: -5,
            mtime: 1,
            ctime: 2,
            btime: i64::MAX,
            data: InodeData::Extents(
                (0..n_extents)
                    .map(|i| Extent {
                        logical: i * 10,
                        physical: 1000 + i * 100,
                        length: 3,
                    })
                    .collect(),
            ),
            extent_block: 0,
            xattr_block: 0,
            generation: 7,
        }
    }

    fn reseal(n: u64, b: &mut [u8; SIZE]) {
        let crc = checksum(n, &b[..ino::CHECKSUM]);
        b[ino::CHECKSUM..].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn round_trip_extents_and_inline() {
        for n in 0..=4 {
            let inode = file(n);
            assert_eq!(Inode::decode(42, &inode.encode(42)).unwrap(), inode);
        }
        let link = Inode {
            mode: FileKind::Symlink.mode_bits() | 0o777,
            size: 11,
            links: 1,
            data: InodeData::Inline(b"/tmp/target".to_vec()),
            ..Inode::default()
        };
        let bytes = link.encode(16);
        assert_eq!(bytes[ino::FLAGS], 1);
        assert_eq!(&bytes[ino::EXTENTS..ino::EXTENTS + 11], b"/tmp/target");
        assert_eq!(Inode::decode(16, &bytes).unwrap(), link);
    }

    #[test]
    fn checksum_includes_inode_number() {
        let bytes = file(1).encode(20);
        assert!(Inode::decode(21, &bytes).is_err());
        let mut bytes = file(1).encode(20);
        bytes[ino::UID] ^= 1;
        assert!(Inode::decode(20, &bytes).is_err());
    }

    #[test]
    fn zero_slot_is_free() {
        let inode = Inode::decode(30, &[0; SIZE]).unwrap();
        assert!(inode.is_free());
        assert_eq!(inode.generation, 0);
    }

    #[test]
    fn free_inode_keeps_generation_without_type_check() {
        let freed = Inode {
            generation: 9,
            ..Inode::default()
        };
        let decoded = Inode::decode(17, &freed.encode(17)).unwrap();
        assert!(decoded.is_free());
        assert_eq!(decoded.generation, 9);
    }

    #[test]
    fn rejects_invalid_allocated_inodes() {
        let cases: [fn(&mut [u8; SIZE]); 8] = [
            |b| b[ino::MODE + 1] = 0x10,                           // type 0x1
            |b| b[ino::MODE + 2] = 0x01,                           // bits above the type
            |b| b[ino::FLAGS] = 0x02,                              // unknown flag
            |b| b[ino::EXTENT_COUNT] = 5,                          // too many extents
            |b| b[ino::EXTENTS + 16] = 0,                          // empty first extent
            |b| b[ino::EXTENTS + 24] = 1,                          // second extent overlaps first
            |b| b[ino::FLAGS] = 1,                                 // inline with extent_count != 0
            |b| b[ino::EXTENTS + 8..ino::EXTENTS + 16].fill(0xFF), // physical overflow
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut b = file(2).encode(50);
            f(&mut b);
            reseal(50, &mut b);
            assert!(Inode::decode(50, &b).is_err(), "case {i}");
        }

        // Inline directory, inline data too long.
        let dir = Inode {
            mode: FileKind::Directory.mode_bits() | 0o755,
            links: 2,
            size: 4,
            data: InodeData::Inline(vec![1; 4]),
            ..Inode::default()
        };
        assert!(Inode::decode(1, &dir.encode(1)).is_err());
        let mut long = file(0);
        long.data = InodeData::Inline(vec![1; 96]);
        long.size = 97;
        assert!(Inode::decode(16, &long.encode(16)).is_err());
    }

    #[test]
    fn kinds() {
        for kind in [
            FileKind::Directory,
            FileKind::RegularFile,
            FileKind::Symlink,
        ] {
            assert_eq!(
                FileKind::from_mode(kind.mode_bits() | 0o7777).unwrap(),
                kind
            );
            assert_eq!(FileKind::from_dir_type(kind.dir_type()).unwrap(), kind);
        }
        assert!(FileKind::from_mode(0).is_err());
        assert!(FileKind::from_dir_type(0).is_err());
        assert!(FileKind::from_dir_type(3).is_err());
    }

    proptest! {
        #[test]
        fn round_trip_any(
            n: u64,
            perm in 0u32..0o10000,
            kind_idx in 0usize..3,
            ids: [u32; 3],
            links in 1u32..,
            times: [i64; 4],
            inline in proptest::option::of(proptest::collection::vec(any::<u8>(), 0..=96)),
            starts in proptest::collection::vec((1u64..1000, 1u64..1 << 40, 1u64..1000), 0..=4),
            generation: u32,
        ) {
            let kind = [FileKind::Directory, FileKind::RegularFile, FileKind::Symlink][kind_idx];
            let (data, size) = match inline {
                Some(d) if kind != FileKind::Directory => {
                    let len = d.len() as u64;
                    (InodeData::Inline(d), len)
                }
                _ => {
                    let mut logical = 0;
                    let extents = starts.iter().map(|&(gap, physical, length)| {
                        let e = Extent { logical: logical + gap, physical, length };
                        logical += gap + length;
                        e
                    }).collect();
                    (InodeData::Extents(extents), logical * 4096)
                }
            };
            let inode = Inode {
                mode: kind.mode_bits() | perm,
                uid: ids[0], gid: ids[1], win_attrs: ids[2],
                size, links,
                atime: times[0], mtime: times[1], ctime: times[2], btime: times[3],
                data, extent_block: 0, xattr_block: 0, generation,
            };
            prop_assert_eq!(Inode::decode(n, &inode.encode(n)).unwrap(), inode);
        }

        #[test]
        fn decode_never_panics(n: u64, bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            let _ = Inode::decode(n, &bytes);
        }
    }
}
