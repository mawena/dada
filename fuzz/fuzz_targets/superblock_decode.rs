#![no_main]

use libdada::superblock::Superblock;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(sb) = Superblock::decode(data) {
        let _ = sb.validate();
        let _ = sb.label();
        // Whatever decodes must encode back to the same fields.
        assert_eq!(Superblock::decode(&sb.encode()).ok(), Some(sb));
    }
});
