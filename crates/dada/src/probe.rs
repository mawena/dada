//! Identification of a dada volume, for people and for udev.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use libdada::format::SUPERBLOCK_SIZE;
use libdada::superblock::Superblock;
use libdada::FORMAT_VERSION;

/// What identifies a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub label: String,
    pub uuid: String,
    pub block_size: u32,
    pub total_blocks: u64,
}

/// Reads the primary superblock of `path`. `Ok(None)` when it holds no
/// valid dada superblock.
pub fn identify(path: &Path) -> Result<Option<Identity>, String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = [0u8; SUPERBLOCK_SIZE];
    if file.read_exact(&mut buf).is_err() {
        return Ok(None);
    }
    Ok(Superblock::decode(&buf).ok().map(|sb| Identity {
        label: sb.label().unwrap_or("").to_string(),
        uuid: uuid::Uuid::from_bytes(sb.uuid).hyphenated().to_string(),
        block_size: sb.block_size,
        total_blocks: sb.total_blocks,
    }))
}

/// The properties udev's blkid would set, in `IMPORT{program}` format.
pub fn udev_properties(id: &Identity) -> String {
    let mut out = String::new();
    let mut add = |key: &str, value: &str| {
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    };
    add("ID_FS_TYPE", "dada");
    add("ID_FS_USAGE", "filesystem");
    add("ID_FS_VERSION", &FORMAT_VERSION.to_string());
    add("ID_FS_UUID", &id.uuid);
    add("ID_FS_UUID_ENC", &id.uuid);
    if !id.label.is_empty() {
        add("ID_FS_LABEL", &safe(&id.label));
        add("ID_FS_LABEL_ENC", &encode(&id.label));
    }
    out
}

/// Label with blanks and control characters replaced, like
/// `blkid_safe_string`.
fn safe(label: &str) -> String {
    label
        .trim_end()
        .chars()
        .map(|c| {
            if c.is_whitespace() || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Label with every byte outside a safe set written `\xNN`, like
/// `blkid_encode_string`; non-ASCII characters are kept.
fn encode(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() || "#+-.:=@_".contains(c) || !c.is_ascii() {
            out.push(c);
        } else {
            out.push_str(&format!("\\x{:02x}", c as u32));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use libdada::{format, FileDevice, FormatOptions};

    #[test]
    fn encoding() {
        assert_eq!(safe("Ma clé  "), "Ma_clé");
        assert_eq!(encode("Ma clé/1"), "Ma\\x20clé\\x2f1");
        assert_eq!(encode("KEY_1.a"), "KEY_1.a");
    }

    #[test]
    fn identifies_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.img");
        File::create(&path).unwrap().set_len(8 << 20).unwrap();
        assert_eq!(identify(&path).unwrap(), None);

        let mut dev = FileDevice::open(&path, 4096, true).unwrap();
        let opts = FormatOptions {
            label: "Ma clé".into(),
            ..FormatOptions::default()
        };
        format(&mut dev, &opts).unwrap();
        drop(dev);
        let id = identify(&path).unwrap().unwrap();
        assert_eq!(id.label, "Ma clé");
        assert_eq!(id.block_size, 4096);
        let props = udev_properties(&id);
        assert!(props.contains("ID_FS_TYPE=dada\n"));
        assert!(props.contains("ID_FS_USAGE=filesystem\n"));
        assert!(props.contains("ID_FS_LABEL=Ma_clé\n"));
        assert!(props.contains("ID_FS_LABEL_ENC=Ma\\x20clé\n"));
        assert!(props.contains(&format!("ID_FS_UUID={}\n", id.uuid)));

        let short = dir.path().join("short.img");
        File::create(&short).unwrap().set_len(100).unwrap();
        assert_eq!(identify(&short).unwrap(), None);
    }
}
