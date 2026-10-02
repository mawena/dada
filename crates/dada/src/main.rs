//! dada: one command to format, check, mount and inspect dada volumes.
//!
//! Installed as `mount.dada` or `umount.fuseblk.dada` (see `dada setup`),
//! it acts as the mount(8) helper of that name.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[cfg(unix)]
mod mount;
mod probe;
#[cfg(target_os = "linux")]
mod setup;

#[derive(Parser)]
#[command(
    name = "dada",
    version,
    about = "Format, check, mount and inspect dada volumes"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Format an image file or a device (asks before erasing data)
    Format(mkfs_dada::FormatArgs),
    /// Check a volume, and repair it with --repair
    #[command(alias = "fsck")]
    Check(fsck_dada::cli::CheckArgs),
    /// Mount a volume; it stays mounted until `dada umount`
    Mount(MountArgs),
    /// Unmount a volume and wait until everything is written
    #[command(alias = "unmount")]
    Umount {
        /// Mount point or device
        target: PathBuf,
    },
    /// Tell whether a device or image holds a dada volume
    Probe {
        /// Print udev properties (used by the udev rule)
        #[arg(long)]
        udev: bool,
        device: PathBuf,
    },
    /// Install automatic mounting and `mount -t dada` (Linux, as root)
    Setup {
        /// Remove what `dada setup` installed
        #[arg(long)]
        uninstall: bool,
    },
    #[command(flatten)]
    Inspect(dadactl::Command),
}

#[derive(clap::Args)]
struct MountArgs {
    /// Mount options, comma-separated: ro, uid=N, gid=N, allow_other, noexec
    #[arg(short = 'o', value_name = "OPTIONS")]
    options: Vec<String>,
    /// Stay in the foreground and log to the terminal until unmounted
    #[arg(long, short = 'f')]
    foreground: bool,
    /// Image file or device
    source: PathBuf,
    /// Mount point (default with sudo: /media/<user>/<label>)
    mountpoint: Option<PathBuf>,
}

fn main() -> ExitCode {
    // As a mount(8) helper, the program name selects the role.
    let mut args = std::env::args();
    let name = args
        .next()
        .map(|a| a.rsplit('/').next().unwrap_or(&a).to_string())
        .unwrap_or_default();
    #[cfg(unix)]
    {
        let rest: Vec<String> = args.collect();
        if name == "mount.dada" {
            init_log("warn");
            return mount::mount_helper(&rest);
        }
        if name.starts_with("umount.") && name.ends_with(".dada") {
            init_log("warn");
            return mount::umount_helper(&rest);
        }
    }
    #[cfg(not(unix))]
    let _ = (args, name);

    match Cli::parse().command {
        Command::Check(args) => {
            #[cfg(unix)]
            if let Some(m) = mount::find_mount(&args.image) {
                eprintln!(
                    "dada: {} is mounted on {}; unmount it first",
                    args.image.display(),
                    m.mount_point.display()
                );
                return ExitCode::from(8);
            }
            let code = fsck_dada::cli::run(&args, "dada check");
            ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX))
        }
        Command::Probe { udev, device } => probe(&device, udev),
        command => match run(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("dada: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

#[cfg(unix)]
fn init_log(default: &str) {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default)).init();
}

fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Format(args) => format(&args),
        Command::Mount(args) => mount_command(&args),
        Command::Umount { target } => umount_command(&target),
        Command::Setup { uninstall } => setup_command(uninstall),
        Command::Inspect(command) => dadactl::run(command),
        Command::Check(_) | Command::Probe { .. } => Ok(()),
    }
}

/// Asks a yes/no question on the terminal; no terminal means no.
fn confirm(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(
        answer.trim().to_lowercase().as_str(),
        "y" | "yes" | "o" | "oui"
    )
}

fn format(args: &mkfs_dada::FormatArgs) -> Result<(), String> {
    #[cfg(unix)]
    if let Some(m) = mount::find_mount(&args.target) {
        return Err(format!(
            "{} is mounted on {}; unmount it first",
            args.target.display(),
            m.mount_point.display()
        ));
    }
    if let Ok(Some(id)) = probe::identify(&args.target) {
        let label = if id.label.is_empty() {
            String::new()
        } else {
            format!(" \"{}\"", id.label)
        };
        eprintln!(
            "{} holds the dada volume{label} ({}).",
            args.target.display(),
            human_size(id.total_blocks.saturating_mul(u64::from(id.block_size)))
        );
    }
    mkfs_dada::run(args, &mut |question| confirm(question))
}

fn human_size(bytes: u64) -> String {
    let units = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    match units.get(unit) {
        Some(&"bytes") | None => format!("{bytes} bytes"),
        Some(u) => format!("{value:.1} {u}"),
    }
}

fn probe(device: &Path, udev: bool) -> ExitCode {
    match probe::identify(device) {
        Ok(Some(id)) if udev => {
            print!("{}", probe::udev_properties(&id));
            ExitCode::SUCCESS
        }
        Ok(Some(id)) => {
            println!("{}: dada filesystem", device.display());
            println!("  label       {}", id.label);
            println!("  uuid        {}", id.uuid);
            println!(
                "  size        {}",
                human_size(id.total_blocks.saturating_mul(u64::from(id.block_size)))
            );
            ExitCode::SUCCESS
        }
        Ok(None) => {
            if !udev {
                println!("{}: not a dada filesystem", device.display());
            }
            ExitCode::from(2)
        }
        Err(e) => {
            if !udev {
                eprintln!("dada: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
fn mount_command(args: &MountArgs) -> Result<(), String> {
    let opts = dada_fuse::parse_options(&args.options, false)?;
    let dir = match &args.mountpoint {
        Some(dir) => dir.clone(),
        None => mount::default_mount_point(&args.source)?,
    };
    if args.foreground {
        init_log("info");
        return dada_fuse::mount(&args.source, &dir, &opts);
    }
    mount::mount(&args.source, &dir, &opts)?;
    println!("{} mounted on {}", args.source.display(), dir.display());
    println!("unmount with: dada umount {}", dir.display());
    Ok(())
}

#[cfg(not(unix))]
fn mount_command(_args: &MountArgs) -> Result<(), String> {
    Err("on Windows, mount with dada-winfsp <image> <letter:>".into())
}

#[cfg(unix)]
fn umount_command(target: &Path) -> Result<(), String> {
    mount::umount(target)?;
    println!("{} unmounted; it can be removed", target.display());
    Ok(())
}

#[cfg(not(unix))]
fn umount_command(_target: &Path) -> Result<(), String> {
    Err("on Windows, press Enter in the dada-winfsp window to unmount".into())
}

#[cfg(target_os = "linux")]
fn setup_command(uninstall: bool) -> Result<(), String> {
    setup::setup(uninstall)
}

#[cfg(not(target_os = "linux"))]
fn setup_command(_uninstall: bool) -> Result<(), String> {
    Err("dada setup is only available on Linux".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(31_457_280_000), "29.3 GiB");
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["dada", "mount", "-o", "ro", "/dev/sda1"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Mount(MountArgs {
                mountpoint: None,
                ..
            })
        ));
        let cli = Cli::try_parse_from(["dada", "ls", "k.img", "/"]).unwrap();
        assert!(matches!(cli.command, Command::Inspect(_)));
        assert!(Cli::try_parse_from(["dada", "fsck", "--repair", "k.img"]).is_ok());
    }
}
