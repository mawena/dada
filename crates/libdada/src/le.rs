//! Panic-free little-endian field access.
//!
//! Readers fail with `DadaError::Corrupt` when the buffer is too short.
//! Writers are only used on buffers owned by the encoder, with offsets from
//! `format.rs`; an out-of-range write is ignored rather than panicking.

use crate::DadaError;

fn array<const N: usize>(buf: &[u8], off: usize) -> Result<[u8; N], DadaError> {
    off.checked_add(N)
        .and_then(|end| buf.get(off..end))
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| DadaError::Corrupt(format!("truncated field at offset {off}")))
}

pub(crate) fn get_bytes<const N: usize>(buf: &[u8], off: usize) -> Result<[u8; N], DadaError> {
    array(buf, off)
}

pub(crate) fn get_u16(buf: &[u8], off: usize) -> Result<u16, DadaError> {
    array(buf, off).map(u16::from_le_bytes)
}

pub(crate) fn get_u32(buf: &[u8], off: usize) -> Result<u32, DadaError> {
    array(buf, off).map(u32::from_le_bytes)
}

pub(crate) fn get_u64(buf: &[u8], off: usize) -> Result<u64, DadaError> {
    array(buf, off).map(u64::from_le_bytes)
}

pub(crate) fn get_i64(buf: &[u8], off: usize) -> Result<i64, DadaError> {
    array(buf, off).map(i64::from_le_bytes)
}

pub(crate) fn put_bytes(buf: &mut [u8], off: usize, bytes: &[u8]) {
    if let Some(dst) = off
        .checked_add(bytes.len())
        .and_then(|end| buf.get_mut(off..end))
    {
        dst.copy_from_slice(bytes);
    }
}

pub(crate) fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    put_bytes(buf, off, &v.to_le_bytes());
}

pub(crate) fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    put_bytes(buf, off, &v.to_le_bytes());
}

pub(crate) fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    put_bytes(buf, off, &v.to_le_bytes());
}

pub(crate) fn put_i64(buf: &mut [u8], off: usize, v: i64) {
    put_bytes(buf, off, &v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_little_endian() {
        let mut buf = [0u8; 16];
        put_u32(&mut buf, 0, 0x0403_0201);
        assert_eq!(&buf[..4], &[1, 2, 3, 4]);
        put_i64(&mut buf, 8, -2);
        assert_eq!(get_i64(&buf, 8).unwrap(), -2);
        assert_eq!(get_u16(&buf, 0).unwrap(), 0x0201);
        assert_eq!(get_u32(&buf, 0).unwrap(), 0x0403_0201);
        put_u64(&mut buf, 0, u64::MAX);
        assert_eq!(get_u64(&buf, 0).unwrap(), u64::MAX);
    }

    #[test]
    fn short_buffers_fail_without_panic() {
        let buf = [0u8; 4];
        assert!(get_u64(&buf, 0).is_err());
        assert!(get_u32(&buf, 1).is_err());
        assert!(get_u16(&buf, usize::MAX).is_err());
        let mut buf = [0u8; 4];
        put_u64(&mut buf, 0, 1);
        put_u16(&mut buf, usize::MAX, 1);
        assert_eq!(buf, [0; 4]);
    }
}
