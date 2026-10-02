//! dadactl: inspect and manipulate dada images.

use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "dadactl",
    version,
    about = "Inspect and manipulate dada images"
)]
struct Cli {
    #[command(subcommand)]
    command: dadactl::Command,
}

fn main() -> ExitCode {
    match dadactl::run(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dadactl: {e}");
            ExitCode::FAILURE
        }
    }
}
