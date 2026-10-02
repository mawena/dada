//! Directory blocks and entries (SPEC 4.8).

use crate::crc::crc32c;
use crate::format::{dirent, is_valid_block_size, DIR_BLOCK_TAIL, DIR_ENTRY_ALIGN, MAX_NAME_LEN};
use crate::inode::FileKind;
use crate::le::{get_u16, get_u32, get_u64, put_bytes, put_u16, put_u32, put_u64};
use crate::DadaError;

/// One entry of a directory block. `ino == 0` marks a free slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// Byte offset of the entry in its block.
    pub offset: usize,
    pub rec_len: usize,
    pub ino: u64,
    pub kind: Option<FileKind>,
    pub name: String,
}

impl Slot {
    /// Bytes this entry needs; a free slot needs none.
    fn min_len(&self) -> usize {
        if self.ino == 0 {
            0
        } else {
            entry_len(self.name.len())
        }
    }
}

/// Minimum length of an entry with a name of `name_len` bytes.
pub fn entry_len(name_len: usize) -> usize {
    (dirent::NAME + name_len).next_multiple_of(DIR_ENTRY_ALIGN)
}

/// A parsed directory block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirBlock {
    block_size: usize,
    slots: Vec<Slot>,
}

fn corrupt(msg: impl Into<String>) -> DadaError {
    DadaError::Corrupt(format!("directory block: {}", msg.into()))
}

/// Checks the structural rules of a stored name (normalization and casefold
/// are handled by the caller).
fn check_name(name: &str) -> Result<(), DadaError> {
    if name.len() > MAX_NAME_LEN {
        return Err(DadaError::NameTooLong);
    }
    if name.is_empty() || name.contains(['/', '\0']) {
        return Err(DadaError::InvalidName);
    }
    Ok(())
}

impl DirBlock {
    /// An empty block: one free slot covering the whole entry area.
    pub fn empty(block_size: u32) -> Result<Self, DadaError> {
        if !is_valid_block_size(block_size) {
            return Err(DadaError::Invalid);
        }
        let block_size = block_size as usize;
        Ok(DirBlock {
            block_size,
            slots: vec![Slot {
                offset: 0,
                rec_len: block_size - DIR_BLOCK_TAIL,
                ino: 0,
                kind: None,
                name: String::new(),
            }],
        })
    }

    /// Parses and validates a block read from disk.
    pub fn parse(buf: &[u8]) -> Result<Self, DadaError> {
        let block_size = buf.len();
        if !u32::try_from(block_size).is_ok_and(is_valid_block_size) {
            return Err(DadaError::Invalid);
        }
        let area = block_size - DIR_BLOCK_TAIL;
        let stored = get_u32(buf, area)?;
        if stored != crc32c(buf.get(..area).unwrap_or_default()) {
            return Err(corrupt("bad checksum"));
        }

        let mut slots = Vec::new();
        let mut offset = 0;
        while offset < area {
            let ino = get_u64(buf, offset + dirent::INODE)?;
            let rec_len = usize::from(get_u16(buf, offset + dirent::REC_LEN)?);
            let name_len = usize::from(*buf.get(offset + dirent::NAME_LEN).unwrap_or(&0));
            let file_type = *buf.get(offset + dirent::FILE_TYPE).unwrap_or(&0);
            let at = |msg: &str| corrupt(format!("entry at {offset}: {msg}"));

            if rec_len < entry_len(1)
                || !rec_len.is_multiple_of(DIR_ENTRY_ALIGN)
                || rec_len > area - offset
            {
                return Err(at("bad rec_len"));
            }
            let (kind, name) = if ino == 0 {
                (None, String::new())
            } else {
                if name_len == 0 || entry_len(name_len) > rec_len {
                    return Err(at("bad name_len"));
                }
                let kind = FileKind::from_dir_type(file_type).map_err(|_| at("bad file_type"))?;
                let start = offset + dirent::NAME;
                let raw = buf
                    .get(start..start + name_len)
                    .ok_or_else(|| at("truncated name"))?;
                let name = std::str::from_utf8(raw).map_err(|_| at("name is not UTF-8"))?;
                check_name(name).map_err(|_| at("invalid name"))?;
                (Some(kind), name.to_owned())
            };
            slots.push(Slot {
                offset,
                rec_len,
                ino,
                kind,
                name,
            });
            offset += rec_len;
        }
        Ok(DirBlock { block_size, slots })
    }

    /// Encodes the block with its checksum.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = vec![0u8; self.block_size];
        for s in &self.slots {
            put_u64(&mut buf, s.offset + dirent::INODE, s.ino);
            put_u16(&mut buf, s.offset + dirent::REC_LEN, s.rec_len as u16);
            if let Some(kind) = s.kind {
                put_bytes(&mut buf, s.offset + dirent::NAME_LEN, &[s.name.len() as u8]);
                put_bytes(&mut buf, s.offset + dirent::FILE_TYPE, &[kind.dir_type()]);
                put_bytes(&mut buf, s.offset + dirent::NAME, s.name.as_bytes());
            }
        }
        let area = self.block_size - DIR_BLOCK_TAIL;
        let crc = crc32c(buf.get(..area).unwrap_or_default());
        put_u32(&mut buf, area, crc);
        buf
    }

    /// All slots, free ones included, in block order.
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    /// Used entries, in block order.
    pub fn entries(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter().filter(|s| s.ino != 0)
    }

    pub fn is_empty(&self) -> bool {
        self.entries().next().is_none()
    }

    /// Inserts an entry in the first slot with enough room. Returns `false`
    /// when the block is full (the caller then adds a block).
    pub fn insert(&mut self, ino: u64, kind: FileKind, name: &str) -> Result<bool, DadaError> {
        check_name(name)?;
        if ino == 0 {
            return Err(DadaError::Invalid);
        }
        let need = entry_len(name.len());
        let Some(i) = self
            .slots
            .iter()
            .position(|s| s.rec_len - s.min_len() >= need)
        else {
            return Ok(false);
        };
        let new = |offset, rec_len| Slot {
            offset,
            rec_len,
            ino,
            kind: Some(kind),
            name: name.to_owned(),
        };
        let Some(slot) = self.slots.get_mut(i) else {
            return Ok(false);
        };
        if slot.ino == 0 {
            *slot = new(slot.offset, slot.rec_len);
        } else {
            let keep = slot.min_len();
            let split = new(slot.offset + keep, slot.rec_len - keep);
            slot.rec_len = keep;
            self.slots.insert(i + 1, split);
        }
        Ok(true)
    }

    /// Removes the used entry at byte `offset`: it is merged into the
    /// previous entry, or becomes a free slot if it is the first one.
    pub fn remove(&mut self, offset: usize) -> Result<(), DadaError> {
        let i = self
            .slots
            .iter()
            .position(|s| s.offset == offset && s.ino != 0)
            .ok_or(DadaError::NotFound)?;
        match i.checked_sub(1) {
            None => {
                if let Some(first) = self.slots.first_mut() {
                    first.ino = 0;
                    first.kind = None;
                    first.name.clear();
                }
            }
            Some(prev) => {
                let removed = self.slots.remove(i);
                if let Some(p) = self.slots.get_mut(prev) {
                    p.rec_len += removed.rec_len;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn rec_lens(b: &DirBlock) -> Vec<usize> {
        b.slots().iter().map(|s| s.rec_len).collect()
    }

    #[test]
    fn empty_block() {
        let b = DirBlock::empty(1024).unwrap();
        let bytes = b.encode();
        assert_eq!(bytes.len(), 1024);
        assert_eq!(&bytes[0..8], &[0; 8]);
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 1016);
        assert_eq!(DirBlock::parse(&bytes).unwrap(), b);
        assert!(b.is_empty());
    }

    #[test]
    fn dot_entries_layout() {
        let mut b = DirBlock::empty(4096).unwrap();
        assert!(b.insert(1, FileKind::Directory, ".").unwrap());
        assert!(b.insert(1, FileKind::Directory, "..").unwrap());
        assert_eq!(rec_lens(&b), [16, 4088 - 16]);
        let bytes = b.encode();
        assert_eq!(&bytes[10..13], &[1, 2, b'.']);
        assert_eq!(&bytes[16 + 10..16 + 14], &[2, 2, b'.', b'.']);
        let parsed = DirBlock::parse(&bytes).unwrap();
        let names: Vec<_> = parsed.entries().map(|s| s.name.as_str()).collect();
        assert_eq!(names, [".", ".."]);
    }

    #[test]
    fn insert_until_full_then_remove() {
        let mut b = DirBlock::empty(1024).unwrap();
        let mut n = 0;
        while b
            .insert(100 + n, FileKind::RegularFile, &format!("file-{n:04}"))
            .unwrap()
        {
            n += 1;
        }
        // 12 + 9 = 21 -> 24 bytes per entry, 1016 / 24 = 42.
        assert_eq!(n, 42);
        assert_eq!(rec_lens(&b).iter().sum::<usize>(), 1016);
        let b2 = DirBlock::parse(&b.encode()).unwrap();
        assert_eq!(b2, b);

        // Removing a middle entry merges it into the previous one.
        let off = b.slots()[5].offset;
        b.remove(off).unwrap();
        assert_eq!(b.slots()[4].rec_len, 48);
        // The freed room is reused.
        assert!(b.insert(999, FileKind::Symlink, "link").unwrap());
        assert_eq!(b.slots()[5].name, "link");

        // Removing the first entry leaves a free slot in place.
        b.remove(0).unwrap();
        assert_eq!(b.slots()[0].ino, 0);
        assert_eq!(b.slots()[0].rec_len, 24);
        assert!(b.insert(5, FileKind::Directory, "dir").unwrap());
        assert_eq!(b.slots()[0].name, "dir");
        assert!(b.remove(3).is_err());
    }

    #[test]
    fn rejects_bad_names() {
        let mut b = DirBlock::empty(1024).unwrap();
        assert!(matches!(
            b.insert(5, FileKind::RegularFile, ""),
            Err(DadaError::InvalidName)
        ));
        assert!(matches!(
            b.insert(5, FileKind::RegularFile, "a/b"),
            Err(DadaError::InvalidName)
        ));
        assert!(matches!(
            b.insert(5, FileKind::RegularFile, "a\0"),
            Err(DadaError::InvalidName)
        ));
        assert!(matches!(
            b.insert(5, FileKind::RegularFile, &"x".repeat(256)),
            Err(DadaError::NameTooLong)
        ));
        assert!(b
            .insert(5, FileKind::RegularFile, &"é".repeat(127))
            .unwrap());
        assert!(b.insert(0, FileKind::RegularFile, "zero").is_err());
    }

    fn sealed(mut bytes: Vec<u8>) -> Vec<u8> {
        let area = bytes.len() - 8;
        let crc = crc32c(&bytes[..area]);
        bytes[area..area + 4].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    #[test]
    fn rejects_corrupt_blocks() {
        let mut b = DirBlock::empty(1024).unwrap();
        b.insert(1, FileKind::Directory, ".").unwrap();
        b.insert(1, FileKind::Directory, "..").unwrap();
        let good = b.encode();

        let mut bad = good.clone();
        bad[0] ^= 1;
        assert!(DirBlock::parse(&bad).is_err(), "checksum");

        let cases: [fn(&mut Vec<u8>); 7] = [
            |v| v[8] = 15,        // rec_len too small
            |v| v[8] = 17,        // rec_len not aligned
            |v| v[16 + 9] = 0x10, // rec_len past the end
            |v| v[10] = 0,        // empty name
            |v| v[10] = 5,        // name longer than rec_len allows
            |v| v[11] = 3,        // unknown file type
            |v| v[12] = b'/',     // invalid character
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut v = good.clone();
            f(&mut v);
            assert!(DirBlock::parse(&sealed(v)).is_err(), "case {i}");
        }
        let mut v = good.clone();
        v[12] = 0xFF;
        assert!(DirBlock::parse(&sealed(v)).is_err(), "invalid UTF-8");
        assert!(DirBlock::parse(&good[..1000]).is_err(), "bad size");
    }

    proptest! {
        #[test]
        fn parse_never_panics(bytes in proptest::collection::vec(any::<u8>(), 1024)) {
            let _ = DirBlock::parse(&bytes);
            let _ = DirBlock::parse(&sealed(bytes));
        }

        #[test]
        fn random_operations_keep_block_valid(
            ops in proptest::collection::vec((any::<bool>(), "[a-zA-Z0-9é]{1,40}"), 1..200)
        ) {
            let mut b = DirBlock::empty(1024).unwrap();
            let mut next_ino = 16;
            for (insert, name) in ops {
                if insert || b.is_empty() {
                    let _ = b.insert(next_ino, FileKind::RegularFile, &name).unwrap();
                    next_ino += 1;
                } else {
                    let victim = b.entries().nth(name.len() % b.entries().count()).unwrap().offset;
                    b.remove(victim).unwrap();
                }
                prop_assert_eq!(rec_lens(&b).iter().sum::<usize>(), 1016);
                prop_assert_eq!(DirBlock::parse(&b.encode()).unwrap(), b.clone());
            }
        }
    }
}
