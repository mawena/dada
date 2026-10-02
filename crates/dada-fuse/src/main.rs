//! dada-fuse: mounts a dada image or device with FUSE (Linux, macOS).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "dada-fuse",
    version,
    about = "Mount a dada filesystem with FUSE"
)]
struct Args {
    /// Mount options, comma-separated: ro, allow_other, uid=N, gid=N, noexec, suid, dev
    #[arg(short = 'o', value_name = "OPTIONS")]
    options: Vec<String>,
    /// Image file or device
    image: PathBuf,
    /// Mount point
    mountpoint: PathBuf,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let result = dada_fuse::parse_options(&args.options, false)
        .and_then(|opts| dada_fuse::mount(&args.image, &args.mountpoint, &opts));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dada-fuse: {e}");
            ExitCode::FAILURE
        }
    }
}
