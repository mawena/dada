//! FUSE adapter for dada (Linux, macOS), shared by `dada-fuse` and the
//! `dada` command.
//!
//! As root on Linux, a block device is mounted as `fuseblk`, like ntfs-3g
//! does: the kernel then ties the mount to the device (udisks and file
//! managers recognise it), and unmounting waits for the daemon's answer.
//! Everything else is mounted as plain FUSE.

use std::path::Path;

#[cfg(all(feature = "fuse", target_os = "linux"))]
mod blkdev;
#[cfg(feature = "fuse")]
mod fs;

/// Mount options, as given with `-o`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountOptions {
    pub read_only: bool,
    pub allow_other: bool,
    /// Show every file as owned by this user (and `gid`), without changing
    /// the disk.
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub exec: bool,
    pub suid: bool,
    pub dev: bool,
}

impl Default for MountOptions {
    fn default() -> Self {
        MountOptions {
            read_only: false,
            allow_other: false,
            uid: None,
            gid: None,
            exec: true,
            suid: false,
            dev: false,
        }
    }
}

/// Options that mount(8), fstab and udisks pass but that change nothing
/// here: permissions are always checked by the kernel, and times are never
/// updated on read.
const IGNORED: &[&str] = &[
    "defaults",
    "auto",
    "noauto",
    "user",
    "users",
    "nouser",
    "owner",
    "group",
    "nofail",
    "_netdev",
    "async",
    "atime",
    "noatime",
    "relatime",
    "norelatime",
    "strictatime",
    "diratime",
    "nodiratime",
    "lazytime",
    "nolazytime",
    "silent",
    "loud",
    "default_permissions",
];
const IGNORED_PREFIXES: &[&str] = &["uhelper=", "helper=", "comment=", "x-"];

/// Parses comma-separated options. With `sloppy`, unknown options are
/// ignored instead of refused (`mount -s`).
pub fn parse_options(raw: &[String], sloppy: bool) -> Result<MountOptions, String> {
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
            None => match option {
                "ro" => opts.read_only = true,
                "rw" => opts.read_only = false,
                "allow_other" => opts.allow_other = true,
                "exec" => opts.exec = true,
                "noexec" => opts.exec = false,
                "suid" => opts.suid = true,
                "nosuid" => opts.suid = false,
                "dev" => opts.dev = true,
                "nodev" => opts.dev = false,
                o if IGNORED.contains(&o) => {}
                o if IGNORED_PREFIXES.iter().any(|p| o.starts_with(p)) => {}
                _ if sloppy => {}
                _ => return Err(format!("unknown option {option:?}")),
            },
            Some(("uid", v)) => opts.uid = Some(number(v)?),
            Some(("gid", v)) => opts.gid = Some(number(v)?),
            Some(_) if IGNORED_PREFIXES.iter().any(|p| option.starts_with(p)) => {}
            Some(_) if sloppy => {}
            Some(_) => return Err(format!("unknown option {option:?}")),
        }
    }
    Ok(opts)
}

impl MountOptions {
    /// The options that differ from the defaults, comma-separated, as
    /// `parse_options` reads them back.
    pub fn to_option_string(&self) -> String {
        let mut out = Vec::new();
        if self.read_only {
            out.push("ro".to_string());
        }
        if self.allow_other {
            out.push("allow_other".into());
        }
        if let Some(uid) = self.uid {
            out.push(format!("uid={uid}"));
        }
        if let Some(gid) = self.gid {
            out.push(format!("gid={gid}"));
        }
        if !self.exec {
            out.push("noexec".into());
        }
        if self.suid {
            out.push("suid".into());
        }
        if self.dev {
            out.push("dev".into());
        }
        out.join(",")
    }
}

/// Whether the process runs as root.
#[cfg(all(feature = "fuse", unix))]
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Mounts `source` on `mountpoint` and serves requests until it is
/// unmounted; the volume is then closed cleanly.
#[cfg(feature = "fuse")]
pub fn mount(source: &Path, mountpoint: &Path, opts: &MountOptions) -> Result<(), String> {
    use fuser::{Config, MountOption, SessionACL};
    use libdada::{FileDevice, Volume};

    let image = source.display();
    let mut opts = opts.clone();
    // Root mounts for someone else: let them in, the kernel still checks
    // permissions.
    if is_root() {
        opts.allow_other = true;
    }
    #[cfg(target_os = "linux")]
    let blkdev = is_root() && is_block_device(source);
    #[cfg(target_os = "linux")]
    if blkdev {
        blkdev::check_not_in_use(source)?;
    }

    let dev =
        FileDevice::open_image(source, !opts.read_only).map_err(|e| format!("{image}: {e}"))?;
    let vol = Volume::open(dev, opts.read_only).map_err(|e| format!("{image}: {e}"))?;
    // Showing everything as one user needs both ids; the missing one is the
    // caller's own.
    let owner = match (opts.uid, opts.gid) {
        (None, None) => None,
        (uid, gid) => Some((
            uid.unwrap_or_else(|| nix::unistd::getuid().as_raw()),
            gid.unwrap_or_else(|| nix::unistd::getgid().as_raw()),
        )),
    };
    let filesystem = fs::DadaFs::new(vol, fs::Presentation { owner });
    log::info!("mounting {image} on {}", mountpoint.display());

    #[cfg(target_os = "linux")]
    if blkdev {
        return blkdev::mount(filesystem, source, mountpoint, &opts);
    }

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName(source.display().to_string()),
        MountOption::Subtype("dada".into()),
        MountOption::DefaultPermissions,
        MountOption::NoAtime,
        if opts.read_only {
            MountOption::RO
        } else {
            MountOption::RW
        },
        if opts.exec {
            MountOption::Exec
        } else {
            MountOption::NoExec
        },
    ];
    if opts.suid {
        config.mount_options.push(MountOption::Suid);
    }
    if opts.dev {
        config.mount_options.push(MountOption::Dev);
    }
    if opts.allow_other {
        config.acl = SessionACL::All;
    }
    fuser::mount(filesystem, mountpoint, &config).map_err(|e| format!("mount failed: {e}"))
}

#[cfg(not(feature = "fuse"))]
pub fn mount(_source: &Path, _mountpoint: &Path, _opts: &MountOptions) -> Result<(), String> {
    Err("this build has no FUSE support; rebuild with `--features fuse`".into())
}

#[cfg(all(feature = "fuse", target_os = "linux"))]
fn is_block_device(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options() {
        let parsed =
            parse_options(&["ro,allow_other".into(), "uid=1000,gid=100".into()], false).unwrap();
        assert_eq!(
            parsed,
            MountOptions {
                read_only: true,
                allow_other: true,
                uid: Some(1000),
                gid: Some(100),
                ..MountOptions::default()
            }
        );
        assert_eq!(parse_options(&[], false).unwrap(), MountOptions::default());
        assert!(parse_options(&["uid=abc".into()], false).is_err());
        assert!(parse_options(&["sync".into()], false).is_err());
        assert!(parse_options(&["sync,foo=1".into()], true).is_ok());
    }

    #[test]
    fn options_from_mount_and_udisks() {
        // What udisks passes through mount(8).
        let parsed = parse_options(
            &["rw,nosuid,nodev,relatime,uid=1000,gid=1000,uhelper=udisks2".into()],
            false,
        )
        .unwrap();
        assert_eq!(parsed.uid, Some(1000));
        assert!(!parsed.read_only && !parsed.suid && !parsed.dev && parsed.exec);
        let parsed = parse_options(&["noexec,suid,dev,x-gvfs-show,nofail".into()], false).unwrap();
        assert!(!parsed.exec && parsed.suid && parsed.dev);
    }

    #[test]
    fn option_string_round_trip() {
        for opts in [
            MountOptions::default(),
            MountOptions {
                read_only: true,
                allow_other: true,
                uid: Some(1),
                gid: Some(2),
                exec: false,
                suid: true,
                dev: true,
            },
        ] {
            let s = opts.to_option_string();
            assert_eq!(parse_options(&[s], false).unwrap(), opts);
        }
        assert_eq!(MountOptions::default().to_option_string(), "");
    }
}
