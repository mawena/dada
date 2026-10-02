//! Write-back LRU cache of metadata blocks.
//!
//! Dirty blocks are never evicted: they stay in the cache until the volume
//! commits them (through the journal when there is one). Only clean blocks
//! are evicted, least recently used first.

use std::collections::{BTreeMap, HashMap};

use crate::device::BlockDevice;
use crate::DadaError;

pub const DEFAULT_CACHE_BLOCKS: usize = 1024;

struct Entry {
    data: Vec<u8>,
    dirty: bool,
    /// Position in the LRU list; only clean entries are listed.
    tick: u64,
}

pub struct BlockCache {
    capacity: usize,
    tick: u64,
    entries: HashMap<u64, Entry>,
    /// Clean entries, least recently used first: tick -> block.
    lru: BTreeMap<u64, u64>,
    dirty: usize,
    /// Blocks shown instead of the device content (journal replayed in
    /// memory on a read-only volume).
    pinned: HashMap<u64, Vec<u8>>,
}

impl BlockCache {
    /// A cache holding at most `capacity` clean blocks (at least one).
    pub fn new(capacity: usize) -> Self {
        BlockCache {
            capacity: capacity.max(1),
            tick: 0,
            entries: HashMap::new(),
            lru: BTreeMap::new(),
            dirty: 0,
            pinned: HashMap::new(),
        }
    }

    /// Shows `data` as the content of block `lba` until it is written.
    pub fn pin(&mut self, lba: u64, data: Vec<u8>) {
        self.pinned.insert(lba, data);
    }

    fn insert(&mut self, lba: u64, data: Vec<u8>, dirty: bool) {
        self.tick += 1;
        if let Some(old) = self.entries.remove(&lba) {
            if old.dirty {
                self.dirty -= 1;
            } else {
                self.lru.remove(&old.tick);
            }
        }
        if dirty {
            self.dirty += 1;
        } else {
            self.lru.insert(self.tick, lba);
        }
        self.entries.insert(
            lba,
            Entry {
                data,
                dirty,
                tick: self.tick,
            },
        );
        while self.lru.len() > self.capacity {
            if let Some((_, victim)) = self.lru.pop_first() {
                self.entries.remove(&victim);
            }
        }
    }

    /// Contents of block `lba`, read from the device on a miss.
    pub fn read<D: BlockDevice>(&mut self, dev: &mut D, lba: u64) -> Result<Vec<u8>, DadaError> {
        if let Some(entry) = self.entries.get_mut(&lba) {
            let data = entry.data.clone();
            if !entry.dirty {
                self.lru.remove(&entry.tick);
                self.tick += 1;
                entry.tick = self.tick;
                self.lru.insert(self.tick, lba);
            }
            return Ok(data);
        }
        if let Some(data) = self.pinned.get(&lba) {
            return Ok(data.clone());
        }
        let mut data = vec![0u8; dev.block_size() as usize];
        dev.read_block(lba, &mut data)?;
        self.insert(lba, data.clone(), false);
        Ok(data)
    }

    /// Replaces block `lba`; it stays dirty until `mark_clean`.
    pub fn write<D: BlockDevice>(
        &mut self,
        dev: &mut D,
        lba: u64,
        data: Vec<u8>,
    ) -> Result<(), DadaError> {
        if data.len() != dev.block_size() as usize || lba >= dev.block_count() {
            return Err(DadaError::Invalid);
        }
        self.pinned.remove(&lba);
        self.insert(lba, data, true);
        Ok(())
    }

    /// Forgets block `lba` without writing it (the block was freed).
    pub fn discard(&mut self, lba: u64) {
        if let Some(entry) = self.entries.remove(&lba) {
            if entry.dirty {
                self.dirty -= 1;
            } else {
                self.lru.remove(&entry.tick);
            }
        }
    }

    pub fn dirty_count(&self) -> usize {
        self.dirty
    }

    /// Copies of the dirty blocks, in block order.
    pub fn dirty_blocks(&self) -> Vec<(u64, Vec<u8>)> {
        let mut out: Vec<(u64, Vec<u8>)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(&lba, e)| (lba, e.data.clone()))
            .collect();
        out.sort_unstable_by_key(|(lba, _)| *lba);
        out
    }

    /// Marks every dirty block clean, once it has reached the device.
    pub fn mark_clean(&mut self) {
        let dirty: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(&lba, _)| lba)
            .collect();
        for lba in dirty {
            if let Some(entry) = self.entries.remove(&lba) {
                self.dirty -= 1;
                self.insert(lba, entry.data, false);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemDevice;

    fn block(byte: u8) -> Vec<u8> {
        vec![byte; 1024]
    }

    #[test]
    fn dirty_blocks_wait_for_a_commit() {
        let mut dev = MemDevice::new(1024, 16).unwrap();
        let mut cache = BlockCache::new(2);
        for lba in 1..6 {
            cache.write(&mut dev, lba, block(lba as u8)).unwrap();
        }
        // Over capacity, but dirty blocks are kept and nothing was written.
        assert_eq!((cache.len(), cache.dirty_count()), (5, 5));
        assert!(dev.as_bytes().iter().all(|&b| b == 0));
        let dirty = cache.dirty_blocks();
        assert_eq!(
            dirty.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
        assert_eq!(cache.read(&mut dev, 3).unwrap(), block(3));

        // Once clean, they become evictable.
        cache.mark_clean();
        assert_eq!((cache.len(), cache.dirty_count()), (2, 0));
    }

    #[test]
    fn clean_blocks_are_evicted_lru_first() {
        let mut dev = MemDevice::new(1024, 16).unwrap();
        for lba in 1..4 {
            dev.write_block(lba, &block(lba as u8)).unwrap();
        }
        let mut cache = BlockCache::new(2);
        cache.read(&mut dev, 1).unwrap();
        cache.read(&mut dev, 2).unwrap();
        cache.read(&mut dev, 1).unwrap(); // 2 is now the least recently used
        cache.read(&mut dev, 3).unwrap();
        assert_eq!(cache.len(), 2);
        // 1 is still cached; 2 was evicted, so a device change is visible.
        dev.write_block(2, &block(9)).unwrap();
        dev.write_block(1, &block(9)).unwrap();
        assert_eq!(cache.read(&mut dev, 1).unwrap(), block(1));
        assert_eq!(cache.read(&mut dev, 2).unwrap(), block(9));
    }

    #[test]
    fn discard_and_pins() {
        let mut dev = MemDevice::new(1024, 16).unwrap();
        let mut cache = BlockCache::new(4);
        cache.write(&mut dev, 4, block(4)).unwrap();
        cache.discard(4);
        assert_eq!(cache.dirty_count(), 0);
        assert_eq!(cache.read(&mut dev, 4).unwrap(), block(0));

        cache.pin(7, block(7));
        assert_eq!(cache.read(&mut dev, 7).unwrap(), block(7));
        cache.write(&mut dev, 7, block(8)).unwrap();
        cache.mark_clean();
        assert_eq!(cache.read(&mut dev, 7).unwrap(), block(8));
    }

    #[test]
    fn rejects_bad_writes() {
        let mut dev = MemDevice::new(1024, 16).unwrap();
        let mut cache = BlockCache::new(4);
        assert!(cache.write(&mut dev, 16, block(1)).is_err());
        assert!(cache.write(&mut dev, 1, vec![0; 10]).is_err());
        assert!(cache.read(&mut dev, 99).is_err());
    }
}
