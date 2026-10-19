//! Linux and macOS integration, for the current user: Sayon lives in
//! `~/.local/share/sayon` and a link in `~/.local/bin` puts `sayon` on the
//! usual per-user `PATH`. Shell profiles are not edited; when that folder is
//! not on `PATH`, the installer says how to add it.

use std::path::{Path, PathBuf};

pub const EXECUTABLE_SUFFIX: &str = "";
pub const UNINSTALLER: &str = "uninstall";
/// Where the system lists installed programs, for removing them; here the
/// uninstaller is the only way.
pub const APP_SETTINGS: Option<&str> = None;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn default_prefix() -> Option<PathBuf> {
    Some(home()?.join(".local").join("share").join("sayon"))
}

pub fn bin_directory(_prefix: &Path) -> PathBuf {
    home()
        .map(|home| home.join(".local").join("bin"))
        .unwrap_or_else(|| PathBuf::from("bin"))
}

fn link(prefix: &Path) -> PathBuf {
    bin_directory(prefix).join("sayon")
}

pub fn changes(prefix: &Path, modify_path: bool) -> Vec<String> {
    if modify_path {
        vec![format!(
            "links {} to the installed `sayon`",
            link(prefix).display()
        )]
    } else {
        Vec::new()
    }
}

pub fn register(
    prefix: &Path,
    _version: &str,
    _bytes: u64,
    modify_path: bool,
) -> Result<(), String> {
    if !modify_path {
        return Ok(());
    }
    let link = link(prefix);
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    if std::fs::symlink_metadata(&link).is_ok() {
        let points_here = std::fs::read_link(&link).is_ok_and(|target| target.starts_with(prefix));
        if !points_here {
            return Err(format!(
                "{} already exists; remove it or install with --no-path",
                link.display()
            ));
        }
        std::fs::remove_file(&link)
            .map_err(|e| format!("cannot replace {}: {e}", link.display()))?;
    }
    std::os::unix::fs::symlink(prefix.join("bin").join("sayon"), &link)
        .map_err(|e| format!("cannot create {}: {e}", link.display()))?;
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir == bin_directory(prefix)));
    if !on_path {
        println!(
            "Add {} to your PATH, for example with this line in ~/.profile:\n    export PATH=\"$HOME/.local/bin:$PATH\"",
            bin_directory(prefix).display()
        );
    }
    Ok(())
}

pub fn unregister(prefix: &Path) -> Result<(), String> {
    let link = link(prefix);
    let ours = std::fs::read_link(&link).is_ok_and(|target| target.starts_with(prefix));
    if ours {
        std::fs::remove_file(&link)
            .map_err(|e| format!("cannot remove {}: {e}", link.display()))?;
    }
    Ok(())
}

pub fn registered_prefix() -> Option<PathBuf> {
    None
}

pub fn owns_console() -> bool {
    false
}

/// Programs in `bin` must be executable; archives made on Windows do not
/// record permissions.
pub fn make_executable_if_needed(path: &Path, relative: &str) {
    use std::os::unix::fs::PermissionsExt;
    if relative.starts_with("bin/") || relative == UNINSTALLER {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
}

/// A running program can be deleted here, so nothing needs moving.
pub fn move_running_uninstaller(_prefix: &Path) -> Option<PathBuf> {
    None
}

/// The running uninstaller goes at once, and then its folder, which held
/// nothing else.
pub fn delete_after_exit(file: &Path, folder: Option<&Path>) {
    let _ = std::fs::remove_file(file);
    if let Some(folder) = folder {
        let _ = std::fs::remove_dir(folder);
    }
}
