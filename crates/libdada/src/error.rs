//! Error type of libdada.

/// Every failure of libdada, including corrupted or malicious images.
#[derive(Debug, thiserror::Error)]
pub enum DadaError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    Exists,
    #[error("not a directory")]
    NotDir,
    #[error("is a directory")]
    IsDir,
    #[error("directory not empty")]
    NotEmpty,
    #[error("no space left")]
    NoSpace,
    #[error("no free inode")]
    NoInodes,
    #[error("invalid name")]
    InvalidName,
    #[error("name too long")]
    NameTooLong,
    #[error("invalid argument")]
    Invalid,
    #[error("read-only volume")]
    ReadOnly,
    #[error("unsupported feature: {0:#x}")]
    Unsupported(u64),
    #[error("corruption: {0}")]
    Corrupt(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

impl DadaError {
    /// Errno value for this error, for the platform libdada is compiled for.
    pub fn to_errno(&self) -> i32 {
        use errno::*;
        match self {
            DadaError::NotFound => ENOENT,
            DadaError::Exists => EEXIST,
            DadaError::NotDir => ENOTDIR,
            DadaError::IsDir => EISDIR,
            DadaError::NotEmpty => ENOTEMPTY,
            DadaError::NoSpace | DadaError::NoInodes => ENOSPC,
            DadaError::InvalidName | DadaError::Invalid => EINVAL,
            DadaError::NameTooLong => ENAMETOOLONG,
            DadaError::ReadOnly => EROFS,
            DadaError::Unsupported(_) => ENOTSUP,
            DadaError::Corrupt(_) => EIO,
            DadaError::Io(e) => io_errno(e),
        }
    }
}

#[cfg(unix)]
fn io_errno(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(errno::EIO)
}

#[cfg(not(unix))]
fn io_errno(_e: &std::io::Error) -> i32 {
    // Raw OS errors are Win32 codes here, not errno values.
    errno::EIO
}

/// Errno values. They are identical on every supported platform except
/// ENAMETOOLONG, ENOTEMPTY and ENOTSUP.
mod errno {
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EEXIST: i32 = 17;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EINVAL: i32 = 22;
    pub const ENOSPC: i32 = 28;
    pub const EROFS: i32 = 30;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod os {
        pub const ENAMETOOLONG: i32 = 36;
        pub const ENOTEMPTY: i32 = 39;
        pub const ENOTSUP: i32 = 95;
    }

    #[cfg(windows)]
    mod os {
        pub const ENAMETOOLONG: i32 = 38;
        pub const ENOTEMPTY: i32 = 41;
        pub const ENOTSUP: i32 = 129;
    }

    // macOS and the BSDs.
    #[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
    mod os {
        pub const ENAMETOOLONG: i32 = 63;
        pub const ENOTEMPTY: i32 = 66;
        pub const ENOTSUP: i32 = 45;
    }

    pub use os::*;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_values() {
        assert_eq!(DadaError::NotFound.to_errno(), 2);
        assert_eq!(DadaError::NoInodes.to_errno(), 28);
        assert_eq!(DadaError::Corrupt("x".into()).to_errno(), 5);
        #[cfg(target_os = "linux")]
        assert_eq!(DadaError::NotEmpty.to_errno(), 39);
        #[cfg(target_os = "macos")]
        assert_eq!(DadaError::NotEmpty.to_errno(), 66);
    }

    #[test]
    fn display() {
        assert_eq!(
            DadaError::Unsupported(8).to_string(),
            "unsupported feature: 0x8"
        );
        assert_eq!(
            DadaError::Corrupt("bad".into()).to_string(),
            "corruption: bad"
        );
    }
}
