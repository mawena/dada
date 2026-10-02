//! Write-back LRU cache of metadata blocks.

use std::collections::{BTreeMap, HashMap};

use crate::device::BlockDevice;
use crate::DadaError;

pub const DEFAULT_CACHE_BLOCKS: usize = 1024;

struct Entry {
    data: Vec<u8>,
    dirty: bool,
    tick: u64,
}

/// Caches whole blocks. Dirty blocks reach the device when evicted or on `flush`.
pub struct BlockCache {
    capacity: usize,
    tick: u64,
    entries: HashMap<u64, Entry>,
    /// Least recently used first: tick -> block.
    lru: BTreeMap<u64, u64>,
}

impl BlockCache {
    /// A cache of at most `capacity` blocks (at least one).
    pub fn new(capacity: usize) -> Self {
        BlockCache {
            capacity: capacity.max(1),
            tick: 0,
            entries: HashMap::new(),
            lru: BTreeMap::new(),
        }
    }

    fn touch(&mut self, lba: u64) {
        self.tick += 1;
        if let Some(entry) = self.entries.get_mut(&lba) {
            self.lru.remove(&entry.tick);
            entry.tick = self.tick;
            self.lru.insert(self.tick, lba);
        }
    }

    fn insert<D: BlockDevice>(
        &mut self,
        dev: &mut D,
        lba: u64,
        data: Vec<u8>,
        dirty: bool,
    ) -> Result<(), DadaError> {
        self.tick += 1;
        if let Some(old) = self.entries.insert(
            lba,
            Entry {
                data,
                dirty,
                tick: self.tick,
            },
        ) {
            self.lru.remove(&old.tick);
        }
        self.lru.insert(self.tick, lba);
        while self.entries.len() > self.capacity {
            let Some((_, victim)) = self.lru.pop_first() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                if entry.dirty {
                    dev.write_block(victim, &entry.data)?;
                }
            }
        }
        Ok(())
    }

    /// Contents of block `lba`, read from the device on a miss.
    pub fn read<D: BlockDevice>(&mut self, dev: &mut D, lba: u64) -> Result<Vec<u8>, DadaError> {
        if let Some(entry) = self.entries.get(&lba) {
            let data = entry.data.clone();
            self.touch(lba);
            return Ok(data);
        }
        let mut data = vec![0u8; dev.block_size() as usize];
        dev.read_block(lba, &mut data)?;
        self.insert(dev, lba, data.clone(), false)?;
        Ok(data)
    }

    /// Replaces block `lba`; it is written to the device later.
    pub fn write<D: BlockDevice>(
        &mut self,
        dev: &mut D,
        lba: u64,
        data: Vec<u8>,
    ) -> Result<(), DadaError> {
        if data.len() != dev.block_size() as usize || lba >= dev.block_count() {
            return Err(DadaError::Invalid);
        }
        self.insert(dev, lba, data, true)
    }

    /// Forgets block `lba` without writing it (the block was freed).
    pub fn discard(&mut self, lba: u64) {
        if let Some(entry) = self.entries.remove(&lba) {
            self.lru.remove(&entry.tick);
        }
    }

    /// Writes every dirty block, in block order. Does not flush the device.
    pub fn write_back<D: BlockDevice>(&mut self, dev: &mut D) -> Result<(), DadaError> {
        let mut dirty: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(&lba, _)| lba)
            .collect();
        dirty.sort_unstable();
        for lba in dirty {
            if let Some(entry) = self.entries.get_mut(&lba) {
                dev.write_block(lba, &entry.data)?;
                entry.dirty = false;
            }
        }
        Ok(())
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
    fn write_back_is_deferred() {
        let mut dev = MemDevice::new(1024, 16).unwrap();
        let mut cache = BlockCache::new(4);
        cache.write(&mut dev, 3, block(7)).unwrap();
        assert_eq!(cache.read(&mut dev, 3).unwrap(), block(7));
        assert_eq!(dev.as_bytes()[3 * 1024], 0, "not written yet");
        cache.write_back(&mut dev).unwrap();
        assert_eq!(dev.as_bytes()[3 * 1024], 7);
    }

    #[test]
    fn eviction_writes_dirty_blocks_lru_first() {
        let mut dev = MemDevice::new(1024, 16).unwrap();
        let mut cache = BlockCache::new(2);
        cache.write(&mut dev, 1, block(1)).unwrap();
        cache.write(&mut dev, 2, block(2)).unwrap();
        cache.read(&mut dev, 1).unwrap(); // 2 becomes the least recently used
        cache.write(&mut dev, 3, block(3)).unwrap();
        assert_eq!(cache.len(), 2);
        assert_eq!(dev.as_bytes()[2 * 1024], 2, "evicted and written");
        assert_eq!(dev.as_bytes()[1024], 0, "still cached");
        // A clean eviction writes nothing.
        cache.write_back(&mut dev).unwrap();
        dev.write_block(1, &block(9)).unwrap();
        cache.read(&mut dev, 5).unwrap();
        cache.read(&mut dev, 6).unwrap();
        assert_eq!(dev.as_bytes()[1024], 9);
    }

    #[test]
    fn discard_drops_pending_writes() {
        let mut dev = MemDevice::new(1024, 16).unwrap();
        let mut cache = BlockCache::new(4);
        cache.write(&mut dev, 4, block(4)).unwrap();
        cache.discard(4);
        cache.write_back(&mut dev).unwrap();
        assert_eq!(dev.as_bytes()[4 * 1024], 0);
        assert!(cache.is_empty());
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
