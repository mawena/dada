//! fsck-dada: checks a dada image and repairs it with `--repair`.
//!
//! Exit codes: 0 clean, 1 errors fixed, 4 errors left, 8 runtime error.

use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "fsck-dada",
    version,
    about = "Check and repair a dada filesystem"
)]
struct Cli {
    #[command(flatten)]
    args: fsck_dada::cli::CheckArgs,
}

fn main() -> ExitCode {
    let code = fsck_dada::cli::run(&Cli::parse().args, "fsck-dada");
    ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX))
}
