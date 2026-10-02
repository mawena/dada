//! libdada: OS-independent core of the dada portable filesystem.
//!
//! All storage access goes through the `BlockDevice` trait; this crate
//! contains no OS-specific code.

#![forbid(unsafe_code)]

/// Version of the on-disk format implemented by this crate.
pub const FORMAT_VERSION: u16 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_version_is_one() {
        assert_eq!(FORMAT_VERSION, 1);
    }
}
