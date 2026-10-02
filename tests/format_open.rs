//! Format image files, then open them as a user of libdada would.

use dada_tests::{create_image, format_image};
use libdada::format::{STATE_CLEAN, STATE_DIRTY};
use libdada::superblock::Superblock;
use libdada::{FileDevice, FileKind, FormatOptions, Volume};

const MIB: u64 = 1024 * 1024;

#[test]
fn format_close_reopen_on_files() {
    let dir = tempfile::tempdir().unwrap();
    let configs = [
        (1024, 4 * MIB, true, false),
        (4096, 100 * MIB, true, false),
        (4096, 16 * MIB, false, true),
        (65536, 64 * MIB, true, true),
    ];
    for (i, (bs, size, journal, casefold)) in configs.into_iter().enumerate() {
        let path = create_image(&dir, &format!("{i}.img"), size);
        let opts = FormatOptions {
            block_size: bs,
            label: format!("vol{i}"),
            journal,
            casefold,
            ..FormatOptions::default()
        };
        format_image(&path, &opts);

        // Mount read-write, then close.
        let dev = FileDevice::open_image(&path, true).unwrap();
        assert_eq!(dev_block_size(&dev), bs);
        let mut vol = Volume::open(dev, false).unwrap();
        assert_eq!(vol.superblock().state, STATE_DIRTY);
        let st = vol.statfs();
        assert_eq!(st.total_blocks, size / u64::from(bs));
        let names: Vec<_> = vol
            .readdir(vol.root(), 0)
            .unwrap()
            .into_iter()
            .map(|(_, e)| (e.name, e.kind))
            .collect();
        assert_eq!(
            names,
            [
                (".".to_string(), FileKind::Directory),
                ("..".to_string(), FileKind::Directory)
            ]
        );
        vol.close().unwrap();

        // The file itself now holds a clean superblock and its backup.
        let bytes = std::fs::read(&path).unwrap();
        let sb = Superblock::decode(&bytes).unwrap();
        sb.validate().unwrap();
        assert_eq!((sb.state, sb.mount_count), (STATE_CLEAN, 1));
        assert_eq!(sb.label().unwrap(), format!("vol{i}"));
        let backup = bytes.len() - bs as usize;
        assert_eq!(Superblock::decode(&bytes[backup..]).unwrap(), sb);

        // Read-only mount leaves the file untouched.
        let vol = Volume::open(FileDevice::open_image(&path, false).unwrap(), true).unwrap();
        vol.close().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

fn dev_block_size(dev: &FileDevice) -> u32 {
    libdada::BlockDevice::block_size(dev)
}
