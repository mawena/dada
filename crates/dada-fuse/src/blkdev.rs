//! `fuseblk` mounts (Linux, root only).
//!
//! fuser only mounts plain FUSE, so the mount call is made here and the
//! /dev/fuse descriptor handed to a fuser session.

use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use fuser::{Config, Filesystem, Session, SessionACL};
use nix::fcntl::OFlag;
use nix::mount::{umount2, MntFlags, MsFlags};

use crate::MountOptions;

/// Refuses a device that is mounted or held by another program: opening a
/// block device with O_EXCL fails while it is claimed.
pub(crate) fn check_not_in_use(source: &Path) -> Result<(), String> {
    File::options()
        .read(true)
        .custom_flags(OFlag::O_EXCL.bits())
        .open(source)
        .map(drop)
        .map_err(|e| match e.raw_os_error() {
            Some(code) if code == nix::errno::Errno::EBUSY as i32 => {
                format!("{} is mounted or in use", source.display())
            }
            _ => format!("{}: {e}", source.display()),
        })
}

/// Mounts `filesystem` from the block device `source` and serves it until
/// it is unmounted.
pub(crate) fn mount<FS: Filesystem>(
    filesystem: FS,
    source: &Path,
    mountpoint: &Path,
    opts: &MountOptions,
) -> Result<(), String> {
    let dev_fuse = File::options()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .map_err(|e| format!("cannot open /dev/fuse: {e}"))?;
    // The kernel checks permissions (default_permissions); every user may
    // reach the files (allow_other), as with any other mounted key.
    let data = format!(
        "fd={},rootmode=40000,user_id=0,group_id=0,default_permissions,allow_other,subtype=dada",
        dev_fuse.as_raw_fd()
    );
    let mut flags = MsFlags::MS_NOATIME;
    if !opts.suid {
        flags |= MsFlags::MS_NOSUID;
    }
    if !opts.dev {
        flags |= MsFlags::MS_NODEV;
    }
    if !opts.exec {
        flags |= MsFlags::MS_NOEXEC;
    }
    if opts.read_only {
        flags |= MsFlags::MS_RDONLY;
    }
    nix::mount::mount(
        Some(source),
        mountpoint,
        Some("fuseblk"),
        flags,
        Some(data.as_str()),
    )
    .map_err(|e| format!("mount failed: {e}"))?;

    let session = match Session::from_fd(
        filesystem,
        OwnedFd::from(dev_fuse),
        SessionACL::All,
        Config::default(),
    ) {
        Ok(session) => session,
        Err(e) => {
            let _ = umount2(mountpoint, MntFlags::MNT_DETACH);
            return Err(format!("FUSE handshake failed: {e}"));
        }
    };
    session
        .run()
        .map_err(|e| format!("FUSE session failed: {e}"))
}
