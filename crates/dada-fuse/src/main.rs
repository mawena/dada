//! dada-fuse: mounts a dada image or device with FUSE (Linux, macOS).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[cfg(feature = "fuse")]
mod fs;

#[derive(Parser)]
#[command(
    name = "dada-fuse",
    version,
    about = "Mount a dada filesystem with FUSE"
)]
struct Args {
    /// Mount options, comma-separated: ro, allow_other, uid=N, gid=N
    #[arg(short = 'o', value_name = "OPTIONS")]
    options: Vec<String>,
    /// Image file or device
    image: PathBuf,
    /// Mount point
    mountpoint: PathBuf,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct MountOptions {
    read_only: bool,
    allow_other: bool,
    uid: Option<u32>,
    gid: Option<u32>,
}

fn parse_options(raw: &[String]) -> Result<MountOptions, String> {
    let mut opts = MountOptions::default();
    for option in raw
        .iter()
        .flat_map(|o| o.split(','))
        .filter(|o| !o.is_empty())
    {
        let number = |v: &str| {
            v.parse::<u32>()
                .map_err(|_| format!("invalid value in {option:?}"))
        };
        match option.split_once('=') {
            None if option == "ro" => opts.read_only = true,
            None if option == "rw" => opts.read_only = false,
            None if option == "allow_other" => opts.allow_other = true,
            Some(("uid", v)) => opts.uid = Some(number(v)?),
            Some(("gid", v)) => opts.gid = Some(number(v)?),
            _ => return Err(format!("unknown option {option:?}")),
        }
    }
    Ok(opts)
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let opts = match parse_options(&args.options) {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("dada-fuse: {e}");
            return ExitCode::FAILURE;
        }
    };
    match run(&args, &opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dada-fuse: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "fuse")]
fn run(args: &Args, opts: &MountOptions) -> Result<(), String> {
    use fuser::{Config, MountOption, SessionACL};
    use libdada::{FileDevice, Volume};

    let image = args.image.display();
    let dev = FileDevice::open_image(&args.image, !opts.read_only)
        .map_err(|e| format!("{image}: {e}"))?;
    let vol = Volume::open(dev, opts.read_only).map_err(|e| format!("{image}: {e}"))?;
    // Showing everything as one user needs both ids; the missing one is the
    // caller's own.
    let owner = match (opts.uid, opts.gid) {
        (None, None) => None,
        (uid, gid) => Some((
            uid.unwrap_or_else(current_uid),
            gid.unwrap_or_else(current_gid),
        )),
    };
    let filesystem = fs::DadaFs::new(vol, fs::Presentation { owner });

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName(args.image.display().to_string()),
        MountOption::Subtype("dada".into()),
        MountOption::DefaultPermissions,
        MountOption::NoAtime,
        if opts.read_only {
            MountOption::RO
        } else {
            MountOption::RW
        },
    ];
    if opts.allow_other {
        config.acl = SessionACL::All;
    }
    log::info!("mounting {image} on {}", args.mountpoint.display());
    fuser::mount(filesystem, &args.mountpoint, &config).map_err(|e| format!("mount failed: {e}"))
}

#[cfg(feature = "fuse")]
fn current_uid() -> u32 {
    std::env::var("UID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            // The owner of the home directory is the current user.
            home_metadata().map_or(0, |m| m.0)
        })
}

#[cfg(feature = "fuse")]
fn current_gid() -> u32 {
    home_metadata().map_or(0, |m| m.1)
}

#[cfg(feature = "fuse")]
fn home_metadata() -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let home = std::env::var_os("HOME")?;
    let meta = std::fs::metadata(home).ok()?;
    Some((meta.uid(), meta.gid()))
}

#[cfg(not(feature = "fuse"))]
fn run(_args: &Args, _opts: &MountOptions) -> Result<(), String> {
    Err("this build has no FUSE support; rebuild with `--features fuse`".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options() {
        let parsed = parse_options(&["ro,allow_other".into(), "uid=1000,gid=100".into()]).unwrap();
        assert_eq!(
            parsed,
            MountOptions {
                read_only: true,
                allow_other: true,
                uid: Some(1000),
                gid: Some(100),
            }
        );
        assert_eq!(parse_options(&[]).unwrap(), MountOptions::default());
        assert!(parse_options(&["uid=abc".into()]).is_err());
        assert!(parse_options(&["noexec".into()]).is_err());
    }
}
