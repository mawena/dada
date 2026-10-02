//! Metadata journal (SPEC 4.10).
//!
//! A transaction is a descriptor block listing target addresses, a copy of
//! each target block, and a commit block whose CRC covers the descriptor and
//! the copies. Transactions are written circularly over journal blocks
//! `1..journal_blocks`, block 0 holding the header.

use std::collections::HashMap;

use crate::crc::{crc32c, crc32c_append};
use crate::device::BlockDevice;
use crate::format::{
    jcommit, jdesc, jhdr, journal_descriptor_capacity, JOURNAL_COMMIT_MAGIC,
    JOURNAL_DESCRIPTOR_MAGIC, JOURNAL_HEADER_MAGIC, STATE_CLEAN,
};
use crate::le::{get_bytes, get_u32, get_u64, put_bytes, put_u32, put_u64};
use crate::superblock::Superblock;
use crate::DadaError;

fn corrupt(msg: impl Into<String>) -> DadaError {
    DadaError::Corrupt(format!("journal: {}", msg.into()))
}

/// Seals a block: CRC32C of bytes `0..bs-4` stored at `bs-4`.
fn seal(buf: &mut [u8]) {
    let end = buf.len() - 4;
    let crc = crc32c(buf.get(..end).unwrap_or_default());
    put_u32(buf, end, crc);
}

fn is_sealed(buf: &[u8]) -> bool {
    let Some(end) = buf.len().checked_sub(4) else {
        return false;
    };
    get_u32(buf, end).is_ok_and(|crc| crc == crc32c(buf.get(..end).unwrap_or_default()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Next transaction number.
    pub sequence: u64,
    /// Oldest transaction not yet applied (relative index, at least 1).
    pub head: u64,
    /// Where the next transaction goes.
    pub tail: u64,
}

impl Header {
    pub fn encode(&self, block_size: u32) -> Vec<u8> {
        let mut buf = vec![0u8; block_size as usize];
        put_bytes(&mut buf, jhdr::MAGIC, &JOURNAL_HEADER_MAGIC);
        put_u64(&mut buf, jhdr::SEQUENCE, self.sequence);
        put_u64(&mut buf, jhdr::HEAD, self.head);
        put_u64(&mut buf, jhdr::TAIL, self.tail);
        seal(&mut buf);
        buf
    }

    /// Decodes and checks the header of a journal of `blocks` blocks.
    pub fn decode(buf: &[u8], blocks: u64) -> Result<Self, DadaError> {
        if get_bytes::<4>(buf, jhdr::MAGIC)? != JOURNAL_HEADER_MAGIC {
            return Err(corrupt("bad header magic"));
        }
        if !is_sealed(buf) {
            return Err(corrupt("bad header checksum"));
        }
        let header = Header {
            sequence: get_u64(buf, jhdr::SEQUENCE)?,
            head: get_u64(buf, jhdr::HEAD)?,
            tail: get_u64(buf, jhdr::TAIL)?,
        };
        let valid = 1..blocks;
        if !valid.contains(&header.head) || !valid.contains(&header.tail) {
            return Err(corrupt("head or tail outside the journal"));
        }
        Ok(header)
    }
}

struct Descriptor {
    sequence: u64,
    targets: Vec<u64>,
}

impl Descriptor {
    fn encode(&self, block_size: u32) -> Vec<u8> {
        let mut buf = vec![0u8; block_size as usize];
        put_bytes(&mut buf, jdesc::MAGIC, &JOURNAL_DESCRIPTOR_MAGIC);
        put_u64(&mut buf, jdesc::SEQUENCE, self.sequence);
        put_u32(&mut buf, jdesc::COUNT, self.targets.len() as u32);
        for (i, t) in self.targets.iter().enumerate() {
            put_u64(&mut buf, jdesc::TARGETS + 8 * i, *t);
        }
        seal(&mut buf);
        buf
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        if get_bytes::<4>(buf, jdesc::MAGIC).ok()? != JOURNAL_DESCRIPTOR_MAGIC || !is_sealed(buf) {
            return None;
        }
        let count = get_u32(buf, jdesc::COUNT).ok()? as usize;
        if count == 0 || count > journal_descriptor_capacity(buf.len() as u32) {
            return None;
        }
        let targets = (0..count)
            .map(|i| get_u64(buf, jdesc::TARGETS + 8 * i))
            .collect::<Result<_, _>>()
            .ok()?;
        Some(Descriptor {
            sequence: get_u64(buf, jdesc::SEQUENCE).ok()?,
            targets,
        })
    }
}

fn encode_commit(sequence: u64, data_crc: u32, block_size: u32) -> Vec<u8> {
    let mut buf = vec![0u8; block_size as usize];
    put_bytes(&mut buf, jcommit::MAGIC, &JOURNAL_COMMIT_MAGIC);
    put_u64(&mut buf, jcommit::SEQUENCE, sequence);
    put_u32(&mut buf, jcommit::DATA_CRC, data_crc);
    seal(&mut buf);
    buf
}

/// `(sequence, data_crc)` of a valid commit block.
fn decode_commit(buf: &[u8]) -> Option<(u64, u32)> {
    if get_bytes::<4>(buf, jcommit::MAGIC).ok()? != JOURNAL_COMMIT_MAGIC || !is_sealed(buf) {
        return None;
    }
    Some((
        get_u64(buf, jcommit::SEQUENCE).ok()?,
        get_u32(buf, jcommit::DATA_CRC).ok()?,
    ))
}

/// Largest number of target blocks in one transaction: what a descriptor
/// holds, and at most `journal_blocks - 2` blocks in all.
pub fn max_targets(block_size: u32, journal_blocks: u64) -> usize {
    let by_size = usize::try_from(journal_blocks.saturating_sub(4)).unwrap_or(usize::MAX);
    journal_descriptor_capacity(block_size).min(by_size).max(1)
}

/// The journal of an opened volume.
pub struct Journal {
    start: u64,
    blocks: u64,
    block_size: u32,
    header: Header,
}

fn read<D: BlockDevice>(dev: &mut D, lba: u64) -> Result<Vec<u8>, DadaError> {
    let mut buf = vec![0u8; dev.block_size() as usize];
    dev.read_block(lba, &mut buf)?;
    Ok(buf)
}

/// Writes an empty journal: a fresh header and an invalid first slot, so
/// that nothing left in the zone can be replayed.
pub fn initialize<D: BlockDevice>(dev: &mut D, sb: &Superblock) -> Result<(), DadaError> {
    let header = Header {
        sequence: 1,
        head: 1,
        tail: 1,
    };
    dev.write_block(sb.journal_start, &header.encode(sb.block_size))?;
    dev.write_block(sb.journal_start + 1, &vec![0u8; sb.block_size as usize])
}

impl Journal {
    pub fn load<D: BlockDevice>(dev: &mut D, sb: &Superblock) -> Result<Self, DadaError> {
        let header = Header::decode(&read(dev, sb.journal_start)?, sb.journal_blocks)?;
        Ok(Journal {
            start: sb.journal_start,
            blocks: sb.journal_blocks,
            block_size: sb.block_size,
            header,
        })
    }

    fn next(&self, pos: u64) -> u64 {
        if pos + 1 >= self.blocks {
            1
        } else {
            pos + 1
        }
    }

    /// Writes `blocks` through the journal: transactions, then the blocks in
    /// place, then the header. Lists longer than one transaction holds are
    /// split.
    pub fn commit<D: BlockDevice>(
        &mut self,
        dev: &mut D,
        blocks: &[(u64, Vec<u8>)],
    ) -> Result<(), DadaError> {
        for chunk in blocks.chunks(max_targets(self.block_size, self.blocks)) {
            let sequence = self.header.sequence;
            let mut pos = self.header.tail;
            let descriptor = Descriptor {
                sequence,
                targets: chunk.iter().map(|(lba, _)| *lba).collect(),
            }
            .encode(self.block_size);
            let mut crc = crc32c(&descriptor);
            dev.write_block(self.start + pos, &descriptor)?;
            for (_, data) in chunk {
                pos = self.next(pos);
                crc = crc32c_append(crc, data);
                dev.write_block(self.start + pos, data)?;
            }
            dev.flush()?;
            pos = self.next(pos);
            dev.write_block(
                self.start + pos,
                &encode_commit(sequence, crc, self.block_size),
            )?;
            dev.flush()?;
            for (lba, data) in chunk {
                dev.write_block(*lba, data)?;
            }
            dev.flush()?;
            let after = self.next(pos);
            self.header = Header {
                sequence: sequence + 1,
                head: after,
                tail: after,
            };
            dev.write_block(self.start, &self.header.encode(self.block_size))?;
            dev.flush()?;
        }
        Ok(())
    }
}

/// Committed transactions found in the journal of a dirty volume.
pub struct Replay {
    /// Blocks to write, in order (a later copy of a block wins).
    pub blocks: Vec<(u64, Vec<u8>)>,
    pub transactions: u64,
    end: u64,
    next_sequence: u64,
}

impl Replay {
    /// Final content of every replayed block.
    pub fn latest(&self) -> HashMap<u64, Vec<u8>> {
        self.blocks.iter().cloned().collect()
    }
}

/// Reads the committed transactions from the head of the journal, stopping
/// at the first incomplete or invalid one.
pub fn scan<D: BlockDevice>(dev: &mut D, sb: &Superblock) -> Result<Replay, DadaError> {
    let journal = Journal::load(dev, sb)?;
    let mut pos = journal.header.head;
    let mut sequence = journal.header.sequence;
    let mut blocks = Vec::new();
    let mut transactions = 0;
    let mut scanned = 0u64;
    while let Some(descriptor) = Descriptor::decode(&read(dev, journal.start + pos)?) {
        let size = descriptor.targets.len() as u64 + 2;
        let journal_zone = sb.journal_start..sb.journal_start + sb.journal_blocks;
        let targets_ok = descriptor
            .targets
            .iter()
            .all(|t| *t < sb.total_blocks && !journal_zone.contains(t));
        if descriptor.sequence != sequence || !targets_ok || scanned + size > journal.blocks {
            break;
        }
        let mut crc = crc32c(&read(dev, journal.start + pos)?);
        let mut copies = Vec::with_capacity(descriptor.targets.len());
        let mut p = pos;
        for target in &descriptor.targets {
            p = journal.next(p);
            let data = read(dev, journal.start + p)?;
            crc = crc32c_append(crc, &data);
            copies.push((*target, data));
        }
        p = journal.next(p);
        match decode_commit(&read(dev, journal.start + p)?) {
            Some((s, c)) if s == sequence && c == crc => {}
            _ => break,
        }
        blocks.extend(copies);
        transactions += 1;
        scanned += size;
        sequence += 1;
        pos = journal.next(p);
    }
    Ok(Replay {
        blocks,
        transactions,
        end: pos,
        next_sequence: sequence,
    })
}

/// Writes the replayed blocks in place, empties the journal and marks the
/// volume clean. Returns the superblock as it now is on disk.
pub fn apply<D: BlockDevice>(
    dev: &mut D,
    sb: &Superblock,
    replay: &Replay,
) -> Result<Superblock, DadaError> {
    for (lba, data) in &replay.blocks {
        dev.write_block(*lba, data)?;
    }
    dev.flush()?;
    let header = Header {
        sequence: replay.next_sequence,
        head: replay.end,
        tail: replay.end,
    };
    dev.write_block(sb.journal_start, &header.encode(sb.block_size))?;
    dev.flush()?;
    let mut current = Superblock::decode(&read(dev, 0)?)?;
    current.validate()?;
    current.state = STATE_CLEAN;
    let mut block = vec![0u8; sb.block_size as usize];
    put_bytes(&mut block, 0, &current.encode());
    dev.write_block(0, &block)?;
    dev.flush()?;
    Ok(current)
}

/// A device seen with replayed blocks on top, for read-only checks of a
/// dirty volume. Writes go to the inner device.
pub struct Overlay<D> {
    inner: D,
    blocks: HashMap<u64, Vec<u8>>,
}

impl<D: BlockDevice> Overlay<D> {
    pub fn new(inner: D, blocks: HashMap<u64, Vec<u8>>) -> Self {
        Overlay { inner, blocks }
    }

    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: BlockDevice> BlockDevice for Overlay<D> {
    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }

    fn block_count(&self) -> u64 {
        self.inner.block_count()
    }

    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DadaError> {
        match self.blocks.get(&lba) {
            Some(data) if data.len() == buf.len() => {
                buf.copy_from_slice(data);
                Ok(())
            }
            _ => self.inner.read_block(lba, buf),
        }
    }

    fn write_block(&mut self, lba: u64, buf: &[u8]) -> Result<(), DadaError> {
        self.blocks.remove(&lba);
        self.inner.write_block(lba, buf)
    }

    fn flush(&mut self) -> Result<(), DadaError> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Layout;
    use crate::{FormatOptions, MemDevice};

    fn setup() -> (MemDevice, Superblock) {
        let opts = FormatOptions {
            block_size: 1024,
            ..FormatOptions::default()
        };
        let mut dev = MemDevice::new(1024, 4096).unwrap();
        let layout = Layout::compute(4096 * 1024, &opts).unwrap();
        let sb = Superblock::from_layout(&layout, &opts, [1; 16], 0).unwrap();
        dev.write_block(0, &sb.encode()).unwrap();
        initialize(&mut dev, &sb).unwrap();
        (dev, sb)
    }

    fn block(byte: u8) -> Vec<u8> {
        vec![byte; 1024]
    }

    #[test]
    fn header_round_trip_and_checks() {
        let h = Header {
            sequence: 9,
            head: 3,
            tail: 7,
        };
        assert_eq!(Header::decode(&h.encode(1024), 256).unwrap(), h);
        assert!(Header::decode(&h.encode(1024), 5).is_err(), "tail outside");
        let mut bad = h.encode(1024);
        bad[9] ^= 1;
        assert!(Header::decode(&bad, 256).is_err());
        assert!(Header::decode(&[0; 1024], 256).is_err());
    }

    #[test]
    fn committed_transactions_are_replayed_in_order() {
        let (mut dev, sb) = setup();
        let target = sb.data_start + 5;
        let mut journal = Journal::load(&mut dev, &sb).unwrap();
        journal.commit(&mut dev, &[(target, block(1))]).unwrap();
        // Checkpointed: the block is in place and the journal is empty again.
        assert_eq!(read(&mut dev, target).unwrap(), block(1));
        assert_eq!(scan(&mut dev, &sb).unwrap().transactions, 0);

        // Simulate a crash after the commit block: rewind the header.
        let before = journal.header;
        journal
            .commit(&mut dev, &[(target, block(2)), (target + 1, block(3))])
            .unwrap();
        journal.commit(&mut dev, &[(target, block(4))]).unwrap();
        dev.write_block(target, &block(0)).unwrap();
        dev.write_block(sb.journal_start, &before.encode(1024))
            .unwrap();
        let replay = scan(&mut dev, &sb).unwrap();
        assert_eq!(replay.transactions, 2);
        assert_eq!(replay.latest()[&target], block(4));
        apply(&mut dev, &sb, &replay).unwrap();
        assert_eq!(read(&mut dev, target).unwrap(), block(4));
        assert_eq!(read(&mut dev, target + 1).unwrap(), block(3));
        assert_eq!(scan(&mut dev, &sb).unwrap().transactions, 0);
    }

    #[test]
    fn incomplete_transactions_are_ignored() {
        let (mut dev, sb) = setup();
        let target = sb.data_start;
        let mut journal = Journal::load(&mut dev, &sb).unwrap();
        let before = journal.header;
        journal.commit(&mut dev, &[(target, block(7))]).unwrap();
        dev.write_block(sb.journal_start, &before.encode(1024))
            .unwrap();
        // Damage the copy: the commit CRC no longer matches.
        dev.write_block(sb.journal_start + 2, &block(8)).unwrap();
        assert_eq!(scan(&mut dev, &sb).unwrap().transactions, 0);
        // Missing commit block.
        dev.write_block(sb.journal_start + 2, &block(7)).unwrap();
        dev.write_block(sb.journal_start + 3, &block(0)).unwrap();
        assert_eq!(scan(&mut dev, &sb).unwrap().transactions, 0);
    }

    #[test]
    fn transactions_wrap_around_the_journal() {
        let (mut dev, sb) = setup();
        let mut journal = Journal::load(&mut dev, &sb).unwrap();
        let blocks: Vec<(u64, Vec<u8>)> = (0..100)
            .map(|i| (sb.data_start + i, block(i as u8)))
            .collect();
        // 256-block journal, 102 blocks per transaction: the third one wraps.
        for _ in 0..3 {
            journal.commit(&mut dev, &blocks).unwrap();
        }
        assert!(journal.header.tail < 102);
        // Replaying the last transaction from its start.
        let rewound = Header {
            sequence: journal.header.sequence - 1,
            head: 205,
            tail: 205,
        };
        dev.write_block(sb.journal_start, &rewound.encode(1024))
            .unwrap();
        let replay = scan(&mut dev, &sb).unwrap();
        assert_eq!(replay.transactions, 1);
        assert_eq!(replay.blocks, blocks);
    }

    #[test]
    fn large_lists_are_split() {
        let (mut dev, sb) = setup();
        let mut journal = Journal::load(&mut dev, &sb).unwrap();
        let n = max_targets(1024, sb.journal_blocks) * 2 + 3;
        let blocks: Vec<(u64, Vec<u8>)> = (0..n as u64)
            .map(|i| (sb.data_start + i, block(i as u8)))
            .collect();
        let first = journal.header.sequence;
        journal.commit(&mut dev, &blocks).unwrap();
        assert_eq!(journal.header.sequence, first + 3);
        for (lba, data) in &blocks {
            assert_eq!(&read(&mut dev, *lba).unwrap(), data);
        }
    }

    #[test]
    fn overlay_reads_replayed_blocks() {
        let mut dev = MemDevice::new(1024, 8).unwrap();
        dev.write_block(2, &block(1)).unwrap();
        let mut over = Overlay::new(dev, HashMap::from([(2, block(9))]));
        let mut buf = vec![0; 1024];
        over.read_block(2, &mut buf).unwrap();
        assert_eq!(buf, block(9));
        over.write_block(2, &block(5)).unwrap();
        over.read_block(2, &mut buf).unwrap();
        assert_eq!(buf, block(5));
    }
}
