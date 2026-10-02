//! Block devices: the only way libdada accesses storage.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::format::{
    is_valid_block_size, DEFAULT_BLOCK_SIZE, MAX_BLOCK_SIZE, MIN_BLOCK_SIZE, SUPERBLOCK_SIZE,
};
use crate::superblock::Superblock;
use crate::DadaError;

/// Storage addressed by fixed-size blocks.
///
/// `buf` must be exactly `block_size()` bytes long and `lba` lower than
/// `block_count()`; otherwise the call fails with `DadaError::Invalid`.
pub trait BlockDevice: Send {
    fn block_size(&self) -> u32;
    fn block_count(&self) -> u64;
    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DadaError>;
    fn write_block(&mut self, lba: u64, buf: &[u8]) -> Result<(), DadaError>;
    fn flush(&mut self) -> Result<(), DadaError>;
}

/// Byte offset of block `lba`, after checking the request against the device geometry.
fn block_offset(
    block_size: u32,
    block_count: u64,
    lba: u64,
    buf_len: usize,
) -> Result<u64, DadaError> {
    if buf_len != block_size as usize || lba >= block_count {
        return Err(DadaError::Invalid);
    }
    lba.checked_mul(u64::from(block_size))
        .ok_or(DadaError::Invalid)
}

/// In-memory device, for tests.
#[derive(Debug, Clone)]
pub struct MemDevice {
    block_size: u32,
    data: Vec<u8>,
}

impl MemDevice {
    /// A zero-filled device of `block_count` blocks.
    pub fn new(block_size: u32, block_count: u64) -> Result<Self, DadaError> {
        let len = block_count
            .checked_mul(u64::from(block_size))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(DadaError::Invalid)?;
        Self::from_bytes(block_size, vec![0; len])
    }

    /// A device over existing bytes; a trailing partial block is ignored.
    pub fn from_bytes(block_size: u32, data: Vec<u8>) -> Result<Self, DadaError> {
        if !is_valid_block_size(block_size) {
            return Err(DadaError::Invalid);
        }
        Ok(MemDevice { block_size, data })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    fn range(&self, lba: u64, len: usize) -> Result<std::ops::Range<usize>, DadaError> {
        let start = block_offset(self.block_size, self.block_count(), lba, len)?;
        let start = usize::try_from(start).map_err(|_| DadaError::Invalid)?;
        let end = start.checked_add(len).ok_or(DadaError::Invalid)?;
        Ok(start..end)
    }
}

impl BlockDevice for MemDevice {
    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        (self.data.len() / self.block_size as usize) as u64
    }

    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DadaError> {
        let range = self.range(lba, buf.len())?;
        let src = self.data.get(range).ok_or(DadaError::Invalid)?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write_block(&mut self, lba: u64, buf: &[u8]) -> Result<(), DadaError> {
        let range = self.range(lba, buf.len())?;
        let dst = self.data.get_mut(range).ok_or(DadaError::Invalid)?;
        dst.copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DadaError> {
        Ok(())
    }
}

/// Device backed by a regular image file.
#[derive(Debug)]
pub struct FileDevice {
    file: File,
    block_size: u32,
    block_count: u64,
    writable: bool,
}

impl FileDevice {
    /// Opens an image file. Its size is rounded down to a whole number of blocks.
    pub fn open(path: &Path, block_size: u32, writable: bool) -> Result<Self, DadaError> {
        let file = OpenOptions::new().read(true).write(writable).open(path)?;
        Self::from_file(file, block_size, writable)
    }

    /// Opens an existing dada image, using the block size recorded in its
    /// primary superblock, or in its backup superblock if the primary is
    /// unreadable. Falls back to the default block size when neither is
    /// found, so that `Volume::open` reports the actual problem.
    pub fn open_image(path: &Path, writable: bool) -> Result<Self, DadaError> {
        let mut file = OpenOptions::new().read(true).write(writable).open(path)?;
        let block_size = detect_block_size(&mut file).unwrap_or(DEFAULT_BLOCK_SIZE);
        Self::from_file(file, block_size, writable)
    }

    /// Wraps an already opened file; `writable` must match how it was opened.
    pub fn from_file(mut file: File, block_size: u32, writable: bool) -> Result<Self, DadaError> {
        if !is_valid_block_size(block_size) {
            return Err(DadaError::Invalid);
        }
        // Seeking to the end also gives the size of a block device.
        let block_count = file.seek(SeekFrom::End(0))? / u64::from(block_size);
        Ok(FileDevice {
            file,
            block_size,
            block_count,
            writable,
        })
    }
}

fn read_superblock_at(file: &mut File, offset: u64) -> Option<Superblock> {
    let mut buf = [0u8; SUPERBLOCK_SIZE];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut buf).ok()?;
    Superblock::decode(&buf).ok()
}

fn detect_block_size(file: &mut File) -> Option<u32> {
    if let Some(sb) = read_superblock_at(file, 0) {
        if is_valid_block_size(sb.block_size) {
            return Some(sb.block_size);
        }
    }
    let len = file.seek(SeekFrom::End(0)).ok()?;
    (MIN_BLOCK_SIZE.trailing_zeros()..=MAX_BLOCK_SIZE.trailing_zeros())
        .map(|shift| 1u32 << shift)
        .find(|&bs| {
            let blocks = len / u64::from(bs);
            blocks >= 2
                && read_superblock_at(file, (blocks - 1) * u64::from(bs))
                    .is_some_and(|sb| sb.block_size == bs)
        })
}

impl BlockDevice for FileDevice {
    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DadaError> {
        let off = block_offset(self.block_size, self.block_count, lba, buf.len())?;
        self.file.seek(SeekFrom::Start(off))?;
        self.file.read_exact(buf)?;
        Ok(())
    }

    fn write_block(&mut self, lba: u64, buf: &[u8]) -> Result<(), DadaError> {
        if !self.writable {
            return Err(DadaError::ReadOnly);
        }
        let off = block_offset(self.block_size, self.block_count, lba, buf.len())?;
        self.file.seek(SeekFrom::Start(off))?;
        self.file.write_all(buf)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DadaError> {
        if self.writable {
            self.file.sync_data()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exercise(dev: &mut dyn BlockDevice) {
        let bs = dev.block_size() as usize;
        let last = dev.block_count() - 1;
        let pattern: Vec<u8> = (0..bs).map(|i| (i % 251) as u8).collect();
        dev.write_block(last, &pattern).unwrap();
        dev.write_block(0, &vec![0xAA; bs]).unwrap();
        dev.flush().unwrap();

        let mut buf = vec![0; bs];
        dev.read_block(last, &mut buf).unwrap();
        assert_eq!(buf, pattern);
        dev.read_block(0, &mut buf).unwrap();
        assert_eq!(buf, vec![0xAA; bs]);
        dev.read_block(1, &mut buf).unwrap();
        assert_eq!(buf, vec![0; bs]);

        // Out of range or wrong buffer size.
        assert!(matches!(
            dev.read_block(last + 1, &mut buf),
            Err(DadaError::Invalid)
        ));
        assert!(matches!(
            dev.read_block(u64::MAX, &mut buf),
            Err(DadaError::Invalid)
        ));
        assert!(matches!(
            dev.write_block(0, &buf[1..]),
            Err(DadaError::Invalid)
        ));
        let mut big = vec![0; bs + 1];
        assert!(matches!(
            dev.read_block(0, &mut big),
            Err(DadaError::Invalid)
        ));
    }

    #[test]
    fn mem_device() {
        let mut dev = MemDevice::new(1024, 8).unwrap();
        assert_eq!(dev.block_count(), 8);
        exercise(&mut dev);
        assert_eq!(dev.as_bytes().len(), 8 * 1024);
    }

    #[test]
    fn mem_device_rejects_bad_geometry() {
        assert!(MemDevice::new(1000, 8).is_err());
        assert!(MemDevice::new(4096, u64::MAX).is_err());
        let dev = MemDevice::from_bytes(1024, vec![0; 2500]).unwrap();
        assert_eq!(dev.block_count(), 2);
    }

    #[test]
    fn file_device() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.img");
        File::create(&path)
            .unwrap()
            .set_len(16 * 4096 + 100)
            .unwrap();

        let mut dev = FileDevice::open(&path, 4096, true).unwrap();
        assert_eq!(dev.block_count(), 16);
        exercise(&mut dev);
        drop(dev);

        let mut ro = FileDevice::open(&path, 4096, false).unwrap();
        let mut buf = vec![0; 4096];
        ro.read_block(0, &mut buf).unwrap();
        assert_eq!(buf, vec![0xAA; 4096]);
        assert!(matches!(ro.write_block(0, &buf), Err(DadaError::ReadOnly)));
        ro.flush().unwrap();
    }

    #[test]
    fn file_device_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = FileDevice::open(&dir.path().join("absent.img"), 4096, false).unwrap_err();
        assert!(matches!(err, DadaError::Io(_)));
    }

    #[test]
    fn open_image_detects_block_size() {
        let dir = tempfile::tempdir().unwrap();
        for bs in [1024, 4096, 65536] {
            let path = dir.path().join(format!("{bs}.img"));
            File::create(&path).unwrap().set_len(8 << 20).unwrap();
            let mut dev = FileDevice::open(&path, bs, true).unwrap();
            let opts = crate::FormatOptions {
                block_size: bs,
                journal: false,
                ..Default::default()
            };
            crate::format(&mut dev, &opts).unwrap();
            drop(dev);
            assert_eq!(
                FileDevice::open_image(&path, false).unwrap().block_size(),
                bs
            );

            // Primary superblock destroyed: the backup gives the block size.
            let mut dev = FileDevice::open(&path, bs, true).unwrap();
            dev.write_block(0, &vec![0; bs as usize]).unwrap();
            drop(dev);
            assert_eq!(
                FileDevice::open_image(&path, false).unwrap().block_size(),
                bs
            );
        }

        // Not a dada image: default block size.
        let path = dir.path().join("zero.img");
        File::create(&path).unwrap().set_len(1 << 20).unwrap();
        let dev = FileDevice::open_image(&path, false).unwrap();
        assert_eq!(dev.block_size(), DEFAULT_BLOCK_SIZE);
    }
}
