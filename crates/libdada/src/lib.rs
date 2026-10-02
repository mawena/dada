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

pub mod error;
pub mod format;

pub use error::DadaError;
pub use format::FORMAT_VERSION;
