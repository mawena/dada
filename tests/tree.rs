//! Milestone 3 criterion: write a random tree of directories and files,
//! remount, check every SHA-256, delete everything and check that the free
//! counters return exactly to their values after formatting.

use dada_tests::{create_image, format_image, hex, seed, Sha256};
use libdada::{FileDevice, FileKind, FormatOptions, Ino, Volume};
use rand::rngs::StdRng;
use rand::{Rng, RngCore, SeedableRng};

const MIB: u64 = 1024 * 1024;
const CHUNK: usize = 256 * 1024;

struct FileSpec {
    path: Vec<String>,
    size: u64,
    seed: u64,
    sha256: [u8; 32],
}

fn open(path: &std::path::Path, read_only: bool) -> Volume<FileDevice> {
    let dev = FileDevice::open_image(path, !read_only).unwrap();
    Volume::open(dev, read_only).unwrap()
}

fn resolve(vol: &mut Volume<FileDevice>, path: &[String]) -> Ino {
    let mut ino = vol.root();
    for name in path {
        ino = vol
            .lookup(ino, name)
            .unwrap_or_else(|e| panic!("{path:?}: {e}"))
            .ino;
    }
    ino
}

/// Writes `size` pseudo-random bytes derived from `seed`; returns their SHA-256.
fn write_file(vol: &mut Volume<FileDevice>, ino: Ino, size: u64, seed: u64) -> [u8; 32] {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut hash = Sha256::default();
    let mut buf = vec![0u8; CHUNK];
    let mut offset = 0;
    while offset < size {
        let n = (size - offset).min(CHUNK as u64) as usize;
        rng.fill_bytes(&mut buf[..n]);
        hash.update(&buf[..n]);
        assert_eq!(vol.write(ino, offset, &buf[..n]).unwrap(), n);
        offset += n as u64;
    }
    hash.finish()
}

fn read_hash(vol: &mut Volume<FileDevice>, ino: Ino) -> [u8; 32] {
    let mut hash = Sha256::default();
    let mut buf = vec![0u8; CHUNK];
    let mut offset = 0;
    loop {
        let n = vol.read(ino, offset, &mut buf).unwrap();
        if n == 0 {
            return hash.finish();
        }
        hash.update(&buf[..n]);
        offset += n as u64;
    }
}

fn tree_round_trip(image_size: u64, dirs: usize, files: usize, max_size: u64) {
    let seed = seed();
    let mut rng = StdRng::seed_from_u64(seed);
    let tmp = tempfile::tempdir().unwrap();
    let image = create_image(&tmp, "tree.img", image_size);
    format_image(&image, &FormatOptions::default());
    let formatted = open(&image, true).statfs();

    // Directories: each one goes under the root or a previous directory.
    let mut vol = open(&image, false);
    let mut dir_paths: Vec<Vec<String>> = vec![Vec::new()];
    let mut dir_inos: Vec<Ino> = vec![vol.root()];
    for i in 0..dirs {
        let parent = rng.random_range(0..dir_paths.len());
        let name = format!("dir-{i}");
        let attr = vol.mkdir(dir_inos[parent], &name, 0o755, 0, 0).unwrap();
        let mut path = dir_paths[parent].clone();
        path.push(name);
        dir_paths.push(path);
        dir_inos.push(attr.ino);
    }

    // Files of random sizes in random directories.
    let mut specs = Vec::with_capacity(files);
    for i in 0..files {
        let parent = rng.random_range(0..dir_paths.len());
        let name = format!("file-{i}.bin");
        let size = rng.random_range(0..=max_size);
        let file_seed = rng.random();
        let attr = vol.create(dir_inos[parent], &name, 0o644, 0, 0).unwrap();
        let sha256 = write_file(&mut vol, attr.ino, size, file_seed);
        let mut path = dir_paths[parent].clone();
        path.push(name);
        specs.push(FileSpec {
            path,
            size,
            seed: file_seed,
            sha256,
        });
    }
    vol.close().unwrap();

    // Remount and check every file and directory.
    let mut vol = open(&image, false);
    for spec in &specs {
        let ino = resolve(&mut vol, &spec.path);
        let attr = vol.getattr(ino).unwrap();
        assert_eq!(attr.kind, FileKind::RegularFile);
        assert_eq!(attr.size, spec.size, "{:?}", spec.path);
        assert_eq!(
            hex(&read_hash(&mut vol, ino)),
            hex(&spec.sha256),
            "{:?} (file seed {})",
            spec.path,
            spec.seed
        );
    }
    let mut entries_seen = 0;
    for path in &dir_paths {
        let ino = resolve(&mut vol, path);
        entries_seen += vol.readdir(ino, 0).unwrap().len() - 2;
    }
    assert_eq!(entries_seen, dirs + files);

    // Delete everything: files, then directories deepest first.
    for spec in &specs {
        let (name, parent) = spec.path.split_last().unwrap();
        let parent = resolve(&mut vol, parent);
        vol.unlink(parent, name).unwrap();
    }
    for path in dir_paths.iter().skip(1).rev() {
        let (name, parent) = path.split_last().unwrap();
        let parent = resolve(&mut vol, parent);
        vol.rmdir(parent, name).unwrap();
    }
    vol.close().unwrap();

    let mut vol = open(&image, true);
    let end = vol.statfs();
    assert_eq!(end.free_blocks, formatted.free_blocks);
    assert_eq!(end.free_inodes, formatted.free_inodes);
    assert_eq!(vol.readdir(vol.root(), 0).unwrap().len(), 2);
    assert_eq!(vol.getattr(vol.root()).unwrap().links, 2);
}

/// Small version, run with every `cargo test`.
#[test]
fn small_tree_round_trip() {
    tree_round_trip(256 * MIB, 50, 400, 256 * 1024);
}

/// The milestone 3 criterion: 1 000 directories, 10 000 files of 0 to 1 MiB
/// (about 5 GiB written). Run with `cargo test --release -- --ignored`.
#[test]
#[ignore]
fn large_tree_round_trip() {
    tree_round_trip(8 * 1024 * MIB, 1000, 10_000, MIB);
}
