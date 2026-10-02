//! Extents and extent blocks (SPEC 4.6, 4.7).

use crate::crc::crc32c;
use crate::format::{
    ext, extent_block_capacity, is_valid_block_size, xblk, EXTENT_BLOCK_MAGIC, EXTENT_SIZE,
};
use crate::le::{get_bytes, get_u32, get_u64, put_bytes, put_u32, put_u64};
use crate::DadaError;

/// `length` blocks starting at file block `logical`, stored at `physical`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Extent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
}

impl Extent {
    pub fn encode(&self) -> [u8; EXTENT_SIZE] {
        let mut b = [0u8; EXTENT_SIZE];
        put_u64(&mut b, ext::LOGICAL, self.logical);
        put_u64(&mut b, ext::PHYSICAL, self.physical);
        put_u64(&mut b, ext::LENGTH, self.length);
        b
    }

    pub fn decode(buf: &[u8]) -> Result<Self, DadaError> {
        Ok(Extent {
            logical: get_u64(buf, ext::LOGICAL)?,
            physical: get_u64(buf, ext::PHYSICAL)?,
            length: get_u64(buf, ext::LENGTH)?,
        })
    }

    /// First logical block after the extent.
    pub fn logical_end(&self) -> Option<u64> {
        self.logical.checked_add(self.length)
    }

    /// First physical block after the extent.
    pub fn physical_end(&self) -> Option<u64> {
        self.physical.checked_add(self.length)
    }

    /// Physical block of `logical`, if the extent covers it.
    pub fn map(&self, logical: u64) -> Option<u64> {
        if logical >= self.logical && logical < self.logical_end()? {
            self.physical.checked_add(logical - self.logical)
        } else {
            None
        }
    }
}

/// Checks that extents are non-empty, sorted by `logical`, non-overlapping
/// and without arithmetic overflow.
pub fn validate_extents(extents: &[Extent]) -> Result<(), DadaError> {
    let mut next_logical = 0u64;
    for (i, e) in extents.iter().enumerate() {
        let bad = |what: &str| DadaError::Corrupt(format!("extent {i}: {what}"));
        if e.length == 0 {
            return Err(bad("empty"));
        }
        if e.logical < next_logical {
            return Err(bad("unsorted or overlapping"));
        }
        next_logical = e.logical_end().ok_or_else(|| bad("logical overflow"))?;
        e.physical_end().ok_or_else(|| bad("physical overflow"))?;
    }
    Ok(())
}

/// Physical block holding file block `logical`, or `None` for a hole.
/// `extents` must be sorted (see `validate_extents`).
pub fn map_block(extents: &[Extent], logical: u64) -> Option<u64> {
    let i = extents.partition_point(|e| e.logical.saturating_add(e.length) <= logical);
    extents.get(i)?.map(logical)
}

/// Unmapped runs `(logical, length)` of the logical range `start..end`.
pub fn holes(extents: &[Extent], start: u64, end: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut pos = start;
    for e in extents {
        if pos >= end {
            break;
        }
        let e_end = e.logical.saturating_add(e.length);
        if e_end <= pos {
            continue;
        }
        if e.logical > pos {
            let hole_end = e.logical.min(end);
            out.push((pos, hole_end - pos));
        }
        pos = pos.max(e_end);
    }
    if pos < end {
        out.push((pos, end - pos));
    }
    out
}

/// Inserts `new` into a sorted list it does not overlap, merging it with
/// neighbours that are contiguous both logically and physically.
pub fn insert_extent(extents: &mut Vec<Extent>, new: Extent) {
    let i = extents.partition_point(|e| e.logical < new.logical);
    extents.insert(i, new);
    // Merge with the next extent, then with the previous one.
    if let (Some(&cur), Some(&next)) = (extents.get(i), extents.get(i + 1)) {
        if cur.logical_end() == Some(next.logical) && cur.physical_end() == Some(next.physical) {
            if let Some(c) = extents.get_mut(i) {
                c.length += next.length;
            }
            extents.remove(i + 1);
        }
    }
    if let Some(prev) = i.checked_sub(1) {
        if let (Some(&p), Some(&cur)) = (extents.get(prev), extents.get(i)) {
            if p.logical_end() == Some(cur.logical) && p.physical_end() == Some(cur.physical) {
                if let Some(pe) = extents.get_mut(prev) {
                    pe.length += cur.length;
                }
                extents.remove(i);
            }
        }
    }
}

/// Keeps only logical blocks `0..keep`. Returns the released physical runs
/// `(physical, length)`.
pub fn truncate_extents(extents: &mut Vec<Extent>, keep: u64) -> Vec<(u64, u64)> {
    let mut freed = Vec::new();
    extents.retain_mut(|e| {
        if e.logical >= keep {
            freed.push((e.physical, e.length));
            return false;
        }
        let end = e.logical.saturating_add(e.length);
        if end > keep {
            let kept = keep - e.logical;
            freed.push((e.physical.saturating_add(kept), e.length - kept));
            e.length = kept;
        }
        true
    });
    freed
}

/// Total number of mapped blocks.
pub fn mapped_blocks(extents: &[Extent]) -> u64 {
    extents
        .iter()
        .fold(0u64, |acc, e| acc.saturating_add(e.length))
}

/// An extent block: extents past the four stored in the inode (SPEC 4.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentBlock {
    /// Next extent block, 0 at the end of the chain.
    pub next: u64,
    pub owner: u64,
    pub extents: Vec<Extent>,
}

impl ExtentBlock {
    /// Encodes the block; extents beyond the capacity are dropped (callers
    /// split lists with `extent_block_capacity`).
    pub fn encode(&self, block_size: u32) -> Vec<u8> {
        let mut buf = vec![0u8; block_size as usize];
        let extents = self
            .extents
            .get(..extent_block_capacity(block_size))
            .unwrap_or(&self.extents);
        put_bytes(&mut buf, xblk::MAGIC, &EXTENT_BLOCK_MAGIC);
        put_u32(&mut buf, xblk::COUNT, extents.len() as u32);
        put_u64(&mut buf, xblk::NEXT, self.next);
        put_u64(&mut buf, xblk::OWNER, self.owner);
        for (i, e) in extents.iter().enumerate() {
            put_bytes(&mut buf, xblk::EXTENTS + i * EXTENT_SIZE, &e.encode());
        }
        let end = buf.len() - 4;
        let crc = crc32c(buf.get(..end).unwrap_or_default());
        put_u32(&mut buf, end, crc);
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self, DadaError> {
        let corrupt = |msg: &str| DadaError::Corrupt(format!("extent block: {msg}"));
        if !u32::try_from(buf.len()).is_ok_and(is_valid_block_size) {
            return Err(DadaError::Invalid);
        }
        if get_bytes::<4>(buf, xblk::MAGIC)? != EXTENT_BLOCK_MAGIC {
            return Err(corrupt("bad magic"));
        }
        let end = buf.len() - 4;
        if get_u32(buf, end)? != crc32c(buf.get(..end).unwrap_or_default()) {
            return Err(corrupt("bad checksum"));
        }
        let count = get_u32(buf, xblk::COUNT)? as usize;
        if count == 0 || count > extent_block_capacity(buf.len() as u32) {
            return Err(corrupt("bad count"));
        }
        let extents = (0..count)
            .map(|i| {
                Extent::decode(
                    buf.get(xblk::EXTENTS + i * EXTENT_SIZE..)
                        .unwrap_or_default(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ExtentBlock {
            next: get_u64(buf, xblk::NEXT)?,
            owner: get_u64(buf, xblk::OWNER)?,
            extents,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn e(logical: u64, physical: u64, length: u64) -> Extent {
        Extent {
            logical,
            physical,
            length,
        }
    }

    #[test]
    fn mapping() {
        let list = [e(0, 100, 2), e(5, 200, 3)];
        validate_extents(&list).unwrap();
        assert_eq!(map_block(&list, 0), Some(100));
        assert_eq!(map_block(&list, 1), Some(101));
        assert_eq!(map_block(&list, 2), None);
        assert_eq!(map_block(&list, 7), Some(202));
        assert_eq!(map_block(&list, 8), None);
    }

    #[test]
    fn validation() {
        assert!(validate_extents(&[]).is_ok());
        assert!(validate_extents(&[e(0, 1, 0)]).is_err());
        assert!(validate_extents(&[e(4, 1, 1), e(0, 2, 1)]).is_err());
        assert!(validate_extents(&[e(0, 1, 5), e(4, 9, 1)]).is_err());
        assert!(validate_extents(&[e(u64::MAX, 1, 2)]).is_err());
        assert!(validate_extents(&[e(0, u64::MAX, 2)]).is_err());
        assert!(validate_extents(&[e(0, 1, 5), e(5, 9, 1)]).is_ok());
    }

    #[test]
    fn holes_in_range() {
        let list = [e(2, 100, 2), e(6, 200, 1)];
        assert_eq!(holes(&list, 0, 10), [(0, 2), (4, 2), (7, 3)]);
        assert_eq!(holes(&list, 2, 4), []);
        assert_eq!(holes(&list, 3, 7), [(4, 2)]);
        assert_eq!(holes(&[], 5, 8), [(5, 3)]);
        assert_eq!(holes(&list, 5, 5), []);
    }

    #[test]
    fn insert_merges_contiguous_extents() {
        let mut list = vec![e(0, 100, 2), e(4, 104, 2)];
        insert_extent(&mut list, e(2, 102, 2));
        assert_eq!(list, [e(0, 100, 6)]);

        let mut list = vec![e(0, 100, 2)];
        insert_extent(&mut list, e(2, 500, 1)); // logically but not physically contiguous
        insert_extent(&mut list, e(10, 501, 1)); // physically but not logically contiguous
        assert_eq!(list, [e(0, 100, 2), e(2, 500, 1), e(10, 501, 1)]);
        validate_extents(&list).unwrap();
    }

    #[test]
    fn truncation() {
        let mut list = vec![e(0, 100, 4), e(6, 200, 4)];
        assert_eq!(truncate_extents(&mut list, 8), [(202, 2)]);
        assert_eq!(list, [e(0, 100, 4), e(6, 200, 2)]);
        assert_eq!(truncate_extents(&mut list, 2), [(102, 2), (200, 2)]);
        assert_eq!(list, [e(0, 100, 2)]);
        assert_eq!(truncate_extents(&mut list, 0), [(100, 2)]);
        assert!(list.is_empty());
    }

    #[test]
    fn extent_block_round_trip_and_rejects() {
        let blk = ExtentBlock {
            next: 77,
            owner: 16,
            extents: (0..10).map(|i| e(i * 3, 1000 + i * 5, 2)).collect(),
        };
        let bytes = blk.encode(1024);
        assert_eq!(&bytes[..4], b"DEXT");
        assert_eq!(ExtentBlock::decode(&bytes).unwrap(), blk);
        assert_eq!(extent_block_capacity(1024), 41);
        assert_eq!(extent_block_capacity(4096), 169);

        let mut bad = bytes.clone();
        bad[30] ^= 1;
        assert!(ExtentBlock::decode(&bad).is_err());
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(ExtentBlock::decode(&bad).is_err());
        assert!(ExtentBlock::decode(&bytes[..1000]).is_err());

        // A count of zero or past the capacity is rejected even with a valid checksum.
        for count in [0u32, 42] {
            let mut bad = bytes.clone();
            bad[4..8].copy_from_slice(&count.to_le_bytes());
            let crc = crc32c(&bad[..1020]);
            bad[1020..].copy_from_slice(&crc.to_le_bytes());
            assert!(ExtentBlock::decode(&bad).is_err(), "{count}");
        }
    }

    proptest! {
        #[test]
        fn round_trip(logical: u64, physical: u64, length: u64) {
            let x = e(logical, physical, length);
            prop_assert_eq!(Extent::decode(&x.encode()).unwrap(), x);
        }

        #[test]
        fn extent_block_decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 1024)) {
            let _ = ExtentBlock::decode(&bytes);
        }

        /// Building a file block by block in random order yields a valid,
        /// fully merged list that maps every block to where it was put.
        #[test]
        fn insert_and_map_agree(order in Just((0u64..64).collect::<Vec<_>>()).prop_shuffle()) {
            let mut list = Vec::new();
            for &l in &order {
                insert_extent(&mut list, e(l, 1000 + l, 1));
            }
            validate_extents(&list).unwrap();
            prop_assert_eq!(list.clone(), vec![e(0, 1000, 64)]);
            for l in 0..64 {
                prop_assert_eq!(map_block(&list, l), Some(1000 + l));
            }
            prop_assert_eq!(map_block(&list, 64), None);
        }
    }
}
