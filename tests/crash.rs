//! Milestone 6 criterion: run a random workload, cut the power after a
//! random number of writes, remount (journal replay) and check that fsck
//! finds nothing and that files validated by a successful `sync` are intact.

use std::collections::{HashMap, HashSet};

use dada_tests::seed;
use fsck_dada::{check, Options, EXIT_CLEAN};
use libdada::format::STATE_CLEAN;
use libdada::journal;
use libdada::superblock::Superblock;
use libdada::{format, DadaError, FaultyDevice, FormatOptions, Ino, MemDevice, SetAttr, Volume};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const DIRS: usize = 4;

type Vol = Volume<FaultyDevice<MemDevice>>;

fn fresh_device() -> MemDevice {
    let mut dev = MemDevice::new(1024, 4096).unwrap();
    let opts = FormatOptions {
        block_size: 1024,
        ..FormatOptions::default()
    };
    format(&mut dev, &opts).unwrap();
    dev
}

fn bytes(rng: &mut StdRng, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    rng.fill(&mut v[..]);
    v
}

/// The workload: the model of the files and what was validated by `sync`.
struct Workload {
    rng: StdRng,
    dirs: Vec<Ino>,
    files: HashMap<String, Vec<u8>>,
    synced: HashMap<String, Vec<u8>>,
    touched: HashSet<String>,
    next: usize,
}

fn split(path: &str) -> (usize, &str) {
    let (d, name) = path.split_once('/').unwrap();
    (d[1..].parse().unwrap(), name)
}

impl Workload {
    fn step(&mut self, vol: &mut Vol) -> Result<(), DadaError> {
        let names: Vec<String> = {
            let mut v: Vec<String> = self.files.keys().cloned().collect();
            v.sort();
            v
        };
        let pick = |rng: &mut StdRng| names[rng.random_range(0..names.len())].clone();
        match self.rng.random_range(0..100) {
            0..=29 => {
                let d = self.rng.random_range(0..DIRS);
                let name = format!("f{}", self.next);
                self.next += 1;
                let path = format!("d{d}/{name}");
                let len = self.rng.random_range(0..20_000);
                let data = bytes(&mut self.rng, len);
                self.touched.insert(path.clone());
                let f = vol.create(self.dirs[d], &name, 0o644, 0, 0)?;
                self.files.insert(path.clone(), Vec::new());
                vol.write(f.ino, 0, &data)?;
                self.files.insert(path, data);
            }
            30..=44 if !names.is_empty() => {
                let path = pick(&mut self.rng);
                let (d, name) = split(&path);
                let ino = vol.lookup(self.dirs[d], name)?.ino;
                let content = self.files.get_mut(&path).unwrap();
                let offset = self.rng.random_range(0..=content.len() + 3000);
                let len = self.rng.random_range(1..8000);
                let data = bytes(&mut self.rng, len);
                self.touched.insert(path.clone());
                vol.write(ino, offset as u64, &data)?;
                if content.len() < offset + len {
                    content.resize(offset + len, 0);
                }
                content[offset..offset + len].copy_from_slice(&data);
            }
            45..=54 if !names.is_empty() => {
                let path = pick(&mut self.rng);
                let (d, name) = split(&path);
                let ino = vol.lookup(self.dirs[d], name)?.ino;
                let content = self.files.get_mut(&path).unwrap();
                let size = self.rng.random_range(0..=content.len() + 5000);
                self.touched.insert(path.clone());
                let changes = SetAttr {
                    size: Some(size as u64),
                    ..SetAttr::default()
                };
                vol.setattr(ino, &changes)?;
                content.resize(size, 0);
            }
            55..=69 if !names.is_empty() => {
                let path = pick(&mut self.rng);
                let (d, name) = split(&path);
                self.touched.insert(path.clone());
                vol.unlink(self.dirs[d], name)?;
                self.files.remove(&path);
            }
            70..=84 if !names.is_empty() => {
                let path = pick(&mut self.rng);
                let (d, name) = split(&path);
                let to_dir = self.rng.random_range(0..DIRS);
                let to_name = if self.rng.random_bool(0.3) && names.len() > 1 {
                    // Sometimes replace another file.
                    let other = pick(&mut self.rng);
                    let (od, oname) = split(&other);
                    if other == path {
                        return Ok(());
                    }
                    let renamed = (od, oname.to_string());
                    self.touched.insert(other.clone());
                    renamed
                } else {
                    let n = format!("r{}", self.next);
                    self.next += 1;
                    (to_dir, n)
                };
                let dest = format!("d{}/{}", to_name.0, to_name.1);
                self.touched.insert(path.clone());
                self.touched.insert(dest.clone());
                vol.rename(self.dirs[d], name, self.dirs[to_name.0], &to_name.1)?;
                let content = self.files.remove(&path).unwrap();
                self.files.insert(dest, content);
            }
            _ => {
                vol.sync()?;
                self.synced = self.files.clone();
                self.touched.clear();
            }
        }
        Ok(())
    }
}

/// What one iteration exercised.
#[derive(Default)]
struct Stats {
    replayed_transactions: u64,
    verified_files: u64,
}

fn crash_iteration(seed: u64) -> Stats {
    let mut stats = Stats::default();
    let mut rng = StdRng::seed_from_u64(seed);
    // A full workload makes about 2 700 writes.
    let cut = rng.random_range(0..3000);
    let mut work = Workload {
        rng: StdRng::seed_from_u64(seed ^ 0x9E37_79B9_7F4A_7C15),
        dirs: Vec::new(),
        files: HashMap::new(),
        synced: HashMap::new(),
        touched: HashSet::new(),
        next: 0,
    };

    let device = FaultyDevice::new(fresh_device(), cut);
    let mem = match Volume::open_with_cache(device, false, 64) {
        Err(_) => None,
        Ok(mut vol) => {
            let setup: Result<(), DadaError> = (|| {
                for d in 0..DIRS {
                    let ino = vol.mkdir(vol.root(), &format!("d{d}"), 0o755, 0, 0)?.ino;
                    work.dirs.push(ino);
                }
                vol.sync()
            })();
            if setup.is_ok() {
                for _ in 0..300 {
                    if work.step(&mut vol).is_err() {
                        break;
                    }
                }
            }
            Some(vol.into_device().into_inner())
        }
    };
    let Some(mut mem) = mem else {
        return stats; // The cut happened while mounting: nothing to check.
    };
    let sb = Superblock::decode(mem.as_bytes()).unwrap();
    if sb.state != STATE_CLEAN {
        stats.replayed_transactions = journal::scan(&mut mem, &sb).unwrap().transactions;
    }

    // Remount: the journal is replayed. Then a clean unmount.
    let mem = Volume::open(mem, false)
        .unwrap_or_else(|e| panic!("seed {seed}: remount failed: {e}"))
        .close()
        .unwrap();
    let (report, mem) = check(mem, Options { repair: false }).unwrap();
    assert_eq!(
        report.exit_code(),
        EXIT_CLEAN,
        "seed {seed} (cut after {cut} writes): {:?}",
        report.problems
    );

    // Every file validated by the last sync and not touched since is intact.
    if work.dirs.is_empty() {
        return stats;
    }
    let mut vol = Volume::open(mem, true).unwrap();
    for (path, content) in &work.synced {
        if work.touched.contains(path) {
            continue;
        }
        let (d, name) = split(path);
        let attr = vol
            .lookup(work.dirs[d], name)
            .unwrap_or_else(|e| panic!("seed {seed}: {path}: {e}"));
        let mut buf = vec![0u8; attr.size as usize];
        vol.read(attr.ino, 0, &mut buf).unwrap();
        assert!(buf == *content, "seed {seed}: {path} differs");
        stats.verified_files += 1;
    }
    stats
}

fn crash_loop(iterations: u64) {
    let base = seed();
    let mut total = Stats::default();
    for i in 0..iterations {
        let stats = crash_iteration(base.wrapping_add(i));
        total.replayed_transactions += stats.replayed_transactions;
        total.verified_files += stats.verified_files;
    }
    eprintln!(
        "{iterations} cuts: {} transactions replayed, {} synced files verified",
        total.replayed_transactions, total.verified_files
    );
    // The loop must really exercise the journal and the durability check.
    assert!(total.replayed_transactions > 0);
    assert!(total.verified_files > 0);
}

#[test]
fn power_cuts_short() {
    crash_loop(40);
}

/// The milestone 6 criterion: 1 000 power cuts.
#[test]
#[ignore]
fn power_cuts_1000() {
    crash_loop(1000);
}
