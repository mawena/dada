//! Conversions between dada and Windows: times, attributes, status codes
//! and symbolic link reparse points.

use libdada::format::{WIN_ATTR_ARCHIVE, WIN_ATTR_HIDDEN, WIN_ATTR_SYSTEM};
use libdada::{DadaError, FileKind};

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const SYMLINK_FLAG_RELATIVE: u32 = 1;

/// 100-ns intervals between 1601-01-01 and 1970-01-01.
const FILETIME_UNIX_EPOCH: i128 = 116_444_736_000_000_000;

/// Nanoseconds since 1970 to FILETIME (100 ns since 1601), saturating.
pub fn to_filetime(ns: i64) -> u64 {
    let ft = i128::from(ns).div_euclid(100) + FILETIME_UNIX_EPOCH;
    u64::try_from(ft.max(0)).unwrap_or(u64::MAX)
}

/// FILETIME to nanoseconds since 1970, saturating.
pub fn from_filetime(ft: u64) -> i64 {
    let ns = (i128::from(ft) - FILETIME_UNIX_EPOCH) * 100;
    i64::try_from(ns).unwrap_or(if ns < 0 { i64::MIN } else { i64::MAX })
}

/// Windows attributes of a file: read-only comes from the owner write bit,
/// hidden, system and archive from `win_attrs`.
pub fn windows_attributes(kind: FileKind, mode: u16, win_attrs: u32) -> u32 {
    let mut attrs =
        win_attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_ARCHIVE);
    match kind {
        FileKind::Directory => attrs |= FILE_ATTRIBUTE_DIRECTORY,
        FileKind::Symlink => attrs |= FILE_ATTRIBUTE_REPARSE_POINT,
        FileKind::RegularFile => {}
    }
    if mode & 0o200 == 0 {
        attrs |= FILE_ATTRIBUTE_READONLY;
    }
    if attrs == 0 {
        FILE_ATTRIBUTE_NORMAL
    } else {
        attrs
    }
}

/// New `(mode, win_attrs)` after Windows sets `attrs`.
pub fn apply_windows_attributes(attrs: u32, mode: u16) -> (u16, u32) {
    let mode = if attrs & FILE_ATTRIBUTE_READONLY != 0 {
        mode & !0o222
    } else {
        mode | 0o200
    };
    let win_attrs = attrs & (WIN_ATTR_HIDDEN | WIN_ATTR_SYSTEM | WIN_ATTR_ARCHIVE);
    (mode, win_attrs)
}

pub const STATUS_END_OF_FILE: u32 = 0xC000_0011;
pub const STATUS_BUFFER_TOO_SMALL: u32 = 0xC000_0023;
pub const STATUS_OBJECT_NAME_INVALID: u32 = 0xC000_0033;
pub const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
pub const STATUS_OBJECT_NAME_COLLISION: u32 = 0xC000_0035;
pub const STATUS_OBJECT_PATH_NOT_FOUND: u32 = 0xC000_003A;
pub const STATUS_INVALID_PARAMETER: u32 = 0xC000_000D;
pub const STATUS_DISK_FULL: u32 = 0xC000_007F;
pub const STATUS_MEDIA_WRITE_PROTECTED: u32 = 0xC000_00A2;
pub const STATUS_FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;
pub const STATUS_NOT_SUPPORTED: u32 = 0xC000_00BB;
pub const STATUS_UNEXPECTED_IO_ERROR: u32 = 0xC000_00E9;
pub const STATUS_DIRECTORY_NOT_EMPTY: u32 = 0xC000_0101;
pub const STATUS_FILE_CORRUPT_ERROR: u32 = 0xC000_0102;
pub const STATUS_NOT_A_DIRECTORY: u32 = 0xC000_0103;
pub const STATUS_NAME_TOO_LONG: u32 = 0xC000_0106;
pub const STATUS_NOT_A_REPARSE_POINT: u32 = 0xC000_0275;

/// NTSTATUS for a libdada error.
pub fn ntstatus(e: &DadaError) -> u32 {
    match e {
        DadaError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        DadaError::Exists => STATUS_OBJECT_NAME_COLLISION,
        DadaError::NotDir => STATUS_NOT_A_DIRECTORY,
        DadaError::IsDir => STATUS_FILE_IS_A_DIRECTORY,
        DadaError::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        DadaError::NoSpace | DadaError::NoInodes => STATUS_DISK_FULL,
        DadaError::InvalidName => STATUS_OBJECT_NAME_INVALID,
        DadaError::NameTooLong => STATUS_NAME_TOO_LONG,
        DadaError::Invalid => STATUS_INVALID_PARAMETER,
        DadaError::ReadOnly => STATUS_MEDIA_WRITE_PROTECTED,
        DadaError::Unsupported(_) => STATUS_NOT_SUPPORTED,
        DadaError::Corrupt(_) => STATUS_FILE_CORRUPT_ERROR,
        DadaError::Io(_) => STATUS_UNEXPECTED_IO_ERROR,
    }
}

/// REPARSE_DATA_BUFFER of a symbolic link to `target` (a dada target with
/// `/` separators). The link is marked relative: an absolute dada target
/// (`/a/b`) becomes `\a\b`, rooted on the dada volume.
pub fn symlink_reparse_buffer(target: &str) -> Vec<u8> {
    let path: Vec<u16> = target.replace('/', "\\").encode_utf16().collect();
    let path_bytes = (path.len() * 2) as u16;
    let mut buf = Vec::with_capacity(20 + 2 * path.len() * 2);
    buf.extend_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
    // ReparseDataLength: the 12 bytes below plus both copies of the path.
    buf.extend_from_slice(&(12 + 2 * path_bytes).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    buf.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    buf.extend_from_slice(&path_bytes.to_le_bytes()); // SubstituteNameLength
    buf.extend_from_slice(&path_bytes.to_le_bytes()); // PrintNameOffset
    buf.extend_from_slice(&path_bytes.to_le_bytes()); // PrintNameLength
    buf.extend_from_slice(&SYMLINK_FLAG_RELATIVE.to_le_bytes());
    for _ in 0..2 {
        for unit in &path {
            buf.extend_from_slice(&unit.to_le_bytes());
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime() {
        assert_eq!(to_filetime(0), 116_444_736_000_000_000);
        for ns in [0, 100, -100, 1_700_000_000_000_000_000] {
            assert_eq!(from_filetime(to_filetime(ns)), ns);
        }
        // Sub-100 ns precision is lost.
        assert_eq!(from_filetime(to_filetime(150)), 100);
        // Every i64 date (1677..2262) is after 1601; FILETIME 0 (year 1601)
        // is before 1677 and saturates.
        assert!(to_filetime(i64::MIN) > 0);
        assert_eq!(from_filetime(0), i64::MIN);
        assert!(from_filetime(u64::MAX) > 0);
    }

    #[test]
    fn attributes() {
        assert_eq!(
            windows_attributes(FileKind::RegularFile, 0o644, 0),
            FILE_ATTRIBUTE_NORMAL
        );
        assert_eq!(
            windows_attributes(FileKind::RegularFile, 0o444, 0x22),
            FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_ARCHIVE
        );
        assert_eq!(
            windows_attributes(FileKind::Directory, 0o755, 0),
            FILE_ATTRIBUTE_DIRECTORY
        );
        assert_eq!(
            windows_attributes(FileKind::Symlink, 0o777, 0),
            FILE_ATTRIBUTE_REPARSE_POINT
        );
        assert_eq!(
            apply_windows_attributes(FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_SYSTEM, 0o664),
            (0o444, 0x4)
        );
        assert_eq!(
            apply_windows_attributes(FILE_ATTRIBUTE_NORMAL, 0o444),
            (0o644, 0)
        );
    }

    #[test]
    fn reparse_buffer_layout() {
        let buf = symlink_reparse_buffer("../a/b");
        let path: Vec<u8> = "..\\a\\b"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(&buf[0..4], &IO_REPARSE_TAG_SYMLINK.to_le_bytes());
        assert_eq!(
            u16::from_le_bytes([buf[4], buf[5]]) as usize,
            12 + 2 * path.len()
        );
        assert_eq!(u16::from_le_bytes([buf[10], buf[11]]) as usize, path.len());
        assert_eq!(u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]), 1);
        assert_eq!(&buf[20..20 + path.len()], &path[..]);
        assert_eq!(buf.len(), 8 + 12 + 2 * path.len());
    }
}
