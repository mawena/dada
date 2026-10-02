//! Mounting in the background and unmounting (Unix), and the mount(8)
//! helpers `mount.dada` and `umount.fuseblk.dada`.
//!
//! `dada mount` starts `dada mount --foreground` as a detached process and
//! returns once the volume is mounted. `dada umount` unmounts, then waits
//! for that process to exit: only then is everything written and the
//! volume marked clean, so the key can be pulled out.

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use dada_fuse::MountOptions;

const POLL: Duration = Duration::from_millis(100);
/// How long mounting may take (journal replay included).
const MOUNT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the daemon may take to write everything after unmounting.
const UMOUNT_TIMEOUT: Duration = Duration::from_secs(600);

/// One line of /proc/self/mountinfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub fstype: String,
    pub source: String,
}

impl MountEntry {
    /// Mounts made by dada: `fuse.dada` and `fuseblk.dada`, or plain `fuse`
    /// and `fuseblk` (no subtype) when root mounted an image directly.
    fn is_dada(&self) -> bool {
        matches!(
            self.fstype.as_str(),
            "fuse.dada" | "fuseblk.dada" | "fuse" | "fuseblk"
        )
    }
}

/// Undoes the octal escapes of mountinfo (`\040` for a space...).
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let code = bytes
            .get(i + 1..i + 4)
            .filter(|d| b == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)))
            .and_then(|d| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok());
        match code {
            Some(c) => {
                out.push(c);
                i += 4;
            }
            None => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let mount_point = left.split(' ').nth(4)?;
            let mut right = right.split(' ');
            let fstype = right.next()?;
            let source = right.next()?;
            Some(MountEntry {
                mount_point: PathBuf::from(unescape(mount_point)),
                fstype: unescape(fstype),
                source: unescape(source),
            })
        })
        .collect()
}

/// The current mounts (empty where /proc is missing).
pub fn mounts() -> Vec<MountEntry> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|text| parse_mountinfo(&text))
        .unwrap_or_default()
}

/// The mount whose mount point or source is `target`.
pub fn find_mount(target: &Path) -> Option<MountEntry> {
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    mounts()
        .into_iter()
        .rev()
        .find(|m| m.mount_point == target || Path::new(&m.source) == target)
}

/// Whether something is mounted on `dir`: it then lives on another device
/// than its parent.
pub fn is_mount_point(dir: &Path) -> bool {
    let parent = dir.join("..");
    match (std::fs::metadata(dir), std::fs::metadata(parent)) {
        (Ok(d), Ok(p)) => d.dev() != p.dev() || d.ino() == p.ino(),
        _ => false,
    }
}

/// Looks for `name` in PATH, then in the usual directories (mount helpers
/// run with a minimal environment).
fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/usr/bin", "/sbin", "/bin"].map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// The user who ran `sudo`, if any.
fn sudo_ids() -> Option<(u32, u32)> {
    let id = |name| std::env::var(name).ok()?.parse::<u32>().ok();
    Some((id("SUDO_UID")?, id("SUDO_GID")?))
}

/// `/media/<user>/<label>`, for `dada mount` without a mount point.
pub fn default_mount_point(source: &Path) -> Result<PathBuf, String> {
    if !nix::unistd::geteuid().is_root() {
        return Err("give a mount point, or run with sudo to mount under /media".into());
    }
    let user = std::env::var("SUDO_USER")
        .ok()
        .filter(|u| !u.is_empty() && !u.contains('/'))
        .unwrap_or_else(|| "root".into());
    let id = crate::probe::identify(source)?
        .ok_or_else(|| format!("{}: not a dada volume", source.display()))?;
    let name = if id.label.is_empty() || id.label.contains('/') || id.label.starts_with('.') {
        id.uuid.chars().take(8).collect()
    } else {
        id.label
    };
    Ok(Path::new("/media").join(user).join(name))
}

/// Mounts `source` on `dir` in the background, returning once the volume
/// is mounted.
pub fn mount(source: &Path, dir: &Path, opts: &MountOptions) -> Result<(), String> {
    let source = source
        .canonicalize()
        .map_err(|e| format!("{}: {e}", source.display()))?;
    let mut opts = opts.clone();
    let root = nix::unistd::geteuid().is_root();
    // `sudo dada mount`: the files belong to the user who asked.
    if root && opts.uid.is_none() && opts.gid.is_none() {
        if let Some((uid, gid)) = sudo_ids() {
            opts.uid = Some(uid);
            opts.gid = Some(gid);
        }
    }
    if let Some(m) = find_mount(&source) {
        return Err(format!(
            "{} is already mounted on {}",
            source.display(),
            m.mount_point.display()
        ));
    }
    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    if is_mount_point(&dir) {
        return Err(format!("{} is already a mount point", dir.display()));
    }
    // Report a missing device or a bad volume here rather than from the
    // background process.
    let dev = libdada::FileDevice::open_image(&source, !opts.read_only)
        .map_err(|e| format!("{}: {e}", source.display()))?;
    libdada::Volume::open(dev, true).map_err(|e| format!("{}: {e}", source.display()))?;

    let exe = std::env::current_exe().map_err(|e| format!("cannot find the dada program: {e}"))?;
    let mut args: Vec<OsString> = vec!["mount".into(), "--foreground".into()];
    let options = opts.to_option_string();
    if !options.is_empty() {
        args.push("-o".into());
        args.push(options.into());
    }
    args.push(source.clone().into());
    args.push(dir.clone().into());

    let journal = find_program("systemd-cat");
    let mut chain: Vec<OsString> = Vec::new();
    // As root (udisks, sudo), run in a scope of its own so that stopping the
    // caller's service does not stop the mount.
    if root && Path::new("/run/systemd/system").is_dir() {
        if let Some(run) = find_program("systemd-run") {
            chain.push(run.into());
            chain.extend(["--scope", "--quiet", "--collect"].map(OsString::from));
            chain.push(
                format!(
                    "--description=dada {} on {}",
                    source.display(),
                    dir.display()
                )
                .into(),
            );
        }
    }
    if let Some(cat) = &journal {
        chain.push(cat.into());
        chain.extend(["-t", "dada"].map(OsString::from));
    }
    chain.push(exe.into());
    chain.extend(args);
    let (program, rest) = chain.split_first().ok_or("empty command")?;
    let mut child = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start the mount process: {e}"))?;

    let start = Instant::now();
    loop {
        if is_mount_point(&dir) {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            let hint = if journal.is_some() {
                "see `journalctl -t dada`"
            } else {
                "run `dada mount --foreground` to see why"
            };
            return Err(format!("mounting failed ({status}); {hint}"));
        }
        if start.elapsed() > MOUNT_TIMEOUT {
            return Err("mounting timed out".into());
        }
        std::thread::sleep(POLL);
    }
}

/// Processes serving the mount on `mount_point`: dada programs whose
/// arguments include it.
fn daemons(mount_point: &Path) -> Vec<u32> {
    let own = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let wanted = mount_point.as_os_str().as_encoded_bytes();
    entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != own)
        .filter(|pid| {
            let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                return false;
            };
            let mut args = cmdline.split(|&b| b == 0);
            let is_dada = args.next().is_some_and(|argv0| {
                let name = argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0);
                name.starts_with(b"dada")
            });
            is_dada && args.any(|a| a == wanted)
        })
        .collect()
}

/// Unmounts the dada volume mounted on (or from) `target`, then waits until
/// it is completely written.
pub fn umount(target: &Path) -> Result<(), String> {
    let entry = find_mount(target).ok_or_else(|| format!("{}: not mounted", target.display()))?;
    if !entry.is_dada() {
        return Err(format!(
            "{} is a {} mount, not dada",
            entry.mount_point.display(),
            entry.fstype
        ));
    }
    let pids = daemons(&entry.mount_point);
    let output = if nix::unistd::geteuid().is_root() {
        // -i: no helper, this may be one.
        Command::new(find_program("umount").ok_or("umount not found")?)
            .arg("-i")
            .arg(&entry.mount_point)
            .output()
    } else {
        let fusermount = find_program("fusermount3")
            .or_else(|| find_program("fusermount"))
            .ok_or("fusermount3 not found")?;
        Command::new(fusermount)
            .arg("-u")
            .arg(&entry.mount_point)
            .output()
    }
    .map_err(|e| format!("cannot unmount: {e}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "cannot unmount {}: {}",
            entry.mount_point.display(),
            message.trim()
        ));
    }

    let start = Instant::now();
    let mut told = false;
    while pids
        .iter()
        .any(|pid| Path::new(&format!("/proc/{pid}")).exists())
    {
        if !told && start.elapsed() > Duration::from_secs(1) {
            eprintln!("writing the last changes to {}...", entry.source);
            told = true;
        }
        if start.elapsed() > UMOUNT_TIMEOUT {
            return Err("the volume is still being written; do not remove it yet".into());
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

/// Arguments of a mount(8) helper: `<spec> <dir> [-sfnvrw] [-o opts] [-t type]`.
#[derive(Debug, Default, PartialEq, Eq)]
struct HelperArgs {
    positional: Vec<String>,
    options: Vec<String>,
    sloppy: bool,
    fake: bool,
    read_only: bool,
}

fn parse_helper_args(args: &[String]) -> Result<HelperArgs, String> {
    let mut out = HelperArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty()) else {
            out.positional.push(arg.clone());
            continue;
        };
        for (i, c) in flags.char_indices() {
            match c {
                's' => out.sloppy = true,
                'f' => out.fake = true,
                'r' => out.read_only = true,
                'n' | 'v' | 'w' | 'l' => {}
                'o' | 't' | 'N' => {
                    let rest = flags.get(i + 1..).unwrap_or("");
                    let value = if rest.is_empty() {
                        it.next().cloned().ok_or(format!("-{c} needs a value"))?
                    } else {
                        rest.to_string()
                    };
                    match c {
                        'o' => out.options.push(value),
                        'N' => return Err("mount namespaces are not supported".into()),
                        _ => {}
                    }
                    break;
                }
                _ => return Err(format!("unknown flag -{c}")),
            }
        }
    }
    Ok(out)
}

/// `mount.dada <spec> <dir> [-o options]`, called by mount(8), hence by
/// udisks and fstab. Exit codes follow mount(8).
pub fn mount_helper(args: &[String]) -> ExitCode {
    let result = parse_helper_args(args).and_then(|h| {
        let [spec, dir] = h.positional.as_slice() else {
            return Err("usage: mount.dada <device> <directory> [-o options]".into());
        };
        let mut opts = dada_fuse::parse_options(&h.options, h.sloppy)?;
        opts.read_only |= h.read_only;
        if h.fake {
            return Ok(());
        }
        mount(Path::new(spec), Path::new(dir), &opts)
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mount.dada: {e}");
            ExitCode::from(32)
        }
    }
}

/// `umount.fuseblk.dada <dir>`, called by umount(8).
pub fn umount_helper(args: &[String]) -> ExitCode {
    let result = parse_helper_args(args).and_then(|h| match h.positional.as_slice() {
        [target] => umount(Path::new(target)),
        _ => Err("usage: umount.dada <directory>".into()),
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("umount: {e}");
            ExitCode::from(32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo() {
        let text = "\
36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 - ext3 /dev/root rw,errors=continue
91 30 8:1 / /media/me/MA\\040CLE rw,nosuid,nodev,noatime shared:1 - fuseblk.dada /dev/sda1 rw,user_id=0
92 30 0:55 / /home/me/mnt rw - fuse.dada /tmp/a\\040b.img rw
bad line";
        let mounts = parse_mountinfo(text);
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[1].mount_point, Path::new("/media/me/MA CLE"));
        assert_eq!(mounts[1].fstype, "fuseblk.dada");
        assert_eq!(mounts[1].source, "/dev/sda1");
        assert!(mounts[1].is_dada() && mounts[2].is_dada() && !mounts[0].is_dada());
        assert_eq!(mounts[2].source, "/tmp/a b.img");
    }

    #[test]
    fn escapes() {
        assert_eq!(unescape("a\\040b\\134c"), "a b\\c");
        assert_eq!(unescape("a\\04"), "a\\04");
        assert_eq!(unescape("a\\999"), "a\\999");
        assert_eq!(unescape("é"), "é");
    }

    #[test]
    fn helper_args() {
        let args: Vec<String> = [
            "/dev/sda1",
            "/media/me/KEY",
            "-o",
            "nosuid,uhelper=udisks2",
            "-sn",
            "-tdada",
        ]
        .map(String::from)
        .to_vec();
        let h = parse_helper_args(&args).unwrap();
        assert_eq!(h.positional, ["/dev/sda1", "/media/me/KEY"]);
        assert_eq!(h.options, ["nosuid,uhelper=udisks2"]);
        assert!(h.sloppy && !h.fake);
        let h = parse_helper_args(&["-oro".into(), "-f".into()]).unwrap();
        assert_eq!(h.options, ["ro"]);
        assert!(h.fake);
        assert!(parse_helper_args(&["-o".into()]).is_err());
        assert!(parse_helper_args(&["-N".into(), "1".into()]).is_err());
        assert!(parse_helper_args(&["-x".into()]).is_err());
    }

    #[test]
    fn mount_points() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_mount_point(dir.path()));
        assert!(is_mount_point(Path::new("/")));
    }
}
