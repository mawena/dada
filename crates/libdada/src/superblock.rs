//! Superblock: encoding, decoding and validation (SPEC 4.4).

use crate::crc::crc32c;
use crate::format::{
    is_valid_block_size, sb, BLOCK_BITMAP_START, FIRST_USER_INO, FORMAT_VERSION, INCOMPAT_CASEFOLD,
    INCOMPAT_JOURNAL, INCOMPAT_SUPPORTED, INODE_SIZE, INO_ROOT, LABEL_LEN, MIN_INODE_COUNT,
    STATE_CLEAN, STATE_DIRTY, SUPERBLOCK_MAGIC, SUPERBLOCK_SIZE, UUID_LEN,
};
use crate::layout::Layout;
use crate::le::{
    get_bytes, get_i64, get_u16, get_u32, get_u64, put_bytes, put_i64, put_u16, put_u32, put_u64,
};
use crate::{DadaError, FormatOptions};

/// In-memory form of the superblock. Magic, version, reserved fields and
/// checksum are handled by `encode` / `decode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub state: u16,
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub inode_count: u64,
    pub free_inodes: u64,
    pub root_inode: u64,
    pub block_bitmap_start: u64,
    pub inode_bitmap_start: u64,
    pub inode_table_start: u64,
    pub data_start: u64,
    pub journal_start: u64,
    pub journal_blocks: u64,
    pub uuid: [u8; UUID_LEN],
    pub features_compat: u64,
    pub features_incompat: u64,
    /// UTF-8, padded with zeros.
    pub label: [u8; LABEL_LEN],
    pub created_ns: i64,
    pub last_mount_ns: i64,
    pub mount_count: u32,
}

fn corrupt(msg: impl Into<String>) -> DadaError {
    DadaError::Corrupt(msg.into())
}

impl Superblock {
    /// Superblock of a freshly formatted volume, before any block or inode
    /// is allocated: every data block and every user inode is free.
    pub fn from_layout(
        layout: &Layout,
        opts: &FormatOptions,
        uuid: [u8; UUID_LEN],
        now_ns: i64,
    ) -> Result<Self, DadaError> {
        let mut features_incompat = 0;
        if opts.casefold {
            features_incompat |= INCOMPAT_CASEFOLD;
        }
        if opts.journal {
            features_incompat |= INCOMPAT_JOURNAL;
        }
        let free_blocks = layout
            .backup_superblock()
            .checked_sub(layout.data_start)
            .ok_or(DadaError::NoSpace)?;
        let free_inodes = layout
            .inode_count
            .checked_sub(FIRST_USER_INO)
            .ok_or(DadaError::NoInodes)?;
        let mut sb = Superblock {
            state: STATE_CLEAN,
            block_size: layout.block_size,
            total_blocks: layout.total_blocks,
            free_blocks,
            inode_count: layout.inode_count,
            free_inodes,
            root_inode: INO_ROOT,
            block_bitmap_start: layout.block_bitmap_start,
            inode_bitmap_start: layout.inode_bitmap_start,
            inode_table_start: layout.inode_table_start,
            data_start: layout.data_start,
            journal_start: layout.journal_start,
            journal_blocks: layout.journal_blocks,
            uuid,
            features_compat: 0,
            features_incompat,
            label: [0; LABEL_LEN],
            created_ns: now_ns,
            last_mount_ns: 0,
            mount_count: 0,
        };
        sb.set_label(&opts.label)?;
        Ok(sb)
    }

    /// Label, up to the first zero byte.
    pub fn label(&self) -> Result<&str, DadaError> {
        let len = self.label.iter().position(|&b| b == 0).unwrap_or(LABEL_LEN);
        let bytes = self.label.get(..len).unwrap_or_default();
        std::str::from_utf8(bytes).map_err(|_| corrupt("label is not valid UTF-8"))
    }

    /// Sets the label; it must fit in `LABEL_LEN` bytes and contain no NUL.
    pub fn set_label(&mut self, label: &str) -> Result<(), DadaError> {
        if label.len() > LABEL_LEN || label.contains('\0') {
            return Err(DadaError::Invalid);
        }
        self.label = [0; LABEL_LEN];
        put_bytes(&mut self.label, 0, label.as_bytes());
        Ok(())
    }

    pub fn encode(&self) -> [u8; SUPERBLOCK_SIZE] {
        let mut b = [0u8; SUPERBLOCK_SIZE];
        put_bytes(&mut b, sb::MAGIC, &SUPERBLOCK_MAGIC);
        put_u16(&mut b, sb::VERSION, FORMAT_VERSION);
        put_u16(&mut b, sb::STATE, self.state);
        put_u32(&mut b, sb::BLOCK_SIZE, self.block_size);
        put_u64(&mut b, sb::TOTAL_BLOCKS, self.total_blocks);
        put_u64(&mut b, sb::FREE_BLOCKS, self.free_blocks);
        put_u64(&mut b, sb::INODE_COUNT, self.inode_count);
        put_u64(&mut b, sb::FREE_INODES, self.free_inodes);
        put_u64(&mut b, sb::ROOT_INODE, self.root_inode);
        put_u64(&mut b, sb::BLOCK_BITMAP_START, self.block_bitmap_start);
        put_u64(&mut b, sb::INODE_BITMAP_START, self.inode_bitmap_start);
        put_u64(&mut b, sb::INODE_TABLE_START, self.inode_table_start);
        put_u64(&mut b, sb::DATA_START, self.data_start);
        put_u64(&mut b, sb::JOURNAL_START, self.journal_start);
        put_u64(&mut b, sb::JOURNAL_BLOCKS, self.journal_blocks);
        put_bytes(&mut b, sb::UUID, &self.uuid);
        put_u64(&mut b, sb::FEATURES_COMPAT, self.features_compat);
        put_u64(&mut b, sb::FEATURES_INCOMPAT, self.features_incompat);
        put_bytes(&mut b, sb::LABEL, &self.label);
        put_i64(&mut b, sb::CREATED_NS, self.created_ns);
        put_i64(&mut b, sb::LAST_MOUNT_NS, self.last_mount_ns);
        put_u32(&mut b, sb::MOUNT_COUNT, self.mount_count);
        let crc = crc32c(b.get(..sb::CHECKSUM).unwrap_or_default());
        put_u32(&mut b, sb::CHECKSUM, crc);
        b
    }

    /// Decodes the first `SUPERBLOCK_SIZE` bytes of `buf`, checking magic,
    /// version and checksum. Use `validate` for the semantic checks.
    pub fn decode(buf: &[u8]) -> Result<Self, DadaError> {
        let b = buf
            .get(..SUPERBLOCK_SIZE)
            .ok_or_else(|| corrupt("superblock truncated"))?;
        if get_bytes::<4>(b, sb::MAGIC)? != SUPERBLOCK_MAGIC {
            return Err(corrupt("bad superblock magic"));
        }
        let version = get_u16(b, sb::VERSION)?;
        if version != FORMAT_VERSION {
            return Err(corrupt(format!("unknown format version {version}")));
        }
        let stored = get_u32(b, sb::CHECKSUM)?;
        let computed = crc32c(b.get(..sb::CHECKSUM).unwrap_or_default());
        if stored != computed {
            return Err(corrupt(format!(
                "bad superblock checksum (stored {stored:#010x}, computed {computed:#010x})"
            )));
        }
        Ok(Superblock {
            state: get_u16(b, sb::STATE)?,
            block_size: get_u32(b, sb::BLOCK_SIZE)?,
            total_blocks: get_u64(b, sb::TOTAL_BLOCKS)?,
            free_blocks: get_u64(b, sb::FREE_BLOCKS)?,
            inode_count: get_u64(b, sb::INODE_COUNT)?,
            free_inodes: get_u64(b, sb::FREE_INODES)?,
            root_inode: get_u64(b, sb::ROOT_INODE)?,
            block_bitmap_start: get_u64(b, sb::BLOCK_BITMAP_START)?,
            inode_bitmap_start: get_u64(b, sb::INODE_BITMAP_START)?,
            inode_table_start: get_u64(b, sb::INODE_TABLE_START)?,
            data_start: get_u64(b, sb::DATA_START)?,
            journal_start: get_u64(b, sb::JOURNAL_START)?,
            journal_blocks: get_u64(b, sb::JOURNAL_BLOCKS)?,
            uuid: get_bytes(b, sb::UUID)?,
            features_compat: get_u64(b, sb::FEATURES_COMPAT)?,
            features_incompat: get_u64(b, sb::FEATURES_INCOMPAT)?,
            label: get_bytes(b, sb::LABEL)?,
            created_ns: get_i64(b, sb::CREATED_NS)?,
            last_mount_ns: get_i64(b, sb::LAST_MOUNT_NS)?,
            mount_count: get_u32(b, sb::MOUNT_COUNT)?,
        })
    }

    /// Semantic checks: known incompatible features, valid block size, state
    /// and label, root inode, free counters, and every zone large enough,
    /// in order and inside the volume.
    pub fn validate(&self) -> Result<(), DadaError> {
        let unknown = self.features_incompat & !INCOMPAT_SUPPORTED;
        if unknown != 0 {
            return Err(DadaError::Unsupported(unknown));
        }
        if !is_valid_block_size(self.block_size) {
            return Err(corrupt(format!("invalid block size {}", self.block_size)));
        }
        if self.state != STATE_CLEAN && self.state != STATE_DIRTY {
            return Err(corrupt(format!("invalid state {}", self.state)));
        }
        self.label()?;
        if self.root_inode != INO_ROOT {
            return Err(corrupt(format!("root inode is {}", self.root_inode)));
        }
        if self.free_blocks > self.total_blocks {
            return Err(corrupt("free_blocks exceeds total_blocks"));
        }
        if self.free_inodes > self.inode_count {
            return Err(corrupt("free_inodes exceeds inode_count"));
        }
        self.validate_zones()
    }

    fn validate_zones(&self) -> Result<(), DadaError> {
        let bs = u64::from(self.block_size);
        let bits_per_block = bs * 8;
        let inodes_per_block = bs / u64::from(INODE_SIZE);
        let out_of_volume = || corrupt("zones overflow the volume");

        if self.total_blocks < 2 {
            return Err(corrupt("volume too small"));
        }
        if self.inode_count < MIN_INODE_COUNT || !self.inode_count.is_multiple_of(inodes_per_block)
        {
            return Err(corrupt(format!("invalid inode count {}", self.inode_count)));
        }
        if self.block_bitmap_start != BLOCK_BITMAP_START {
            return Err(corrupt("block bitmap does not start at block 1"));
        }

        let block_bitmap_end = self
            .block_bitmap_start
            .checked_add(self.total_blocks.div_ceil(bits_per_block))
            .ok_or_else(out_of_volume)?;
        if self.inode_bitmap_start < block_bitmap_end {
            return Err(corrupt("inode bitmap overlaps block bitmap"));
        }
        let inode_bitmap_end = self
            .inode_bitmap_start
            .checked_add(self.inode_count.div_ceil(bits_per_block))
            .ok_or_else(out_of_volume)?;
        if self.inode_table_start < inode_bitmap_end {
            return Err(corrupt("inode table overlaps inode bitmap"));
        }
        let inode_table_end = self
            .inode_table_start
            .checked_add(self.inode_count / inodes_per_block)
            .ok_or_else(out_of_volume)?;

        let has_journal = self.features_incompat & INCOMPAT_JOURNAL != 0;
        let metadata_end = if has_journal {
            if self.journal_blocks == 0 {
                return Err(corrupt("journal feature set but journal is empty"));
            }
            if self.journal_start < inode_table_end {
                return Err(corrupt("journal overlaps inode table"));
            }
            self.journal_start
                .checked_add(self.journal_blocks)
                .ok_or_else(out_of_volume)?
        } else {
            if self.journal_start != 0 || self.journal_blocks != 0 {
                return Err(corrupt("journal zone present without journal feature"));
            }
            inode_table_end
        };
        if self.data_start < metadata_end {
            return Err(corrupt("data zone overlaps metadata"));
        }
        // The last block holds the backup superblock.
        if self.data_start >= self.total_blocks - 1 {
            return Err(out_of_volume());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{INCOMPAT_EXTENT_BLOCKS, MAX_BLOCK_SIZE};
    use proptest::prelude::*;

    const MIB: u64 = 1024 * 1024;

    fn sample() -> Superblock {
        let opts = FormatOptions {
            label: "TEST".into(),
            ..FormatOptions::default()
        };
        let layout = Layout::compute(100 * MIB, &opts).unwrap();
        Superblock::from_layout(&layout, &opts, [7; UUID_LEN], 1_700_000_000_000_000_000).unwrap()
    }

    /// Re-encodes after patching raw bytes, with a valid checksum.
    fn patch(sb: &Superblock, f: impl FnOnce(&mut [u8; SUPERBLOCK_SIZE])) -> Vec<u8> {
        let mut b = sb.encode();
        f(&mut b);
        let crc = crc32c(&b[..sb::CHECKSUM]);
        b[sb::CHECKSUM..].copy_from_slice(&crc.to_le_bytes());
        b.to_vec()
    }

    fn rejected(sb: &Superblock) -> DadaError {
        sb.validate().unwrap_err()
    }

    #[test]
    fn round_trip() {
        let sb = sample();
        sb.validate().unwrap();
        let bytes = sb.encode();
        assert_eq!(&bytes[..4], b"DADA");
        assert_eq!(&bytes[4..6], &[1, 0]);
        assert_eq!(Superblock::decode(&bytes).unwrap(), sb);
        assert_eq!(sb.label().unwrap(), "TEST");
    }

    #[test]
    fn layout_fields() {
        let sb = sample();
        assert_eq!(sb.total_blocks, 25600);
        assert_eq!(sb.data_start, 659);
        assert_eq!(sb.free_blocks, 25600 - 659 - 1);
        assert_eq!(sb.free_inodes, 6400 - 16);
        assert_eq!(sb.features_incompat, INCOMPAT_JOURNAL);
    }

    #[test]
    fn decode_ignores_reserved_bytes_and_extra_input() {
        let sb = sample();
        let mut bytes = patch(&sb, |b| {
            b[12] = 0xFF;
            b[500] = 0xFF;
        });
        bytes.extend_from_slice(&[0xEE; 3072]);
        assert_eq!(Superblock::decode(&bytes).unwrap(), sb);
        // Reserved bytes are written back as zero.
        let re = Superblock::decode(&bytes).unwrap().encode();
        assert_eq!(re[12], 0);
        assert_eq!(re[500], 0);
    }

    #[test]
    fn rejects_bad_magic() {
        let bytes = patch(&sample(), |b| b[0] = b'X');
        let err = Superblock::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("magic"), "{err}");
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut bytes = sample().encode();
        bytes[sb::TOTAL_BLOCKS] ^= 1;
        let err = Superblock::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");

        let mut bytes = sample().encode();
        bytes[sb::CHECKSUM] ^= 1;
        assert!(Superblock::decode(&bytes).is_err());
    }

    #[test]
    fn rejects_unknown_version() {
        for v in [0u16, 2, u16::MAX] {
            let bytes = patch(&sample(), |b| b[4..6].copy_from_slice(&v.to_le_bytes()));
            let err = Superblock::decode(&bytes).unwrap_err();
            assert!(err.to_string().contains("version"), "{err}");
        }
    }

    #[test]
    fn rejects_truncated_input() {
        let bytes = sample().encode();
        assert!(Superblock::decode(&bytes[..SUPERBLOCK_SIZE - 1]).is_err());
        assert!(Superblock::decode(&[]).is_err());
    }

    #[test]
    fn rejects_unknown_incompat_bit() {
        let mut sb = sample();
        sb.features_incompat |= INCOMPAT_EXTENT_BLOCKS | INCOMPAT_CASEFOLD;
        sb.validate().unwrap();
        sb.features_incompat |= 1 << 3 | 1 << 63;
        assert!(matches!(
            rejected(&sb),
            DadaError::Unsupported(bits) if bits == (1 << 3 | 1 << 63)
        ));
    }

    #[test]
    fn unknown_compat_bits_are_accepted() {
        let mut sb = sample();
        sb.features_compat = u64::MAX;
        sb.validate().unwrap();
    }

    #[test]
    fn rejects_zones_outside_volume() {
        let mut sb = sample();
        sb.data_start = sb.total_blocks - 1;
        assert!(matches!(rejected(&sb), DadaError::Corrupt(_)));

        let mut sb = sample();
        sb.journal_start = u64::MAX;
        assert!(matches!(rejected(&sb), DadaError::Corrupt(_)));

        let mut sb = sample();
        sb.total_blocks = sb.data_start;
        sb.free_blocks = 0;
        assert!(matches!(rejected(&sb), DadaError::Corrupt(_)));

        let mut sb = sample();
        sb.inode_count = u64::MAX - u64::MAX % 16;
        sb.free_inodes = 0;
        assert!(matches!(rejected(&sb), DadaError::Corrupt(_)));
    }

    #[test]
    fn rejects_zones_out_of_order() {
        let base = sample();
        let cases: [fn(&mut Superblock); 7] = [
            |s| s.block_bitmap_start = 0,
            |s| s.inode_bitmap_start = 1,
            |s| s.inode_table_start = s.inode_bitmap_start,
            |s| s.journal_start = s.inode_table_start + 1,
            |s| s.data_start = s.journal_start,
            |s| s.journal_blocks = 0,
            |s| s.features_incompat &= !INCOMPAT_JOURNAL,
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut sb = base.clone();
            f(&mut sb);
            assert!(matches!(rejected(&sb), DadaError::Corrupt(_)), "case {i}");
        }
    }

    #[test]
    fn rejects_bad_fields() {
        let base = sample();
        let cases: [fn(&mut Superblock); 8] = [
            |s| s.block_size = 3000,
            |s| s.block_size = MAX_BLOCK_SIZE * 2,
            |s| s.state = 2,
            |s| s.root_inode = 2,
            |s| s.free_blocks = s.total_blocks + 1,
            |s| s.free_inodes = s.inode_count + 1,
            |s| s.inode_count = 6401,
            |s| s.label = [0xFF; LABEL_LEN],
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut sb = base.clone();
            f(&mut sb);
            assert!(matches!(rejected(&sb), DadaError::Corrupt(_)), "case {i}");
        }
    }

    #[test]
    fn labels() {
        let mut sb = sample();
        sb.set_label("").unwrap();
        assert_eq!(sb.label().unwrap(), "");
        let full = "é".repeat(16);
        sb.set_label(&full).unwrap();
        assert_eq!(sb.label().unwrap(), full);
        assert!(sb.set_label(&"x".repeat(33)).is_err());
        assert!(sb.set_label("a\0b").is_err());
        let opts = FormatOptions {
            label: "x".repeat(33),
            ..FormatOptions::default()
        };
        let layout = Layout::compute(100 * MIB, &opts).unwrap();
        assert!(Superblock::from_layout(&layout, &opts, [0; 16], 0).is_err());
    }

    #[test]
    fn layouts_produce_valid_superblocks() {
        for bs in [1024, 2048, 4096, 8192, 16384, 32768, 65536] {
            for size in [MIB, 100 * MIB, 10 * 1024 * MIB] {
                for journal in [false, true] {
                    let opts = FormatOptions {
                        block_size: bs,
                        journal,
                        casefold: true,
                        ..FormatOptions::default()
                    };
                    if let Ok(layout) = Layout::compute(size, &opts) {
                        let sb = Superblock::from_layout(&layout, &opts, [1; 16], 0).unwrap();
                        sb.validate().unwrap();
                        let decoded = Superblock::decode(&sb.encode()).unwrap();
                        assert_eq!(decoded, sb);
                    }
                }
            }
        }
    }

    prop_compose! {
        fn any_superblock()(
            state: u16,
            block_size: u32,
            counts: [u64; 4],
            zones: [u64; 7],
            uuid: [u8; UUID_LEN],
            features: [u64; 2],
            label: [u8; LABEL_LEN],
            dates: [i64; 2],
            mount_count: u32,
        ) -> Superblock {
            Superblock {
                state,
                block_size,
                total_blocks: counts[0],
                free_blocks: counts[1],
                inode_count: counts[2],
                free_inodes: counts[3],
                root_inode: zones[0],
                block_bitmap_start: zones[1],
                inode_bitmap_start: zones[2],
                inode_table_start: zones[3],
                data_start: zones[4],
                journal_start: zones[5],
                journal_blocks: zones[6],
                uuid,
                features_compat: features[0],
                features_incompat: features[1],
                label,
                created_ns: dates[0],
                last_mount_ns: dates[1],
                mount_count,
            }
        }
    }

    proptest! {
        #[test]
        fn encode_decode_round_trip(sb in any_superblock()) {
            let bytes = sb.encode();
            prop_assert_eq!(Superblock::decode(&bytes).unwrap(), sb.clone());
            // Validation never panics, whatever the field values.
            let _ = sb.validate();
        }

        #[test]
        fn decode_arbitrary_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            if let Ok(sb) = Superblock::decode(&bytes) {
                let _ = sb.validate();
            }
        }

        #[test]
        fn single_bit_flip_is_detected(byte in 0usize..SUPERBLOCK_SIZE, bit in 0u8..8) {
            let mut bytes = sample().encode();
            bytes[byte] ^= 1 << bit;
            prop_assert!(Superblock::decode(&bytes).is_err());
        }
    }
}
