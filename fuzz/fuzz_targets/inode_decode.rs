#![no_main]

use libdada::inode::Inode;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first 8 bytes choose the inode number, the rest is the slot.
    let Some((n, slot)) = data.split_first_chunk::<8>() else {
        return;
    };
    let n = u64::from_le_bytes(*n);
    if let Ok(inode) = Inode::decode(n, slot) {
        let _ = inode.kind();
        if !inode.is_free() {
            assert_eq!(Inode::decode(n, &inode.encode(n)).ok(), Some(inode));
        }
    }
});
