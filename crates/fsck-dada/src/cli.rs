//! Command-line front end, shared by `fsck-dada` and `dada check`.

use std::path::PathBuf;

use libdada::FileDevice;

use crate::{check, Options, EXIT_ERROR};

/// Options of the check command.
#[derive(clap::Args, Debug)]
pub struct CheckArgs {
    /// Fix the problems found
    #[arg(long)]
    pub repair: bool,
    /// Also print notes and a summary
    #[arg(long)]
    pub verbose: bool,
    /// Image file or device to check
    pub image: PathBuf,
}

/// Checks `args.image`, prints the problems and returns the exit code
/// (0 clean, 1 errors fixed, 4 errors left, 8 runtime error).
pub fn run(args: &CheckArgs, program: &str) -> i32 {
    let image = args.image.display();
    let dev = match FileDevice::open_image(&args.image, args.repair) {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!("{program}: {image}: {e}");
            return EXIT_ERROR;
        }
    };
    let opts = Options {
        repair: args.repair,
    };
    let report = match check(dev, opts) {
        Ok((report, _)) => report,
        Err(e) => {
            eprintln!("{program}: {image}: {e}");
            return EXIT_ERROR;
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
    report.exit_code()
}
