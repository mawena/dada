//! Constants of the on-disk format, version 1 (see SPEC.md).
//!
//! Every offset, size, magic number and reserved inode number of the format
//! is defined here and nowhere else.

/// Version of the on-disk format implemented by this crate.
pub const FORMAT_VERSION: u16 = 1;

// ---------------------------------------------------------------------------
// Block size (SPEC 4.1)
// ---------------------------------------------------------------------------

pub const MIN_BLOCK_SIZE: u32 = 1024;
pub const MAX_BLOCK_SIZE: u32 = 65536;
pub const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// A block size is valid if it is a power of two between 1 KiB and 64 KiB.
pub const fn is_valid_block_size(block_size: u32) -> bool {
    block_size.is_power_of_two() && block_size >= MIN_BLOCK_SIZE && block_size <= MAX_BLOCK_SIZE
}

// ---------------------------------------------------------------------------
// Inode numbers (SPEC 4.1)
// ---------------------------------------------------------------------------

pub const INO_INVALID: u64 = 0;
pub const INO_ROOT: u64 = 1;
pub const INO_JOURNAL: u64 = 2;
/// Inodes 0 to 15 are reserved; user inodes start here.
pub const FIRST_USER_INO: u64 = 16;

/// Size of an on-disk inode in bytes (SPEC 4.5).
pub const INODE_SIZE: u32 = 256;

/// Maximum length of a name in bytes (SPEC 4.9).
pub const MAX_NAME_LEN: usize = 255;

// ---------------------------------------------------------------------------
// Volume layout (SPEC 4.2, 4.3)
// ---------------------------------------------------------------------------

pub const DEFAULT_INODE_RATIO: u64 = 16384;
pub const MIN_INODE_COUNT: u64 = 64;
/// The journal takes `total_blocks / JOURNAL_SIZE_DIVISOR` blocks, clamped.
pub const JOURNAL_SIZE_DIVISOR: u64 = 100;
pub const JOURNAL_MIN_BLOCKS: u64 = 256;
pub const JOURNAL_MAX_BLOCKS: u64 = 32768;
/// Minimum number of data blocks for a volume to be formatted.
pub const MIN_DATA_BLOCKS: u64 = 16;
/// The block bitmap always starts right after the superblock block.
pub const BLOCK_BITMAP_START: u64 = 1;

// ---------------------------------------------------------------------------
// Superblock (SPEC 4.4)
// ---------------------------------------------------------------------------

pub const SUPERBLOCK_MAGIC: [u8; 4] = *b"DADA";
pub const SUPERBLOCK_SIZE: usize = 1024;
pub const LABEL_LEN: usize = 32;
pub const UUID_LEN: usize = 16;

pub const STATE_CLEAN: u16 = 0;
pub const STATE_DIRTY: u16 = 1;

/// Byte offsets of the superblock fields.
pub mod sb {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const STATE: usize = 6;
    pub const BLOCK_SIZE: usize = 8;
    pub const TOTAL_BLOCKS: usize = 16;
    pub const FREE_BLOCKS: usize = 24;
    pub const INODE_COUNT: usize = 32;
    pub const FREE_INODES: usize = 40;
    pub const ROOT_INODE: usize = 48;
    pub const BLOCK_BITMAP_START: usize = 56;
    pub const INODE_BITMAP_START: usize = 64;
    pub const INODE_TABLE_START: usize = 72;
    pub const DATA_START: usize = 80;
    pub const JOURNAL_START: usize = 88;
    pub const JOURNAL_BLOCKS: usize = 96;
    pub const UUID: usize = 104;
    pub const FEATURES_COMPAT: usize = 120;
    pub const FEATURES_INCOMPAT: usize = 128;
    pub const LABEL: usize = 136;
    pub const CREATED_NS: usize = 168;
    pub const LAST_MOUNT_NS: usize = 176;
    pub const MOUNT_COUNT: usize = 184;
    /// The checksum covers bytes `0..CHECKSUM`.
    pub const CHECKSUM: usize = 1020;
}

/// `features_incompat`: names are compared case-insensitively.
pub const INCOMPAT_CASEFOLD: u64 = 1 << 0;
/// `features_incompat`: a journal is present.
pub const INCOMPAT_JOURNAL: u64 = 1 << 1;
/// `features_incompat`: at least one inode uses extent blocks.
pub const INCOMPAT_EXTENT_BLOCKS: u64 = 1 << 2;
/// All `features_incompat` bits understood by this implementation.
pub const INCOMPAT_SUPPORTED: u64 = INCOMPAT_CASEFOLD | INCOMPAT_JOURNAL | INCOMPAT_EXTENT_BLOCKS;

/// `features_compat`: extended attributes present (reserved in v1).
pub const COMPAT_XATTR: u64 = 1 << 0;
/// `features_compat`: Windows attributes are populated.
pub const COMPAT_WIN_ATTRS: u64 = 1 << 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_size_validity() {
        for bs in [1024, 2048, 4096, 8192, 16384, 32768, 65536] {
            assert!(is_valid_block_size(bs), "{bs}");
        }
        for bs in [0, 512, 1000, 3000, 4095, 131072, u32::MAX] {
            assert!(!is_valid_block_size(bs), "{bs}");
        }
    }

    #[test]
    fn superblock_offsets_fit() {
        assert_eq!(sb::MOUNT_COUNT + 4 + 832, sb::CHECKSUM);
        assert_eq!(sb::CHECKSUM + 4, SUPERBLOCK_SIZE);
        assert_eq!(sb::LABEL + LABEL_LEN, sb::CREATED_NS);
        assert_eq!(sb::UUID + UUID_LEN, sb::FEATURES_COMPAT);
    }
}
