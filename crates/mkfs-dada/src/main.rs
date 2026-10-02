//! mkfs-dada: formats an image file or a device with the dada filesystem.

use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "mkfs-dada",
    version,
    about = "Format an image file or a device with the dada filesystem"
)]
struct Cli {
    #[command(flatten)]
    args: mkfs_dada::FormatArgs,
}

fn main() -> ExitCode {
    // Only `--force` overwrites a device or a non-empty target.
    match mkfs_dada::run(&Cli::parse().args, &mut |_| false) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mkfs-dada: {e}");
            ExitCode::FAILURE
        }
    }
}
