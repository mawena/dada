#![no_main]

use libdada::dir::DirBlock;
use libdada::FileKind;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for size in [1024usize, 4096] {
        let mut block = vec![0u8; size];
        let n = data.len().min(size);
        block[..n].copy_from_slice(&data[..n]);
        // Reseal so that the parser gets past the checksum.
        let area = size - 8;
        let crc = libdada::crc::crc32c(&block[..area]);
        block[area..area + 4].copy_from_slice(&crc.to_le_bytes());
        if let Ok(mut dir) = DirBlock::parse(&block) {
            assert_eq!(DirBlock::parse(&dir.encode()).ok().as_ref(), Some(&dir));
            let first = dir.entries().next().map(|s| s.offset);
            let _ = dir.insert(99, FileKind::RegularFile, "fuzz");
            if let Some(offset) = first {
                let _ = dir.remove(offset);
            }
            assert!(DirBlock::parse(&dir.encode()).is_ok());
        }
    }
});
