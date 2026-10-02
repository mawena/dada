//! CRC32C (Castagnoli) checksums used by all metadata structures.

/// CRC32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    ::crc32c::crc32c(data)
}

/// Continues a CRC32C computation: `crc32c_append(crc32c(a), b) == crc32c(a ‖ b)`.
pub fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    ::crc32c::crc32c_append(crc, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
    }

    #[test]
    fn append_matches_concatenation() {
        let (a, b) = (b"dada ".as_slice(), b"filesystem".as_slice());
        assert_eq!(crc32c_append(crc32c(a), b), crc32c(b"dada filesystem"));
    }
}
