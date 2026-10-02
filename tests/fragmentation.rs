//! Milestone 4 criterion: fragment the volume on purpose, then write a
//! 50 MiB file spread over more than 100 extents and read it back.

use dada_tests::{create_image, format_image, hex, seed, Sha256};
use libdada::{FileDevice, FormatOptions, Volume};
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};

const MIB: u64 = 1024 * 1024;

#[test]
fn fragmented_volume_large_file() {
    let seed = seed();
    let tmp = tempfile::tempdir().unwrap();
    let image = create_image(&tmp, "frag.img", 160 * MIB);
    format_image(&image, &FormatOptions::default());
    let mut vol = Volume::open(FileDevice::open_image(&image, true).unwrap(), false).unwrap();
    let root = vol.root();

    // Fill the volume with 32 KiB files, then delete every other one: the
    // free space becomes a comb of 8-block holes. The files are spread over
    // a few directories to keep the linear directory searches short.
    let dirs: Vec<u64> = (0..64)
        .map(|i| vol.mkdir(root, &format!("d{i}"), 0o755, 0, 0).unwrap().ino)
        .collect();
    let place = |i: usize| (dirs[i % dirs.len()], format!("filler-{i}"));
    let filler = vec![0x5Au8; 32 * 1024];
    let mut count = 0;
    loop {
        let (dir, name) = place(count);
        let f = match vol.create(dir, &name, 0o644, 0, 0) {
            Ok(f) => f,
            Err(_) => break,
        };
        if vol.write(f.ino, 0, &filler).is_err() {
            vol.unlink(dir, &name).unwrap();
            break;
        }
        count += 1;
    }
    assert!(count > 3000, "only {count} filler files");
    for i in (0..count).step_by(2) {
        let (dir, name) = place(i);
        vol.unlink(dir, &name).unwrap();
    }

    // 50 MiB written in 1 MiB pieces.
    let big = vol.create(root, "big.bin", 0o644, 0, 0).unwrap();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut hash = Sha256::default();
    let mut chunk = vec![0u8; MIB as usize];
    for i in 0..50 {
        rng.fill_bytes(&mut chunk);
        hash.update(&chunk);
        vol.write(big.ino, i * MIB, &chunk).unwrap();
    }
    let expected = hash.finish();
    let extents = vol.extents(big.ino).unwrap();
    assert!(extents.len() > 100, "only {} extents", extents.len());
    assert!(
        extents
            .windows(2)
            .all(|w| w[0].logical + w[0].length <= w[1].logical),
        "extents sorted and disjoint"
    );
    vol.close().unwrap();

    // Read it back after a remount.
    let mut vol = Volume::open(FileDevice::open_image(&image, false).unwrap(), true).unwrap();
    let big = vol.lookup(vol.root(), "big.bin").unwrap();
    assert_eq!(big.size, 50 * MIB);
    let mut hash = Sha256::default();
    let mut offset = 0;
    loop {
        let n = vol.read(big.ino, offset, &mut chunk).unwrap();
        if n == 0 {
            break;
        }
        hash.update(&chunk[..n]);
        offset += n as u64;
    }
    assert_eq!(hex(&hash.finish()), hex(&expected));
    assert_eq!(vol.extents(big.ino).unwrap().len(), extents.len());
}
