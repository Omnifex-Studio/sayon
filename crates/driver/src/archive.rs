//! Package archives, as the registry stores them: the package's canonical
//! digest input itself. An archive is the domain `sayon-package-digest-1\0`,
//! then each file, in the order of its path, as the length of its path, the
//! path (`/` between its parts), the length of its contents, and the
//! contents, each length 8 bytes, least significant first. Its SHA-256 is
//! the package's digest (`packages::digest`), so an archive, the files it
//! holds, and a cached copy of them are checked against the same value.
//!
//! A package holds every file of its folder but hidden ones (any part
//! starting with `.`), and `build`, `target`, and `sayon.lock` at its top.
//! Unpacking refuses whatever would not have been packed: paths out of
//! order or twice, paths that differ only in case, and paths that leave the
//! folder.

use crate::sha256::{Sha256, hex};
use std::path::{Component, Path, PathBuf};

pub const DOMAIN: &[u8] = b"sayon-package-digest-1\0";

/// The most an archive may hold.
pub const LARGEST: usize = 10 * 1024 * 1024;
const MOST_FILES: usize = 10_000;

/// Whether a top-level name is left out of packages.
fn left_out(name: &str) -> bool {
    matches!(name, "build" | "target" | crate::packages::LOCK_FILE)
}

/// The files a package holds, by their paths, in order.
pub fn files(dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
        let entries =
            std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || (prefix.is_empty() && left_out(&name)) {
                continue;
            }
            let relative = format!("{prefix}{name}");
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() {
                return Err(format!(
                    "`{relative}` is a symbolic link; a package holds files only, so copy what it points to"
                ));
            }
            if kind.is_dir() {
                walk(&entry.path(), &format!("{relative}/"), out)?;
            } else {
                out.push((relative, entry.path()));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out)?;
    out.sort();
    Ok(out)
}

/// The archive of the package at `dir`, and the paths of its files.
pub fn pack(dir: &Path) -> Result<(Vec<u8>, Vec<String>), String> {
    let mut archive = DOMAIN.to_vec();
    let mut paths = Vec::new();
    for (relative, path) in files(dir)? {
        let bytes =
            std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        archive.extend((relative.len() as u64).to_le_bytes());
        archive.extend(relative.as_bytes());
        archive.extend((bytes.len() as u64).to_le_bytes());
        archive.extend(&bytes);
        if archive.len() > LARGEST {
            return Err(format!(
                "the package holds more than {} MiB; a package is its source, so leave out what it builds or generates",
                LARGEST / (1024 * 1024)
            ));
        }
        paths.push(relative);
    }
    if !paths.iter().any(|p| p == crate::manifest::FILE_NAME) {
        return Err("the package has no sayon.toml".to_string());
    }
    Ok((archive, paths))
}

/// The digest of an archive, as `sha256:...`.
pub fn digest(archive: &[u8]) -> String {
    let mut hasher = Sha256::default();
    hasher.update(archive);
    format!("sha256:{}", hex(&hasher.finish()))
}

/// Whether a path inside a package is one a package can hold.
fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && !path.contains('\0')
        && !path.starts_with('/')
        && !path.contains(':')
        && path
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.'))
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !left_out(path.split('/').next().unwrap_or(path))
}

/// The files an archive holds, checked: each path once, in order, none
/// differing from another only in case, and none leaving the package.
pub fn entries(archive: &[u8]) -> Result<Vec<(String, &[u8])>, String> {
    let damaged = || "the archive is damaged or not a Sayon package".to_string();
    let rest = archive.strip_prefix(DOMAIN).ok_or_else(damaged)?;
    if archive.len() > LARGEST {
        return Err("the archive is larger than a package can be".to_string());
    }
    let mut at = 0;
    let mut out: Vec<(String, &[u8])> = Vec::new();
    let mut folded = std::collections::HashSet::new();
    let take = |at: &mut usize, count: usize| -> Option<&[u8]> {
        let slice = rest.get(*at..at.checked_add(count)?)?;
        *at += count;
        Some(slice)
    };
    while at < rest.len() {
        if out.len() >= MOST_FILES {
            return Err("the archive holds too many files".to_string());
        }
        let length = |bytes: Option<&[u8]>| -> Option<usize> {
            usize::try_from(u64::from_le_bytes(bytes?.try_into().ok()?)).ok()
        };
        let path_length = length(take(&mut at, 8)).ok_or_else(damaged)?;
        let path = std::str::from_utf8(take(&mut at, path_length).ok_or_else(damaged)?)
            .map_err(|_| damaged())?
            .to_string();
        let content_length = length(take(&mut at, 8)).ok_or_else(damaged)?;
        let content = take(&mut at, content_length).ok_or_else(damaged)?;
        if !valid_path(&path) {
            return Err(format!(
                "the archive holds `{path}`, which a package cannot"
            ));
        }
        if out.last().is_some_and(|(previous, _)| *previous >= path) {
            return Err(format!("the archive holds `{path}` out of order or twice"));
        }
        if !folded.insert(path.to_lowercase()) {
            return Err(format!(
                "the archive holds `{path}` and another path that differs only in case"
            ));
        }
        out.push((path, content));
    }
    if !out.iter().any(|(p, _)| p == crate::manifest::FILE_NAME) {
        return Err("the archive has no sayon.toml".to_string());
    }
    Ok(out)
}

/// Writes an archive's files into `dir`, which must not exist yet.
pub fn unpack(archive: &[u8], dir: &Path) -> Result<(), String> {
    let entries = entries(archive)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    for (path, content) in entries {
        let target = dir.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, content)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, path: &str, text: &str) {
        let target = dir.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, text).unwrap();
    }

    fn archive_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = DOMAIN.to_vec();
        for (path, content) in files {
            out.extend((path.len() as u64).to_le_bytes());
            out.extend(path.as_bytes());
            out.extend((content.len() as u64).to_le_bytes());
            out.extend(*content);
        }
        out
    }

    #[test]
    fn a_package_packs_and_unpacks_to_its_digest() {
        let work = std::env::temp_dir().join(format!("sayon-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let package = work.join("package");
        write(
            &package,
            "sayon.toml",
            "[package]\nname = \"a\"\nversion = \"1.0.0\"\n",
        );
        write(&package, "src/main.sayon", "print(1)\r\n");
        write(&package, "build/debug/a", "built");
        write(&package, ".git/HEAD", "ref");
        write(&package, "sayon.lock", "schema = 1\n");
        let (archive, paths) = pack(&package).unwrap();
        assert_eq!(paths, vec!["sayon.toml", "src/main.sayon"]);
        let copy = work.join("copy");
        unpack(&archive, &copy).unwrap();
        assert_eq!(
            std::fs::read(copy.join("src/main.sayon")).unwrap(),
            b"print(1)\r\n"
        );
        assert_eq!(
            crate::packages::digest(&copy)
                .map_err(|e| e.message)
                .unwrap(),
            digest(&archive)
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn unsafe_archives_are_refused() {
        let manifest: (&str, &[u8]) = ("sayon.toml", b"");
        for bad in [
            archive_of(&[("../outside", b"x"), manifest]),
            archive_of(&[(".hidden", b"x"), manifest]),
            archive_of(&[("Src/a.sayon", b"x"), manifest, ("src/a.sayon", b"y")]),
            archive_of(&[manifest, manifest]),
            archive_of(&[("src/z", b"x"), ("src/a", b"y"), manifest]),
            archive_of(&[("src/a", b"x")]),
            archive_of(&[("C:/windows", b"x"), manifest]),
            b"not an archive".to_vec(),
        ] {
            assert!(entries(&bad).is_err());
        }
        let mut cut = archive_of(&[manifest, ("src/a", b"content")]);
        cut.truncate(cut.len() - 3);
        assert!(entries(&cut).is_err());
    }
}
