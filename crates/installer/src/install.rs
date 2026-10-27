//! Putting the Sayon files in place and taking them away again.
//!
//! An installation directory holds a marker file naming the installed
//! version. The installer only replaces or removes a directory that has the
//! marker, so a mistyped `--prefix` can never delete someone's files. Files
//! are unpacked into a sibling staging directory first and swapped in at the
//! end, so an interrupted installation leaves the previous one intact.
//!
//! Removing a directory takes its marker last, so one that could not be
//! removed completely, because a program still had a file of it open, is
//! still recognized and removed by the next installation. Until then the
//! staging and previous directories take the next free numbered name, so
//! that a leftover never stops an installation.

use crate::archive::Entry;
use std::fs;
use std::path::{Path, PathBuf};

/// The file that marks a directory as a Sayon installation.
pub const MARKER: &str = ".sayon-installation";

/// The installed version, when `prefix` holds a Sayon installation.
pub fn installed_version(prefix: &Path) -> Option<String> {
    let text = fs::read_to_string(prefix.join(MARKER)).ok()?;
    text.lines().find_map(|line| {
        line.strip_prefix("version = ")
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

/// Checks that installing into `prefix` replaces nothing but Sayon.
pub fn check_destination(prefix: &Path) -> Result<(), String> {
    if !prefix.exists() {
        return Ok(());
    }
    if !prefix.is_dir() {
        return Err(format!("{} exists and is not a folder", prefix.display()));
    }
    let empty = fs::read_dir(prefix)
        .map_err(|e| format!("cannot read {}: {e}", prefix.display()))?
        .next()
        .is_none();
    if empty || prefix.join(MARKER).is_file() {
        Ok(())
    } else {
        Err(format!(
            "{} already contains other files; choose an empty folder or one that holds Sayon",
            prefix.display()
        ))
    }
}

/// The files to install, with the archive's top-level folder removed.
pub fn layout(entries: Vec<Entry>) -> Vec<Entry> {
    let first = |path: &str| path.split('/').next().unwrap_or("").to_string();
    let top = entries.first().map(|e| first(&e.path));
    let shared = top.as_ref().is_some_and(|top| {
        entries
            .iter()
            .all(|e| e.path.contains('/') && first(&e.path) == *top)
    });
    if !shared {
        return entries;
    }
    entries
        .into_iter()
        .map(|entry| Entry {
            path: entry.path.split_once('/').expect("nested").1.to_string(),
            data: entry.data,
        })
        .collect()
}

/// A sibling of `prefix` with a suffix added to its name.
fn sibling(prefix: &Path, suffix: &str) -> PathBuf {
    let mut name = prefix.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    prefix.with_file_name(name)
}

/// The folders an installation works in next to `prefix`, and how many
/// numbered ones of each it tries when the first are taken.
const WORK_FOLDERS: [&str; 2] = [".installing", ".previous"];
const NUMBERED: u32 = 20;

/// The `n`th folder next to `prefix` with `suffix`: `Sayon.previous`,
/// then `Sayon.previous-2`, and so on.
fn work_folder(prefix: &Path, suffix: &str, n: u32) -> PathBuf {
    match n {
        1 => sibling(prefix, suffix),
        n => sibling(prefix, &format!("{suffix}-{n}")),
    }
}

/// Removes a folder the installer made earlier, as far as it can now: one
/// that holds the marker, or an empty one. Anything else is left alone.
/// Whether the folder is gone.
fn discard(dir: &Path) -> bool {
    if !dir.exists() {
        return true;
    }
    if fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none()) {
        return fs::remove_dir(dir).is_ok();
    }
    dir.join(MARKER).is_file() && remove(dir, None).unwrap_or(false)
}

/// Removes what earlier installations left next to `prefix`, as far as it
/// can now.
pub fn clear_leftovers(prefix: &Path) {
    for suffix in WORK_FOLDERS {
        for n in 1..=NUMBERED {
            discard(&work_folder(prefix, suffix, n));
        }
    }
}

/// The first folder next to `prefix` with `suffix` that is free.
fn free_folder(prefix: &Path, suffix: &str) -> Result<PathBuf, String> {
    (1..=NUMBERED)
        .map(|n| work_folder(prefix, suffix, n))
        .find(|dir| !dir.exists())
        .ok_or_else(|| {
            format!(
                "{} and the {NUMBERED} numbered folders after it are in the way; remove those you do not need and try again",
                sibling(prefix, suffix).display()
            )
        })
}

/// Unpacks `files` and a copy of the installer (as the uninstaller) into
/// `prefix`, replacing a previous installation. Returns the bytes written.
pub fn unpack(
    files: &[Entry],
    prefix: &Path,
    version: &str,
    uninstaller: Option<(&Path, &str)>,
) -> Result<u64, String> {
    check_destination(prefix)?;
    // Left by an interrupted installation, or by one that could not finish
    // removing the installation it replaced.
    clear_leftovers(prefix);
    let staging = free_folder(prefix, ".installing")?;
    fs::create_dir_all(&staging)
        .map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
    // The marker comes first, so a leftover staging folder is recognizable.
    fs::write(
        staging.join(MARKER),
        format!("# Written by the Sayon installer; the uninstaller removes this folder.\nversion = \"{version}\"\n"),
    )
    .map_err(|e| format!("cannot write the installation marker: {e}"))?;

    let mut total = 0u64;
    for file in files {
        let target = staging.join(&file.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        fs::write(&target, &file.data)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
        crate::platform::make_executable_if_needed(&target, &file.path);
        total += file.data.len() as u64;
    }
    if let Some((source, name)) = uninstaller {
        let target = staging.join(name);
        total +=
            fs::copy(source, &target).map_err(|e| format!("cannot copy the uninstaller: {e}"))?;
    }
    swap(&staging, prefix)?;
    Ok(total)
}

/// Moves the staged files into place, keeping the old installation until
/// the new one is there.
fn swap(staging: &Path, prefix: &Path) -> Result<(), String> {
    let busy = |e: std::io::Error| {
        format!(
            "cannot replace {}: {e}\nclose programs that are using Sayon (a running `sayon` command, or an editor) and try again",
            prefix.display()
        )
    };
    if !prefix.exists() {
        if let Some(parent) = prefix.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        return fs::rename(staging, prefix).map_err(busy);
    }
    let old = free_folder(prefix, ".previous")?;
    fs::rename(prefix, &old).map_err(busy)?;
    if let Err(error) = fs::rename(staging, prefix) {
        // Put the previous installation back.
        let _ = fs::rename(&old, prefix);
        return Err(busy(error));
    }
    // What is still open stays, with its marker, for the next installation.
    discard(&old);
    Ok(())
}

/// Removes a Sayon installation, except `keep` (the running uninstaller,
/// which some systems cannot delete while it runs). The marker goes last,
/// so an interrupted removal can be retried. Returns whether the whole
/// directory is gone.
pub fn remove(prefix: &Path, keep: Option<&Path>) -> Result<bool, String> {
    let marker = prefix.join(MARKER);
    if !marker.is_file() {
        return Err(format!(
            "{} is not a Sayon installation (it has no {MARKER} file)",
            prefix.display()
        ));
    }
    let mut kept: Vec<PathBuf> = fs::canonicalize(&marker).into_iter().collect();
    kept.extend(keep.and_then(|path| fs::canonicalize(path).ok()));
    remove_tree(prefix, &kept).map_err(|e| {
        format!("{e}\nclose programs that are using Sayon (a running `sayon` command, or an editor) and try again")
    })?;
    fs::remove_file(&marker).map_err(|e| format!("cannot remove {}: {e}", marker.display()))?;
    let _ = fs::remove_dir(prefix);
    Ok(!prefix.exists())
}

/// Removes everything inside `dir` except the `kept` files, and `dir`
/// itself once it is empty.
fn remove_tree(dir: &Path, kept: &[PathBuf]) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut keeps_something = false;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if kind.is_dir() {
            remove_tree(&path, kept)?;
            keeps_something |= path.exists();
        } else if fs::canonicalize(&path).is_ok_and(|p| kept.contains(&p)) {
            keeps_something = true;
        } else {
            fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
        }
    }
    if !keeps_something {
        fs::remove_dir(dir).map_err(|e| format!("cannot remove {}: {e}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sayon-installer-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn file(path: &str, text: &str) -> Entry {
        Entry {
            path: path.to_string(),
            data: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn the_top_folder_is_removed() {
        let files = layout(vec![
            file("sayon-1/bin/a", "x"),
            file("sayon-1/README.md", "y"),
        ]);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["bin/a", "README.md"]);
        let mixed = layout(vec![file("a/b", "x"), file("c", "y")]);
        assert_eq!(mixed[1].path, "c");
    }

    #[test]
    fn installs_replace_and_remove_only_sayon() {
        let root = scratch("cycle");
        let prefix = root.join("Sayon");
        let first = vec![file("bin/tool", "one"), file("docs/old.md", "gone later")];
        unpack(&first, &prefix, "1.0.0", None).unwrap();
        assert_eq!(installed_version(&prefix).as_deref(), Some("1.0.0"));
        assert_eq!(fs::read_to_string(prefix.join("bin/tool")).unwrap(), "one");

        // An upgrade replaces every file.
        let second = vec![file("bin/tool", "two")];
        unpack(&second, &prefix, "2.0.0", None).unwrap();
        assert_eq!(installed_version(&prefix).as_deref(), Some("2.0.0"));
        assert!(!prefix.join("docs/old.md").exists());
        assert!(!root.join("Sayon.previous").exists());
        assert!(!root.join("Sayon.installing").exists());

        // Folders holding other files are left alone.
        let other = root.join("Documents");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("notes.txt"), "mine").unwrap();
        assert!(unpack(&second, &other, "2.0.0", None).is_err());
        assert!(remove(&other, None).is_err());
        assert_eq!(fs::read_to_string(other.join("notes.txt")).unwrap(), "mine");

        assert!(remove(&prefix, None).unwrap());
        assert!(!prefix.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn leftovers_never_stop_an_installation() {
        let root = scratch("leftovers");
        let prefix = root.join("Sayon");
        unpack(&[file("bin/tool", "one")], &prefix, "1.0.0", None).unwrap();
        // What an older installer left when it could not finish removing an
        // installation: its files without the marker, so not the installer's
        // to remove.
        let stranger = root.join("Sayon.previous");
        fs::create_dir_all(stranger.join("bin")).unwrap();
        fs::write(stranger.join("bin/tool"), "old").unwrap();
        // A staging folder an interrupted installation left, with its marker.
        let interrupted = root.join("Sayon.installing");
        fs::create_dir_all(&interrupted).unwrap();
        fs::write(interrupted.join(MARKER), "version = \"0.9.0\"\n").unwrap();
        fs::write(interrupted.join("half"), "x").unwrap();

        unpack(&[file("bin/tool", "two")], &prefix, "2.0.0", None).unwrap();
        assert_eq!(fs::read_to_string(prefix.join("bin/tool")).unwrap(), "two");
        assert!(!interrupted.exists());
        assert!(!root.join("Sayon.previous-2").exists());
        assert_eq!(
            fs::read_to_string(stranger.join("bin/tool")).unwrap(),
            "old"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// A file a program holds open, as Windows lets no one delete it.
    #[cfg(windows)]
    fn hold(path: &Path) -> fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn a_folder_still_open_keeps_its_marker_for_the_next_installation() {
        let root = scratch("open");
        let prefix = root.join("Sayon");
        unpack(&[file("bin/tool", "one")], &prefix, "1.0.0", None).unwrap();
        let stuck = root.join("Sayon.previous");
        unpack(
            &[file("bin/tool", "old"), file("docs/a.md", "a")],
            &stuck,
            "0.9.0",
            None,
        )
        .unwrap();
        let open = hold(&stuck.join("bin/tool"));
        assert!(!discard(&stuck));
        assert!(stuck.join(MARKER).is_file());

        // The installation goes on beside it, and the next one clears it.
        unpack(&[file("bin/tool", "two")], &prefix, "2.0.0", None).unwrap();
        assert_eq!(fs::read_to_string(prefix.join("bin/tool")).unwrap(), "two");
        assert!(stuck.join(MARKER).is_file());
        assert!(!root.join("Sayon.previous-2").exists());
        drop(open);
        unpack(&[file("bin/tool", "three")], &prefix, "3.0.0", None).unwrap();
        assert!(!stuck.exists());
        assert!(remove(&prefix, None).unwrap());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removal_can_keep_the_running_uninstaller() {
        let root = scratch("keep");
        let prefix = root.join("Sayon");
        unpack(
            &[file("bin/tool", "x"), file("uninstall", "me")],
            &prefix,
            "1.0.0",
            None,
        )
        .unwrap();
        let fully = remove(&prefix, Some(&prefix.join("uninstall"))).unwrap();
        assert!(!fully);
        assert!(prefix.join("uninstall").exists());
        assert!(!prefix.join("bin").exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
