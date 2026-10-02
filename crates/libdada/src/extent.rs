//! Extents (SPEC 4.6).

use crate::format::{ext, EXTENT_SIZE};
use crate::le::{get_u64, put_u64};
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
pub fn map_block(extents: &[Extent], logical: u64) -> Option<u64> {
    extents.iter().find_map(|e| e.map(logical))
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

    proptest! {
        #[test]
        fn round_trip(logical: u64, physical: u64, length: u64) {
            let x = e(logical, physical, length);
            prop_assert_eq!(Extent::decode(&x.encode()).unwrap(), x);
        }
    }
}
