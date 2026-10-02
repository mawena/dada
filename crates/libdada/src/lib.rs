//! libdada: OS-independent core of the dada portable filesystem.
//!
//! All storage access goes through the `BlockDevice` trait; this crate
//! contains no OS-specific code.

#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod crc;
pub mod device;
pub mod error;
pub mod format;
pub mod layout;
mod le;
pub mod superblock;

pub use device::{BlockDevice, FileDevice, MemDevice};
pub use error::DadaError;
pub use format::FORMAT_VERSION;

/// Parameters of `format`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatOptions {
    pub block_size: u32,
    /// One inode per `inode_ratio` bytes of volume.
    pub inode_ratio: u64,
    /// At most `format::LABEL_LEN` bytes of UTF-8.
    pub label: String,
    pub casefold: bool,
    pub journal: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        FormatOptions {
            block_size: format::DEFAULT_BLOCK_SIZE,
            inode_ratio: format::DEFAULT_INODE_RATIO,
            label: String::new(),
            casefold: false,
            journal: true,
        }
    }
}
