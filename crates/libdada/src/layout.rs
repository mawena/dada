//! Volume layout computed at format time (SPEC 4.3).

use crate::format::{
    is_valid_block_size, BLOCK_BITMAP_START, INODE_SIZE, JOURNAL_MAX_BLOCKS, JOURNAL_MIN_BLOCKS,
    JOURNAL_SIZE_DIVISOR, MIN_DATA_BLOCKS, MIN_INODE_COUNT,
};
use crate::{DadaError, FormatOptions};

/// Position and size, in blocks, of every zone of a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub block_size: u32,
    pub total_blocks: u64,
    pub inode_count: u64,
    pub block_bitmap_start: u64,
    pub block_bitmap_blocks: u64,
    pub inode_bitmap_start: u64,
    pub inode_bitmap_blocks: u64,
    pub inode_table_start: u64,
    pub inode_table_blocks: u64,
    /// 0 when the volume has no journal.
    pub journal_start: u64,
    pub journal_blocks: u64,
    pub data_start: u64,
}

fn overflow() -> DadaError {
    DadaError::Invalid
}

impl Layout {
    /// Computes the layout of a volume of `volume_bytes` bytes.
    ///
    /// Fails with `Invalid` for a bad block size or inode ratio, and with
    /// `NoSpace` when the volume is too small to hold the metadata plus
    /// `MIN_DATA_BLOCKS` data blocks.
    pub fn compute(volume_bytes: u64, opts: &FormatOptions) -> Result<Self, DadaError> {
        if !is_valid_block_size(opts.block_size) || opts.inode_ratio == 0 {
            return Err(DadaError::Invalid);
        }
        let bs = u64::from(opts.block_size);
        let total_blocks = volume_bytes / bs;
        if total_blocks < 2 {
            return Err(DadaError::NoSpace);
        }

        // The minimum is applied before rounding, so the count is always a
        // whole number of inode table blocks.
        let inodes_per_block = bs / u64::from(INODE_SIZE);
        let inode_count = (volume_bytes / opts.inode_ratio)
            .max(MIN_INODE_COUNT)
            .div_ceil(inodes_per_block)
            .checked_mul(inodes_per_block)
            .ok_or_else(overflow)?;

        let bits_per_block = bs * 8;
        let block_bitmap_blocks = total_blocks.div_ceil(bits_per_block);
        let inode_bitmap_blocks = inode_count.div_ceil(bits_per_block);
        let inode_table_blocks = inode_count / inodes_per_block;
        let journal_blocks = if opts.journal {
            (total_blocks / JOURNAL_SIZE_DIVISOR).clamp(JOURNAL_MIN_BLOCKS, JOURNAL_MAX_BLOCKS)
        } else {
            0
        };

        let block_bitmap_start = BLOCK_BITMAP_START;
        let inode_bitmap_start = block_bitmap_start
            .checked_add(block_bitmap_blocks)
            .ok_or_else(overflow)?;
        let inode_table_start = inode_bitmap_start
            .checked_add(inode_bitmap_blocks)
            .ok_or_else(overflow)?;
        let inode_table_end = inode_table_start
            .checked_add(inode_table_blocks)
            .ok_or_else(overflow)?;
        let journal_start = if opts.journal { inode_table_end } else { 0 };
        let data_start = inode_table_end
            .checked_add(journal_blocks)
            .ok_or_else(overflow)?;

        let backup = total_blocks - 1;
        if data_start.saturating_add(MIN_DATA_BLOCKS) > backup {
            return Err(DadaError::NoSpace);
        }

        Ok(Layout {
            block_size: opts.block_size,
            total_blocks,
            inode_count,
            block_bitmap_start,
            block_bitmap_blocks,
            inode_bitmap_start,
            inode_bitmap_blocks,
            inode_table_start,
            inode_table_blocks,
            journal_start,
            journal_blocks,
            data_start,
        })
    }

    /// Block holding the backup superblock.
    pub fn backup_superblock(&self) -> u64 {
        self.total_blocks - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const BLOCK_SIZES: [u32; 7] = [1024, 2048, 4096, 8192, 16384, 32768, 65536];

    fn opts(block_size: u32, journal: bool) -> FormatOptions {
        FormatOptions {
            block_size,
            journal,
            ..FormatOptions::default()
        }
    }

    /// Invariants every computed layout must satisfy.
    fn check_invariants(l: &Layout, volume_bytes: u64, o: &FormatOptions) {
        let bs = u64::from(l.block_size);
        let ipb = bs / 256;
        assert_eq!(l.total_blocks, volume_bytes / bs);
        assert!(l.inode_count >= MIN_INODE_COUNT);
        assert_eq!(l.inode_count % ipb, 0);
        assert!(l.inode_count >= volume_bytes / o.inode_ratio);
        assert!(l.inode_count - volume_bytes / o.inode_ratio < ipb.max(MIN_INODE_COUNT));
        assert!(l.block_bitmap_blocks * bs * 8 >= l.total_blocks);
        assert!(l.inode_bitmap_blocks * bs * 8 >= l.inode_count);
        assert_eq!(l.inode_table_blocks * bs, l.inode_count * 256);
        assert_eq!(l.block_bitmap_start, 1);
        assert_eq!(
            l.inode_bitmap_start,
            l.block_bitmap_start + l.block_bitmap_blocks
        );
        assert_eq!(
            l.inode_table_start,
            l.inode_bitmap_start + l.inode_bitmap_blocks
        );
        let table_end = l.inode_table_start + l.inode_table_blocks;
        if o.journal {
            assert_eq!(l.journal_start, table_end);
            assert!((JOURNAL_MIN_BLOCKS..=JOURNAL_MAX_BLOCKS).contains(&l.journal_blocks));
        } else {
            assert_eq!((l.journal_start, l.journal_blocks), (0, 0));
        }
        assert_eq!(l.data_start, table_end + l.journal_blocks);
        assert!(l.data_start + MIN_DATA_BLOCKS <= l.backup_superblock());
    }

    #[test]
    fn hundred_mib_default() {
        let o = FormatOptions::default();
        let l = Layout::compute(100 * MIB, &o).unwrap();
        check_invariants(&l, 100 * MIB, &o);
        assert_eq!(
            l,
            Layout {
                block_size: 4096,
                total_blocks: 25600,
                inode_count: 6400,
                block_bitmap_start: 1,
                block_bitmap_blocks: 1,
                inode_bitmap_start: 2,
                inode_bitmap_blocks: 1,
                inode_table_start: 3,
                inode_table_blocks: 400,
                journal_start: 403,
                journal_blocks: 256,
                data_start: 659,
            }
        );
        assert_eq!(l.backup_superblock(), 25599);
    }

    #[test]
    fn one_mib() {
        // 1 KiB blocks: the minimum journal fits.
        let l = Layout::compute(MIB, &opts(1024, true)).unwrap();
        assert_eq!(
            (l.total_blocks, l.inode_count, l.inode_table_blocks),
            (1024, 64, 16)
        );
        assert_eq!(
            (l.journal_start, l.journal_blocks, l.data_start),
            (19, 256, 275)
        );

        // 4 KiB blocks: 256 blocks cannot hold a 256-block journal...
        assert!(matches!(
            Layout::compute(MIB, &opts(4096, true)),
            Err(DadaError::NoSpace)
        ));
        // ...but fit without one.
        let l = Layout::compute(MIB, &opts(4096, false)).unwrap();
        assert_eq!(
            (l.inode_count, l.inode_table_blocks, l.data_start),
            (64, 4, 7)
        );

        // 64 KiB blocks: 16 blocks are not enough.
        assert!(matches!(
            Layout::compute(MIB, &opts(65536, false)),
            Err(DadaError::NoSpace)
        ));
    }

    #[test]
    fn ten_gib() {
        let l = Layout::compute(10 * GIB, &opts(4096, true)).unwrap();
        assert_eq!(l.total_blocks, 2_621_440);
        assert_eq!(l.inode_count, 655_360);
        assert_eq!((l.block_bitmap_blocks, l.inode_bitmap_blocks), (80, 20));
        assert_eq!(l.inode_table_blocks, 40_960);
        assert_eq!(l.journal_blocks, 26_214);
        assert_eq!(l.data_start, 1 + 80 + 20 + 40_960 + 26_214);

        let l = Layout::compute(10 * GIB, &opts(65536, true)).unwrap();
        assert_eq!(l.total_blocks, 163_840);
        // 655 360 inodes need 1.25 bitmap blocks of 524 288 bits.
        assert_eq!((l.block_bitmap_blocks, l.inode_bitmap_blocks), (1, 2));
        assert_eq!(l.inode_table_blocks, 2560);
        assert_eq!(l.journal_blocks, 1638);
    }

    #[test]
    fn journal_is_clamped() {
        let l = Layout::compute(100 * MIB, &opts(1024, true)).unwrap();
        assert_eq!(l.journal_blocks, 1024); // 102 400 / 100
        let l = Layout::compute(1024 * GIB, &opts(4096, true)).unwrap();
        assert_eq!(l.journal_blocks, JOURNAL_MAX_BLOCKS);
    }

    #[test]
    fn minimum_inode_count_rounded_to_whole_blocks() {
        // 64 KiB blocks hold 256 inodes: the 64-inode minimum becomes 256.
        let l = Layout::compute(4 * MIB, &opts(65536, false)).unwrap();
        assert_eq!((l.inode_count, l.inode_table_blocks), (256, 1));
    }

    #[test]
    fn all_block_sizes_and_sizes() {
        for bs in BLOCK_SIZES {
            for size in [MIB, 100 * MIB, 10 * GIB] {
                for journal in [false, true] {
                    let o = opts(bs, journal);
                    match Layout::compute(size, &o) {
                        Ok(l) => check_invariants(&l, size, &o),
                        Err(DadaError::NoSpace) => assert!(size <= MIB, "{bs} {size}"),
                        Err(e) => panic!("{bs} {size} {journal}: {e}"),
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_bad_options() {
        for bs in [0, 512, 3000, 131072] {
            assert!(matches!(
                Layout::compute(100 * MIB, &opts(bs, true)),
                Err(DadaError::Invalid)
            ));
        }
        let o = FormatOptions {
            inode_ratio: 0,
            ..FormatOptions::default()
        };
        assert!(matches!(
            Layout::compute(100 * MIB, &o),
            Err(DadaError::Invalid)
        ));
    }

    #[test]
    fn tiny_or_huge_volumes_do_not_panic() {
        for size in [0, 1, 4095, 4096, 8191, 8192] {
            assert!(matches!(
                Layout::compute(size, &FormatOptions::default()),
                Err(DadaError::NoSpace)
            ));
        }
        let o = FormatOptions {
            inode_ratio: 1,
            ..FormatOptions::default()
        };
        assert!(Layout::compute(u64::MAX, &o).is_err());
        assert!(Layout::compute(u64::MAX, &FormatOptions::default()).is_ok());
    }

    proptest! {
        #[test]
        fn computed_layouts_are_consistent(
            size in 0u64..(1u64 << 44),
            bs_idx in 0usize..BLOCK_SIZES.len(),
            inode_ratio in 1024u64..(1 << 20),
            journal: bool,
        ) {
            let o = FormatOptions {
                block_size: BLOCK_SIZES[bs_idx],
                inode_ratio,
                journal,
                ..FormatOptions::default()
            };
            match Layout::compute(size, &o) {
                Ok(l) => check_invariants(&l, size, &o),
                Err(e) => prop_assert!(matches!(e, DadaError::NoSpace), "{e}"),
            }
        }
    }
}
