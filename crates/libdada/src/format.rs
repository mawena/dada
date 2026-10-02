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

// ---------------------------------------------------------------------------
// Bitmaps (SPEC 4.3)
// ---------------------------------------------------------------------------

/// Inodes 0 to `RESERVED_INODES - 1` are always marked used.
pub const RESERVED_INODES: u64 = FIRST_USER_INO;

// ---------------------------------------------------------------------------
// Inode (SPEC 4.5)
// ---------------------------------------------------------------------------

/// Byte offsets of the inode fields.
pub mod ino {
    pub const MODE: usize = 0;
    pub const UID: usize = 4;
    pub const GID: usize = 8;
    pub const WIN_ATTRS: usize = 12;
    pub const SIZE: usize = 16;
    pub const LINKS: usize = 24;
    pub const FLAGS: usize = 28;
    pub const ATIME: usize = 32;
    pub const MTIME: usize = 40;
    pub const CTIME: usize = 48;
    pub const BTIME: usize = 56;
    pub const EXTENT_COUNT: usize = 64;
    /// Four inline extents, or inline data.
    pub const EXTENTS: usize = 72;
    pub const EXTENT_BLOCK: usize = 168;
    pub const XATTR_BLOCK: usize = 176;
    pub const GENERATION: usize = 184;
    /// The checksum covers the inode number (u64 LE) followed by bytes `0..CHECKSUM`.
    pub const CHECKSUM: usize = 252;
}

/// Inode flag: content is stored inline in the extents area.
pub const INODE_FLAG_INLINE_DATA: u32 = 1 << 0;
/// All inode flags understood by this implementation.
pub const INODE_FLAGS_SUPPORTED: u32 = INODE_FLAG_INLINE_DATA;
/// Maximum size of inline content.
pub const INLINE_DATA_MAX: usize = 96;
/// Number of extents stored in the inode itself.
pub const INODE_INLINE_EXTENTS: usize = 4;

/// File type is `mode >> MODE_TYPE_SHIFT`.
pub const MODE_TYPE_SHIFT: u32 = 12;
pub const MODE_TYPE_DIR: u32 = 0x4;
pub const MODE_TYPE_REGULAR: u32 = 0x8;
pub const MODE_TYPE_SYMLINK: u32 = 0xA;
/// POSIX permission bits (including setuid, setgid, sticky).
pub const MODE_PERM_MASK: u32 = 0o7777;

pub const WIN_ATTR_READONLY: u32 = 1 << 0;
pub const WIN_ATTR_HIDDEN: u32 = 1 << 1;
pub const WIN_ATTR_SYSTEM: u32 = 1 << 2;
pub const WIN_ATTR_ARCHIVE: u32 = 1 << 5;

// ---------------------------------------------------------------------------
// Extent (SPEC 4.6)
// ---------------------------------------------------------------------------

pub const EXTENT_SIZE: usize = 24;

/// Byte offsets of the extent fields.
pub mod ext {
    pub const LOGICAL: usize = 0;
    pub const PHYSICAL: usize = 8;
    pub const LENGTH: usize = 16;
}

// ---------------------------------------------------------------------------
// Extent block (SPEC 4.7)
// ---------------------------------------------------------------------------

pub const EXTENT_BLOCK_MAGIC: [u8; 4] = *b"DEXT";

/// Byte offsets of the extent block fields.
pub mod xblk {
    pub const MAGIC: usize = 0;
    pub const COUNT: usize = 4;
    pub const NEXT: usize = 8;
    pub const OWNER: usize = 16;
    pub const EXTENTS: usize = 24;
    /// Header plus trailing checksum.
    pub const OVERHEAD: usize = 28;
}

/// Number of extents an extent block holds.
pub const fn extent_block_capacity(block_size: u32) -> usize {
    (block_size as usize - xblk::OVERHEAD) / EXTENT_SIZE
}

// ---------------------------------------------------------------------------
// Directories (SPEC 4.8)
// ---------------------------------------------------------------------------

/// Bytes at the end of a directory block: CRC32C (4) then reserved (4).
pub const DIR_BLOCK_TAIL: usize = 8;
/// Directory entries are aligned on this many bytes.
pub const DIR_ENTRY_ALIGN: usize = 8;

/// Byte offsets of the directory entry fields.
pub mod dirent {
    pub const INODE: usize = 0;
    pub const REC_LEN: usize = 8;
    pub const NAME_LEN: usize = 10;
    pub const FILE_TYPE: usize = 11;
    pub const NAME: usize = 12;
}

pub const FT_REGULAR: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_SYMLINK: u8 = 7;

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

    #[test]
    fn inode_offsets_fit() {
        assert_eq!(
            ino::EXTENTS + INODE_INLINE_EXTENTS * EXTENT_SIZE,
            ino::EXTENT_BLOCK
        );
        assert_eq!(ino::EXTENTS + INLINE_DATA_MAX, ino::EXTENT_BLOCK);
        assert_eq!(ino::GENERATION + 4 + 64, ino::CHECKSUM);
        assert_eq!(ino::CHECKSUM + 4, INODE_SIZE as usize);
        assert_eq!(ext::LENGTH + 8, EXTENT_SIZE);
    }
}
