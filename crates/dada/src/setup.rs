//! `dada setup`: installs the Linux system integration, so that dada
//! volumes are recognised (lsblk, file managers), mounted automatically
//! when plugged in, and mountable with `mount -t dada`.
//!
//! - the program itself in /usr/local/bin;
//! - the mount(8) helpers `mount.dada` and `umount.fuseblk.dada` (links);
//! - a udev rule that identifies dada volumes with `dada probe --udev`;
//! - udisks options so that the files belong to the user who plugged the key.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = "/usr/local/bin/dada";
const HELPERS: [&str; 3] = [
    "/sbin/mount.dada",
    "/sbin/umount.fuseblk.dada",
    "/sbin/umount.fuse.dada",
];
const RULES: &str = "/etc/udev/rules.d/61-dada.rules";
const UDISKS_CONF: &str = "/etc/udisks2/mount_options.conf";

const BEGIN: &str = "# >>> dada (dada setup)";
const END: &str = "# <<< dada";

fn rules() -> String {
    format!(
        "# Identifies dada filesystems for lsblk, udisks and file managers.
# Installed by `dada setup`, removed by `dada setup --uninstall`.
ACTION==\"remove\", GOTO=\"dada_end\"
SUBSYSTEM!=\"block\", GOTO=\"dada_end\"
ENV{{ID_FS_TYPE}}==\"?*\", GOTO=\"dada_end\"
ENV{{ID_PART_TABLE_TYPE}}==\"?*\", GOTO=\"dada_end\"
IMPORT{{program}}=\"{BIN} probe --udev $devnode\"
ENV{{ID_FS_TYPE}}!=\"dada\", GOTO=\"dada_end\"
ENV{{ID_FS_UUID_ENC}}==\"?*\", SYMLINK+=\"disk/by-uuid/$env{{ID_FS_UUID_ENC}}\"
ENV{{ID_FS_LABEL_ENC}}==\"?*\", SYMLINK+=\"disk/by-label/$env{{ID_FS_LABEL_ENC}}\"
LABEL=\"dada_end\"
"
    )
}

fn udisks_block() -> String {
    format!("{BEGIN}\ndada_defaults=uid=$UID,gid=$GID\ndada_allow=uid=$UID,gid=$GID\n{END}\n")
}

/// The udisks configuration with the dada options added to `[defaults]`.
fn add_udisks_options(current: Option<&str>) -> String {
    let current = current.map(remove_udisks_options).unwrap_or_default();
    let block = udisks_block();
    let mut out = String::new();
    let mut added = false;
    for line in current.lines() {
        out.push_str(line);
        out.push('\n');
        if !added && line.trim() == "[defaults]" {
            out.push_str(&block);
            added = true;
        }
    }
    if added {
        out
    } else {
        // Keys before the first group would be invalid: the new group goes
        // first.
        format!(
            "[defaults]\n{block}{}{out}",
            if out.is_empty() { "" } else { "\n" }
        )
    }
}

/// The udisks configuration without the dada options.
fn remove_udisks_options(current: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in current.lines() {
        if line == BEGIN {
            inside = true;
        } else if inside && line == END {
            inside = false;
        } else if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn run(program: &str, args: &[&str]) {
    match Command::new(program).args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("warning: {program} {} failed ({s})", args.join(" ")),
        Err(e) => eprintln!("warning: cannot run {program}: {e}"),
    }
}

fn reload_udev() {
    run("udevadm", &["control", "--reload"]);
    run(
        "udevadm",
        &["trigger", "--action=change", "--subsystem-match=block"],
    );
}

/// Whether `path` is a link to the installed program.
fn is_our_link(path: &Path) -> bool {
    fs::read_link(path).is_ok_and(|target| target == Path::new(BIN))
}

fn write(path: &str, content: &str) -> Result<(), String> {
    if let Some(dir) = Path::new(path).parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    fs::write(path, content).map_err(|e| format!("cannot write {path}: {e}"))?;
    println!("wrote     {path}");
    Ok(())
}

fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe);
    if exe != Path::new(BIN) {
        let tmp = PathBuf::from(format!("{BIN}.new"));
        fs::copy(&exe, &tmp).map_err(|e| format!("cannot copy to {}: {e}", tmp.display()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        fs::rename(&tmp, BIN).map_err(|e| format!("cannot install {BIN}: {e}"))?;
        println!("installed {BIN}");
    }
    for helper in HELPERS {
        let path = Path::new(helper);
        if path.symlink_metadata().is_ok() {
            if !is_our_link(path) {
                return Err(format!("{helper} exists and does not belong to dada"));
            }
            fs::remove_file(path).map_err(|e| format!("{helper}: {e}"))?;
        }
        std::os::unix::fs::symlink(BIN, path)
            .map_err(|e| format!("cannot create {helper}: {e}"))?;
        println!("linked    {helper} -> {BIN}");
    }
    write(RULES, &rules())?;
    let current = fs::read_to_string(UDISKS_CONF).ok();
    write(UDISKS_CONF, &add_udisks_options(current.as_deref()))?;
    reload_udev();
    println!();
    println!("Done. Unplug and plug the key again: it is mounted automatically.");
    println!("By hand: dada mount <device> [<directory>], dada umount <directory>.");
    Ok(())
}

fn uninstall() -> Result<(), String> {
    for helper in HELPERS {
        if is_our_link(Path::new(helper)) {
            fs::remove_file(helper).map_err(|e| format!("{helper}: {e}"))?;
            println!("removed   {helper}");
        }
    }
    if Path::new(RULES).exists() {
        fs::remove_file(RULES).map_err(|e| format!("{RULES}: {e}"))?;
        println!("removed   {RULES}");
    }
    if let Ok(current) = fs::read_to_string(UDISKS_CONF) {
        let cleaned = remove_udisks_options(&current);
        if cleaned != current {
            if cleaned.trim() == "[defaults]" {
                // Created by `dada setup`.
                fs::remove_file(UDISKS_CONF).map_err(|e| format!("{UDISKS_CONF}: {e}"))?;
                println!("removed   {UDISKS_CONF}");
            } else {
                write(UDISKS_CONF, &cleaned)?;
            }
        }
    }
    reload_udev();
    if Path::new(BIN).exists() {
        fs::remove_file(BIN).map_err(|e| format!("{BIN}: {e}"))?;
        println!("removed   {BIN}");
    }
    Ok(())
}

pub fn setup(uninstall_it: bool) -> Result<(), String> {
    if !nix::unistd::geteuid().is_root() {
        return Err("run it as root: sudo dada setup".into());
    }
    if uninstall_it {
        uninstall()
    } else {
        install()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udisks_new_file() {
        let conf = add_udisks_options(None);
        assert!(conf.starts_with("[defaults]\n# >>> dada"));
        assert!(conf.contains("dada_defaults=uid=$UID,gid=$GID\n"));
        assert_eq!(remove_udisks_options(&conf).trim(), "[defaults]");
    }

    #[test]
    fn udisks_existing_defaults() {
        let current = "# mine\n[defaults]\nntfs_defaults=uid=$UID\n[/dev/sdz]\nvfat_defaults=ro\n";
        let conf = add_udisks_options(Some(current));
        let defaults = conf.find("[defaults]").unwrap();
        let ours = conf.find(BEGIN).unwrap();
        let device = conf.find("[/dev/sdz]").unwrap();
        assert!(defaults < ours && ours < device);
        // Installing twice adds the options once.
        let again = add_udisks_options(Some(&conf));
        assert_eq!(again, conf);
        assert_eq!(remove_udisks_options(&conf), current);
    }

    #[test]
    fn udisks_without_defaults() {
        let current = "[/dev/sdz]\nvfat_defaults=ro\n";
        let conf = add_udisks_options(Some(current));
        assert!(conf.starts_with("[defaults]\n"));
        assert!(conf.ends_with(current));
        assert_eq!(
            remove_udisks_options(&conf).trim(),
            "[defaults]\n\n[/dev/sdz]\nvfat_defaults=ro".trim()
        );
    }

    #[test]
    fn rule_calls_the_installed_program() {
        assert!(rules().contains("IMPORT{program}=\"/usr/local/bin/dada probe --udev $devnode\""));
    }
}
