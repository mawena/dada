//! fsck-dada: checks a dada image and repairs it with `--repair`.
//!
//! Exit codes: 0 clean, 1 errors fixed, 4 errors left, 8 runtime error.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use fsck_dada::{check, Options, EXIT_ERROR};
use libdada::FileDevice;

#[derive(Parser)]
#[command(
    name = "fsck-dada",
    version,
    about = "Check and repair a dada filesystem"
)]
struct Args {
    /// Fix the problems found
    #[arg(long)]
    repair: bool,
    /// Also print notes and a summary
    #[arg(long)]
    verbose: bool,
    /// Image file to check
    image: PathBuf,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let image = args.image.display();
    let dev = match FileDevice::open_image(&args.image, args.repair) {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!("fsck-dada: {image}: {e}");
            return ExitCode::from(EXIT_ERROR as u8);
        }
    };
    let opts = Options {
        repair: args.repair,
    };
    let report = match check(dev, opts) {
        Ok((report, _)) => report,
        Err(e) => {
            eprintln!("fsck-dada: {image}: {e}");
            return ExitCode::from(EXIT_ERROR as u8);
        }
    };
    for problem in &report.problems {
        println!("{image}: {problem}");
    }
    if args.verbose {
        for note in &report.notes {
            println!("{image}: note: {note}");
        }
        println!(
            "{image}: {} problems found, {} fixed",
            report.found, report.fixed
        );
    }
    ExitCode::from(report.exit_code() as u8)
}
