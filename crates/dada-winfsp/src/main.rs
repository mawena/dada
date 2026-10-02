//! dada-winfsp: mounts a dada image or device on Windows with WinFsp.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

// Used by the adapter, and tested on every platform.
#[cfg_attr(not(all(windows, feature = "winfsp")), allow(dead_code))]
mod convert;
#[cfg(all(windows, feature = "winfsp"))]
mod fs;
#[cfg_attr(not(all(windows, feature = "winfsp")), allow(dead_code))]
mod names;

#[derive(Parser)]
#[command(
    name = "dada-winfsp",
    version,
    about = "Mount a dada filesystem on Windows with WinFsp"
)]
struct Args {
    /// Mount read-only
    #[arg(long)]
    read_only: bool,
    /// Image file or device
    image: PathBuf,
    /// Drive letter (`X:`) or directory to mount on
    mountpoint: String,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dada-winfsp: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(windows, feature = "winfsp"))]
fn run(args: &Args) -> Result<(), String> {
    use std::sync::{Arc, Mutex};

    use libdada::format::INCOMPAT_CASEFOLD;
    use libdada::{FileDevice, Volume};
    use winfsp::host::{FileSystemHost, VolumeParams};

    let _init = winfsp::winfsp_init().map_err(|e| format!("WinFsp is not available: {e}"))?;
    let image = args.image.display();
    let dev = FileDevice::open_image(&args.image, !args.read_only)
        .map_err(|e| format!("{image}: {e}"))?;
    let vol = Volume::open(dev, args.read_only).map_err(|e| format!("{image}: {e}"))?;
    let sb = vol.superblock().clone();
    let casefold = sb.features_incompat & INCOMPAT_CASEFOLD != 0;
    if !casefold {
        log::warn!("volume without CASEFOLD: exposed as case-sensitive, which some Windows programs do not expect");
    }
    let label = sb.label().unwrap_or("dada").to_string();
    let block_size = u16::try_from(sb.block_size / 512).unwrap_or(u16::MAX);

    let mut params = VolumeParams::new();
    params
        .filesystem_name("dada")
        .sector_size(512)
        .sectors_per_allocation_unit(block_size)
        .max_component_length(255)
        .volume_creation_time(convert::to_filetime(sb.created_ns))
        .volume_serial_number(u32::from_le_bytes([
            sb.uuid[0], sb.uuid[1], sb.uuid[2], sb.uuid[3],
        ]))
        .file_info_timeout(1000)
        .case_sensitive_search(!casefold)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(true)
        .reparse_points(true)
        .reparse_points_access_check(false)
        .post_cleanup_when_modified_only(true)
        .read_only_volume(args.read_only);

    let shared = Arc::new(Mutex::new(Some(vol)));
    let context = fs::DadaWinFs::new(shared.clone(), fs::security_descriptor(), label);
    let mut host: FileSystemHost<fs::DadaWinFs> =
        FileSystemHost::new(params, context).map_err(|e| format!("WinFsp: {e}"))?;
    host.mount(args.mountpoint.as_str())
        .map_err(|e| format!("cannot mount on {}: {e}", args.mountpoint))?;
    host.start().map_err(|e| format!("WinFsp: {e}"))?;
    println!(
        "{image} mounted on {}. Press Enter to unmount.",
        args.mountpoint
    );
    let mut line = String::new();
    if matches!(std::io::stdin().read_line(&mut line), Ok(0) | Err(_)) {
        // No console (service, CI): stay mounted until the process is stopped.
        loop {
            std::thread::park();
        }
    }
    host.stop();
    host.unmount();
    drop(host);

    let vol = shared
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .ok_or("volume already closed")?;
    vol.close()
        .map_err(|e| format!("closing {image} failed: {e}"))?;
    println!("{image} unmounted cleanly.");
    Ok(())
}

#[cfg(not(all(windows, feature = "winfsp")))]
fn run(_args: &Args) -> Result<(), String> {
    Err("this build has no WinFsp support; build on Windows with `--features winfsp`".into())
}
