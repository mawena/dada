//! Block and inode bitmaps (SPEC 4.3).
//!
//! Bit `i` is bit `i % 8` (least significant first) of byte `i / 8`; 1 means
//! used. Bits past `len`, up to the end of the bitmap zone, are set to 1.

use crate::DadaError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    bytes: Vec<u8>,
    len: u64,
}

fn byte_and_mask(i: u64) -> Result<(usize, u8), DadaError> {
    let byte = usize::try_from(i / 8).map_err(|_| DadaError::Invalid)?;
    Ok((byte, 1u8 << (i % 8)))
}

impl Bitmap {
    /// A bitmap of `len` clear bits stored in `zone_bytes` bytes, with the
    /// padding bits set.
    pub fn new(len: u64, zone_bytes: usize) -> Result<Self, DadaError> {
        let mut bitmap = Bitmap {
            bytes: vec![0; zone_bytes],
            len,
        };
        bitmap.check_capacity()?;
        let capacity = bitmap.capacity();
        bitmap.set_range(len, capacity - len, true)?;
        Ok(bitmap)
    }

    /// Bitmap read from disk; `bytes` is the whole zone.
    pub fn from_bytes(bytes: Vec<u8>, len: u64) -> Result<Self, DadaError> {
        let bitmap = Bitmap { bytes, len };
        bitmap.check_capacity()?;
        Ok(bitmap)
    }

    fn capacity(&self) -> u64 {
        (self.bytes.len() as u64).saturating_mul(8)
    }

    fn check_capacity(&self) -> Result<(), DadaError> {
        if self.len > self.capacity() {
            return Err(DadaError::Corrupt(format!(
                "bitmap of {} bits does not fit in {} bytes",
                self.len,
                self.bytes.len()
            )));
        }
        Ok(())
    }

    /// Number of meaningful bits.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Bit `i`; bits outside the zone fail with `Invalid`.
    pub fn get(&self, i: u64) -> Result<bool, DadaError> {
        let (byte, mask) = byte_and_mask(i)?;
        let b = self.bytes.get(byte).ok_or(DadaError::Invalid)?;
        Ok(b & mask != 0)
    }

    /// Sets bit `i`, which must be lower than `len()`.
    pub fn set(&mut self, i: u64, used: bool) -> Result<(), DadaError> {
        if i >= self.len {
            return Err(DadaError::Invalid);
        }
        self.put(i, used)
    }

    fn put(&mut self, i: u64, used: bool) -> Result<(), DadaError> {
        let (byte, mask) = byte_and_mask(i)?;
        let b = self.bytes.get_mut(byte).ok_or(DadaError::Invalid)?;
        if used {
            *b |= mask;
        } else {
            *b &= !mask;
        }
        Ok(())
    }

    /// Sets bits `start .. start + count`, which may include padding bits.
    pub fn set_range(&mut self, start: u64, count: u64, used: bool) -> Result<(), DadaError> {
        let end = start.checked_add(count).ok_or(DadaError::Invalid)?;
        if end > self.capacity() {
            return Err(DadaError::Invalid);
        }
        let mut i = start;
        while i < end {
            if i.is_multiple_of(8) && end - i >= 8 {
                let (byte, _) = byte_and_mask(i)?;
                let b = self.bytes.get_mut(byte).ok_or(DadaError::Invalid)?;
                *b = if used { 0xFF } else { 0 };
                i += 8;
            } else {
                self.put(i, used)?;
                i += 1;
            }
        }
        Ok(())
    }

    /// Number of used bits among the first `len()`.
    pub fn count_used(&self) -> u64 {
        let full = usize::try_from(self.len / 8).unwrap_or(usize::MAX);
        let mut count: u64 = self
            .bytes
            .iter()
            .take(full)
            .map(|b| u64::from(b.count_ones()))
            .sum();
        let rest = self.len % 8;
        if rest != 0 {
            if let Some(b) = self.bytes.get(full) {
                let mask = (1u8 << rest) - 1;
                count += u64::from((b & mask).count_ones());
            }
        }
        count
    }

    /// First clear bit at or after `from`, among the first `len()`.
    pub fn find_clear(&self, from: u64) -> Option<u64> {
        let mut i = from;
        while i < self.len {
            let (byte, mask) = byte_and_mask(i).ok()?;
            let b = *self.bytes.get(byte)?;
            if b == 0xFF && i.is_multiple_of(8) {
                i += 8;
                continue;
            }
            if b & mask == 0 {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// First-fit search in `lo..hi`: the first clear bit at or after `goal`
    /// (wrapping around to `lo`), extended to a run of at most `want` clear
    /// bits. Returns `(start, length)`.
    pub fn find_run(&self, goal: u64, lo: u64, hi: u64, want: u64) -> Option<(u64, u64)> {
        let hi = hi.min(self.len);
        if lo >= hi || want == 0 {
            return None;
        }
        let goal = if (lo..hi).contains(&goal) { goal } else { lo };
        let start = self
            .find_clear(goal)
            .filter(|&s| s < hi)
            .or_else(|| self.find_clear(lo).filter(|&s| s < goal))?;
        let mut end = start + 1;
        while end < hi && end - start < want && !self.get(end).unwrap_or(true) {
            end += 1;
        }
        Some((start, end - start))
    }

    /// Whether every padding bit (past `len()`) is set.
    pub fn padding_is_set(&self) -> bool {
        let mut i = self.len;
        while i < self.capacity() {
            if i.is_multiple_of(8) {
                let rest = self
                    .bytes
                    .get(usize::try_from(i / 8).unwrap_or(usize::MAX)..);
                return rest.is_some_and(|r| r.iter().all(|&b| b == 0xFF));
            }
            if !self.get(i).unwrap_or(false) {
                return false;
            }
            i += 1;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn bit_order_is_lsb_first() {
        let mut b = Bitmap::new(16, 2).unwrap();
        b.set(0, true).unwrap();
        b.set(9, true).unwrap();
        assert_eq!(b.as_bytes(), &[0b0000_0001, 0b0000_0010]);
        assert!(b.get(9).unwrap());
        b.set(9, false).unwrap();
        assert!(!b.get(9).unwrap());
    }

    #[test]
    fn padding_is_set_to_the_end_of_the_zone() {
        let b = Bitmap::new(10, 4).unwrap();
        assert_eq!(b.as_bytes(), &[0, 0b1111_1100, 0xFF, 0xFF]);
        assert!(b.padding_is_set());
        assert_eq!(b.count_used(), 0);

        let mut raw = b.as_bytes().to_vec();
        raw[3] = 0x7F;
        assert!(!Bitmap::from_bytes(raw, 10).unwrap().padding_is_set());
        let mut raw = b.as_bytes().to_vec();
        raw[1] = 0b1111_1000;
        assert!(!Bitmap::from_bytes(raw, 10).unwrap().padding_is_set());
    }

    #[test]
    fn ranges_and_counts() {
        let mut b = Bitmap::new(100, 16).unwrap();
        b.set_range(3, 70, true).unwrap();
        assert_eq!(b.count_used(), 70);
        assert_eq!(b.find_clear(0), Some(0));
        assert_eq!(b.find_clear(3), Some(73));
        b.set_range(73, 27, true).unwrap();
        b.set_range(0, 3, true).unwrap();
        assert_eq!(b.count_used(), 100);
        assert_eq!(b.find_clear(0), None);
        b.set(64, false).unwrap();
        assert_eq!(b.find_clear(0), Some(64));
    }

    #[test]
    fn runs() {
        let mut b = Bitmap::new(100, 16).unwrap();
        b.set_range(0, 10, true).unwrap();
        b.set_range(20, 5, true).unwrap();
        assert_eq!(b.find_run(0, 10, 90, 4), Some((10, 4)));
        assert_eq!(b.find_run(15, 10, 90, 100), Some((15, 5)));
        assert_eq!(b.find_run(22, 10, 90, 3), Some((25, 3)));
        // The run stops at `hi`.
        assert_eq!(b.find_run(85, 10, 90, 10), Some((85, 5)));
        // Wraps around to `lo` when nothing is free after the goal.
        b.set_range(25, 75, true).unwrap();
        assert_eq!(b.find_run(50, 10, 90, 2), Some((10, 2)));
        // A goal outside the range starts at `lo`.
        assert_eq!(b.find_run(5, 10, 90, 1), Some((10, 1)));
        b.set_range(10, 10, true).unwrap();
        assert_eq!(b.find_run(50, 10, 90, 2), None);
        assert_eq!(b.find_run(0, 50, 50, 1), None);
        assert_eq!(b.find_run(0, 0, 100, 0), None);
    }

    #[test]
    fn out_of_range_fails() {
        let mut b = Bitmap::new(10, 2).unwrap();
        assert!(b.set(10, true).is_err());
        assert!(b.get(16).is_err());
        assert!(b.get(u64::MAX).is_err());
        assert!(b.set_range(8, 9, true).is_err());
        assert!(b.set_range(u64::MAX, 2, true).is_err());
        assert!(Bitmap::new(17, 2).is_err());
        assert!(Bitmap::from_bytes(vec![0; 2], 17).is_err());
    }

    proptest! {
        #[test]
        fn count_matches_naive(len in 0u64..300, bits in proptest::collection::vec(any::<bool>(), 300)) {
            let mut b = Bitmap::new(len, 40).unwrap();
            let mut expected = 0;
            for i in 0..len {
                if bits[i as usize] {
                    b.set(i, true).unwrap();
                    expected += 1;
                }
            }
            prop_assert_eq!(b.count_used(), expected);
            prop_assert!(b.padding_is_set());
            let first_clear = (0..len).find(|&i| !bits[i as usize]);
            prop_assert_eq!(b.find_clear(0), first_clear);
        }
    }
}
