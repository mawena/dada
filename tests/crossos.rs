//! dada-crossos: cross-OS check of the milestone 8 criterion.
//!
//! `make` writes a deterministic tree into a new image, `verify` reads an
//! image (made on any OS) through libdada, and `verify-dir` reads the same
//! tree from a mounted volume (FUSE or WinFsp). All compare SHA-256 sums.
//!
//!     dada-crossos make <image>
//!     dada-crossos verify <image>
//!     dada-crossos verify-dir [--windows-names] <mountpoint>

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dada_tests::{hex, Sha256};
use libdada::{format, FileDevice, FileKind, FormatOptions, Ino, Volume};
use rand::rngs::StdRng;
use rand::{Rng, RngCore, SeedableRng};

const IMAGE_SIZE: u64 = 64 << 20;
const SEED: u64 = 0xDADA;
const LINK_NAME: &str = "lien vers fichier";
const LINK_TARGET: &str = "répertoire 0/fichier 0.bin";

/// One file of the reference tree.
struct Entry {
    dirs: Vec<String>,
    name: String,
    content: Vec<u8>,
}

/// The reference tree, identical on every OS.
fn tree() -> Vec<Entry> {
    let mut rng = StdRng::seed_from_u64(SEED);
    let mut out = Vec::new();
    for d in 0..4 {
        let dirs = if d == 3 {
            vec!["répertoire 0".to_string(), "sous:dossier".to_string()]
        } else {
            vec![format!("répertoire {d}")]
        };
        for f in 0..5 {
            let size = match f {
                0 => 0,
                1 => 50, // inline
                _ => rng.random_range(97..200_000),
            };
            let mut content = vec![0u8; size];
            rng.fill_bytes(&mut content);
            out.push(Entry {
                dirs: dirs.clone(),
                name: format!("fichier {f}.bin"),
                content,
            });
        }
    }
    out.push(Entry {
        dirs: Vec::new(),
        name: "Ünïcödé été.txt".into(),
        content: "contenu accentué\n".repeat(100).into_bytes(),
    });
    out
}

fn make(image: &Path) -> Result<(), String> {
    File::create(image)
        .and_then(|f| f.set_len(IMAGE_SIZE))
        .map_err(|e| e.to_string())?;
    let mut dev = FileDevice::open(image, 4096, true).map_err(|e| e.to_string())?;
    format(&mut dev, &FormatOptions::default()).map_err(|e| e.to_string())?;
    let mut vol = Volume::open(dev, false).map_err(|e| e.to_string())?;
    for entry in tree() {
        let mut parent = vol.root();
        for dir in &entry.dirs {
            parent = match vol.lookup(parent, dir) {
                Ok(attr) => attr.ino,
                Err(_) => {
                    vol.mkdir(parent, dir, 0o755, 0, 0)
                        .map_err(|e| e.to_string())?
                        .ino
                }
            };
        }
        let f = vol
            .create(parent, &entry.name, 0o644, 0, 0)
            .map_err(|e| e.to_string())?;
        vol.write(f.ino, 0, &entry.content)
            .map_err(|e| e.to_string())?;
    }
    let root = vol.root();
    vol.symlink(root, LINK_NAME, LINK_TARGET, 0, 0)
        .map_err(|e| e.to_string())?;
    vol.close().map_err(|e| e.to_string())?;
    println!("made {}", image.display());
    Ok(())
}

fn digest(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn check(what: &str, got: &[u8], expected: &[u8], failures: &mut usize) {
    if got == expected {
        println!("ok   {} {what}", digest(got));
    } else {
        println!(
            "FAIL {what}: {} instead of {}",
            digest(got),
            digest(expected)
        );
        *failures += 1;
    }
}

fn verify(image: &Path) -> Result<usize, String> {
    let dev = FileDevice::open_image(image, false).map_err(|e| e.to_string())?;
    let mut vol = Volume::open(dev, true).map_err(|e| e.to_string())?;
    let mut failures = 0;
    for entry in tree() {
        let path = [entry.dirs.clone(), vec![entry.name.clone()]].concat();
        let mut ino: Ino = vol.root();
        for name in &path {
            ino = vol
                .lookup(ino, name)
                .map_err(|e| format!("{path:?}: {e}"))?
                .ino;
        }
        let attr = vol.getattr(ino).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; attr.size as usize];
        vol.read(ino, 0, &mut buf).map_err(|e| e.to_string())?;
        check(&path.join("/"), &buf, &entry.content, &mut failures);
    }
    let root = vol.root();
    let link = vol.lookup(root, LINK_NAME).map_err(|e| e.to_string())?;
    if link.kind != FileKind::Symlink
        || vol.readlink(link.ino).map_err(|e| e.to_string())? != LINK_TARGET
    {
        println!("FAIL {LINK_NAME}: wrong symbolic link");
        failures += 1;
    }
    Ok(failures)
}

/// Name as the WinFsp adapter shows it (forbidden characters escaped).
fn windows_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if "\\:*?\"<>|".contains(c) || ('\u{1}'..='\u{1f}').contains(&c) {
                char::from_u32(0xF000 + c as u32).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

fn verify_dir(root: &Path, windows_names: bool) -> Result<usize, String> {
    let shown = |name: &str| {
        if windows_names {
            windows_name(name)
        } else {
            name.to_string()
        }
    };
    let mut failures = 0;
    for entry in tree() {
        let mut path = root.to_path_buf();
        for part in entry.dirs.iter().chain(std::iter::once(&entry.name)) {
            path.push(shown(part));
        }
        let got = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        check(
            &path.display().to_string(),
            &got,
            &entry.content,
            &mut failures,
        );
    }
    // Reading through the link checks that it resolves.
    let through_link =
        std::fs::read(root.join(LINK_NAME)).map_err(|e| format!("{LINK_NAME}: {e}"))?;
    check(LINK_NAME, &through_link, &tree()[0].content, &mut failures);
    Ok(failures)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["make", image] => make(Path::new(image)).map(|()| 0),
        ["verify", image] => verify(Path::new(image)),
        ["verify-dir", "--windows-names", dir] => verify_dir(&PathBuf::from(dir), true),
        ["verify-dir", dir] => verify_dir(&PathBuf::from(dir), false),
        _ => Err(
            "usage: dada-crossos make|verify <image> | verify-dir [--windows-names] <dir>".into(),
        ),
    };
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(n) => {
            eprintln!("{n} differences");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("dada-crossos: {e}");
            ExitCode::FAILURE
        }
    }
}
