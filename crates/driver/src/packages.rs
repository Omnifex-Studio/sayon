//! Dependencies: resolving a project's packages, the lock file `sayon.lock`,
//! package digests, and the cache of fetched sources.
//!
//! `sayon install` chooses a version of each registry package the graph
//! reaches, resolves the graph declared by the manifests, fetches Git and
//! registry sources into the cache, and writes the lock. Every other command
//! reads the lock, checks that it still matches the manifests and that
//! cached sources are intact, and neither fetches nor changes anything.
//!
//! A package's identity is its source: `path:<directory>`, relative to the
//! root package and written with `/`, `git:<url>#<commit>`, or
//! `registry:<address>#<name>@<version>`. Packages from local directories
//! are used as they are; Git and registry sources are locked to a digest of
//! their contents.
//!
//! A graph has one version of each registry package: the latest that every
//! range asking for it accepts, or, while it still does, the version the
//! previous lock chose, so that installing again changes nothing.

use crate::manifest::{self, Dependency, Manifest, Source, Value};
use crate::registry::{Index, Published};
use crate::semver::{Requirement, Version};
use crate::sha256::{Sha256, hex};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub const LOCK_FILE: &str = "sayon.lock";
const SCHEMA: i64 = 1;
const DIGEST_DOMAIN: &[u8] = b"sayon-package-digest-1\0";

/// A problem with the dependencies, and what to do about it.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageError {
    pub message: String,
    pub help: Option<String>,
}

impl PackageError {
    pub fn new(message: impl Into<String>) -> PackageError {
        PackageError {
            message: message.into(),
            help: None,
        }
    }

    pub fn help(mut self, help: impl Into<String>) -> PackageError {
        self.help = Some(help.into());
        self
    }
}

fn install_hint(error: PackageError) -> PackageError {
    error.help("run `sayon install` to bring sayon.lock up to date")
}

/// One package of the graph, as the lock records it.
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    pub source: String,
    pub name: String,
    pub version: String,
    /// The digest of a Git source's contents; empty for a local directory.
    pub digest: String,
    /// `(alias, source)` for each dependency, sorted by alias.
    pub dependencies: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Lock {
    /// The root package's dependencies, `(alias, source)` sorted by alias.
    pub dependencies: Vec<(String, String)>,
    pub dev_dependencies: Vec<(String, String)>,
    /// Every package of the graph, sorted by source.
    pub packages: Vec<Package>,
}

/// Which package directories an import of `dependencies/<alias>/...` from
/// each package reaches. Keys are canonical package directories.
#[derive(Debug, Clone, Default)]
pub struct Packages {
    pub aliases: HashMap<PathBuf, HashMap<String, PathBuf>>,
}

impl Packages {
    /// The directory of the package that `alias` names for the package at
    /// `root`.
    pub fn find(&self, root: &Path, alias: &str) -> Option<&PathBuf> {
        self.aliases.get(&canonical(root))?.get(alias)
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The relative path from `from` to `to`, with `/`, or `to` itself when
/// they share no root (such as different drives).
fn relative(from: &Path, to: &Path) -> String {
    let from: Vec<Component> = from.components().collect();
    let to_parts: Vec<Component> = to.components().collect();
    if from.first() != to_parts.first() {
        return to.to_string_lossy().replace('\\', "/");
    }
    let common = from
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    parts.extend(
        to_parts[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

// ----- the cache of fetched sources ----------------------------------------------

/// Where fetched sources are kept: `SAYON_CACHE`, or the user's cache folder.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SAYON_CACHE") {
        return PathBuf::from(dir);
    }
    if cfg!(windows)
        && let Some(base) = std::env::var_os("LOCALAPPDATA")
    {
        return PathBuf::from(base).join("Sayon").join("cache");
    }
    if let Some(base) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(base).join("sayon");
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".cache").join("sayon"),
        None => std::env::temp_dir().join("sayon-cache"),
    }
}

/// The cache folder of one commit of a repository. The name is short, so
/// that paths inside the repository stay within Windows' path limits.
fn git_dir(url: &str, revision: &str) -> PathBuf {
    let mut hasher = Sha256::default();
    hasher.update(url.as_bytes());
    hasher.update(b"\0");
    hasher.update(revision.as_bytes());
    cache_dir().join("git").join(&hex(&hasher.finish())[..20])
}

fn git(args: &[&str], dir: Option<&Path>) -> Result<String, PackageError> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    let output = command.args(args).output().map_err(|_| {
        PackageError::new("Git is needed to fetch Git dependencies, and it was not found")
            .help("install Git, make sure `git` runs in a terminal, and try again")
    })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(PackageError::new(format!(
            "`git {}` failed: {detail}",
            args.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Clones a repository at one commit into the cache, through a temporary
/// directory so that an interrupted fetch leaves nothing behind.
fn fetch(url: &str, revision: &str, dir: &Path) -> Result<(), PackageError> {
    let parent = dir.parent().expect("cache directory");
    std::fs::create_dir_all(parent).map_err(|e| {
        PackageError::new(format!(
            "cannot create the cache folder {}: {e}",
            parent.display()
        ))
    })?;
    let temporary = parent.join(format!(
        "{}.{}",
        dir.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = remove_tree(&temporary);
    let result = (|| {
        let target = temporary.to_string_lossy().into_owned();
        git(&["clone", "--quiet", "--no-checkout", url, &target], None)?;
        git(&["checkout", "--quiet", "--detach", revision], Some(&temporary)).map_err(|_| {
            PackageError::new(format!("the commit {revision} is not in {url}"))
                .help("check the `revision` in sayon.toml; it must be a full commit id from that repository")
        })?;
        let head = git(&["rev-parse", "HEAD"], Some(&temporary))?;
        if head != revision {
            return Err(PackageError::new(format!(
                "{url} checked out {head} instead of {revision}"
            )));
        }
        std::fs::rename(&temporary, dir)
            .map_err(|e| PackageError::new(format!("cannot store {url} in the cache: {e}")))
    })();
    if result.is_err() {
        let _ = remove_tree(&temporary);
    }
    result
}

/// The cache folder of one version of a registry package.
fn registry_dir(address: &str, name: &str, version: &Version) -> PathBuf {
    let mut hasher = Sha256::default();
    hasher.update(address.as_bytes());
    cache_dir()
        .join("registry")
        .join(&hex(&hasher.finish())[..16])
        .join(format!("{name}-{version}"))
}

/// A registry package's identity.
fn registry_source(address: &str, name: &str, version: &Version) -> String {
    format!("registry:{address}#{name}@{version}")
}

/// The registry, name, and version a registry package's identity names.
fn registry_parts(source: &str) -> Option<(&str, &str, Version)> {
    let (address, package) = source.strip_prefix("registry:")?.rsplit_once('#')?;
    let (name, version) = package.split_once('@')?;
    Some((address, name, Version::parse(version)?))
}

/// Writes an archive's files into the cache, through a temporary directory
/// so that an interrupted fetch leaves nothing behind.
fn store(archive: &[u8], dir: &Path) -> Result<(), PackageError> {
    let parent = dir.parent().expect("cache directory");
    std::fs::create_dir_all(parent).map_err(|e| {
        PackageError::new(format!(
            "cannot create the cache folder {}: {e}",
            parent.display()
        ))
    })?;
    let temporary = parent.join(format!(
        "{}.{}",
        dir.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = remove_tree(&temporary);
    let result = crate::archive::unpack(archive, &temporary)
        .map_err(PackageError::new)
        .and_then(|()| match std::fs::rename(&temporary, dir) {
            // Another `sayon install` stored it first.
            Err(_) if dir.is_dir() => remove_tree(&temporary),
            other => other.map_err(|e| {
                PackageError::new(format!("cannot store {} in the cache: {e}", dir.display()))
            }),
        });
    if result.is_err() {
        let _ = remove_tree(&temporary);
    }
    result
}

/// Removes a directory and everything in it, including files Git marks
/// read-only.
fn remove_tree(dir: &Path) -> Result<(), PackageError> {
    fn writable(path: &Path) {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && !entry.file_type().is_ok_and(|t| t.is_symlink()) {
                    writable(&path);
                } else if let Ok(metadata) = std::fs::metadata(&path) {
                    let mut permissions = metadata.permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(false);
                    let _ = std::fs::set_permissions(&path, permissions);
                }
            }
        }
    }
    if !dir.exists() {
        return Ok(());
    }
    writable(dir);
    std::fs::remove_dir_all(dir)
        .map_err(|e| PackageError::new(format!("cannot remove {}: {e}", dir.display())))
}

// ----- digests ---------------------------------------------------------------------

/// The digest of a package's contents: every file under its directory except
/// hidden entries (such as `.git`), `target`, and `sayon.lock`, in order of
/// their paths. Each file contributes its path and its bytes, each preceded
/// by its length, so that no two different packages hash the same input.
pub fn digest(dir: &Path) -> Result<String, PackageError> {
    let mut files = Vec::new();
    collect_files(dir, "", &mut files)?;
    files.sort();
    let mut hasher = Sha256::default();
    hasher.update(DIGEST_DOMAIN);
    for (relative, path) in files {
        let bytes = std::fs::read(&path)
            .map_err(|e| PackageError::new(format!("cannot read {}: {e}", path.display())))?;
        hasher.update(&(relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("sha256:{}", hex(&hasher.finish())))
}

fn collect_files(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), PackageError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| PackageError::new(format!("cannot read {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| PackageError::new(e.to_string()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || (prefix.is_empty() && (name == "target" || name == LOCK_FILE)) {
            continue;
        }
        let relative = format!("{prefix}{name}");
        let kind = entry
            .file_type()
            .map_err(|e| PackageError::new(e.to_string()))?;
        if kind.is_symlink() {
            return Err(PackageError::new(format!(
                "the package at {} contains a symbolic link, `{relative}`",
                dir.display()
            ))
            .help("packages cannot contain symbolic links; copy the file instead"));
        }
        if kind.is_dir() {
            collect_files(&entry.path(), &format!("{relative}/"), out)?;
        } else {
            out.push((relative, entry.path()));
        }
    }
    Ok(())
}

// ----- resolving ---------------------------------------------------------------------

/// Whether resolving may fetch Git and registry sources that are not in the
/// cache, and ask registries for their packages' versions.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    Allowed,
    Offline,
}

/// A resolved graph: the lock, and the directory of every package.
pub struct Resolved {
    pub lock: Lock,
    pub dirs: HashMap<String, PathBuf>,
}

/// A registry package: its registry's address and its name.
type Key = (String, String);

/// What asked for a version of a registry package: the range, and who.
type Wanted = BTreeMap<Key, Vec<(String, String)>>;

struct Resolver<'i> {
    root: PathBuf,
    fetch: Fetch,
    packages: BTreeMap<String, Package>,
    dirs: HashMap<String, PathBuf>,
    /// Sources being resolved, to find cycles.
    chain: Vec<String>,
    /// The digests the previous lock recorded, by source.
    known: HashMap<String, String>,
    /// The registries, while fetching is allowed.
    index: Option<&'i mut dyn Index>,
    /// The version of each registry package the graph has.
    choices: HashMap<Key, Version>,
    /// What registries said of their packages' versions.
    published: HashMap<Key, Vec<Published>>,
    /// The registry of dependencies that name none, but those of registry
    /// packages, which come from their package's registry.
    default: String,
}

fn read_package_manifest(dir: &Path, alias: &str) -> Result<Manifest, PackageError> {
    let path = dir.join(manifest::FILE_NAME);
    let text = std::fs::read_to_string(&path).map_err(|_| {
        PackageError::new(format!(
            "the dependency `{alias}` has no {} at {}",
            manifest::FILE_NAME,
            dir.display()
        ))
        .help("a dependency is a Sayon package: a folder with a sayon.toml")
    })?;
    manifest::parse(sayon_syntax::FileId(0), &text).map_err(|errors| {
        let first = errors
            .first()
            .map(|e| e.message.clone())
            .unwrap_or_default();
        PackageError::new(format!(
            "the manifest of the dependency `{alias}` ({}) has errors: {first}",
            path.display()
        ))
        .help(format!(
            "run `sayon check` in {} to see them all",
            dir.display()
        ))
    })
}

impl Resolver<'_> {
    fn new(root: PathBuf, fetch: Fetch, index: Option<&mut dyn Index>) -> Resolver<'_> {
        Resolver {
            root,
            fetch,
            packages: BTreeMap::new(),
            dirs: HashMap::new(),
            chain: Vec::new(),
            known: HashMap::new(),
            index,
            choices: HashMap::new(),
            published: HashMap::new(),
            default: crate::registry::default_address(),
        }
    }

    /// Takes the registry packages' versions and the digests a lock has.
    fn remember(&mut self, lock: &Lock) {
        for package in &lock.packages {
            if !package.digest.is_empty() {
                self.known
                    .insert(package.source.clone(), package.digest.clone());
            }
            if let Some((address, name, version)) = registry_parts(&package.source) {
                self.choices
                    .insert((address.to_string(), name.to_string()), version);
            }
        }
    }

    /// The registry that the dependencies of the package `source` come from
    /// when they name none.
    fn home(&self, source: &str) -> String {
        registry_parts(source)
            .map_or_else(|| self.default.clone(), |(address, ..)| address.to_string())
    }

    /// The versions a registry has of a package.
    fn versions(&mut self, key: &Key) -> Result<Vec<Published>, PackageError> {
        if let Some(found) = self.published.get(key) {
            return Ok(found.clone());
        }
        let Some(index) = self.index.as_deref_mut() else {
            return Err(PackageError::new(format!(
                "choosing a version of `{}` needs its registry, {}, and installing is offline",
                key.1, key.0
            ))
            .help("run `sayon install` without `--offline`"));
        };
        let found = index.versions(&key.0, &key.1)?;
        self.published.insert(key.clone(), found.clone());
        Ok(found)
    }

    /// What the registry says of one version of a package.
    fn version(&mut self, key: &Key, version: &Version) -> Result<Published, PackageError> {
        self.versions(key)?
            .into_iter()
            .find(|published| published.version == *version)
            .ok_or_else(|| {
                PackageError::new(format!("{} has no version {version} of `{}`", key.0, key.1))
                    .help("run `sayon install` to choose a version it has")
            })
    }

    /// The dependencies of one version of a registry package: from its
    /// manifest when it is in the cache, or else from its registry.
    fn registry_dependencies(
        &mut self,
        key: &Key,
        version: &Version,
    ) -> Result<Vec<Dependency>, PackageError> {
        let dir = registry_dir(&key.0, &key.1, version);
        if dir.join(manifest::FILE_NAME).is_file() {
            return Ok(read_package_manifest(&dir, &key.1)?.dependencies);
        }
        Ok(self.version(key, version)?.dependencies)
    }

    /// Chooses the version of each registry package the graph reaches. Each
    /// round walks the graph with the versions chosen so far and chooses
    /// again for the ranges it found, until nothing changes.
    fn select(&mut self, manifest: &Manifest) -> Result<(), PackageError> {
        for _ in 0..100 {
            let mut wanted = Wanted::new();
            let mut seen = HashSet::new();
            let (root, home) = (self.root.clone(), self.default.clone());
            for dependencies in [&manifest.dependencies, &manifest.dev_dependencies] {
                self.gather(
                    &root,
                    &home,
                    dependencies,
                    "this project",
                    &mut wanted,
                    &mut seen,
                )?;
            }
            let mut next = HashMap::new();
            for (key, ranges) in &wanted {
                let parsed = ranges
                    .iter()
                    .map(|(range, _)| Requirement::parse(range))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(PackageError::new)?;
                let accepted =
                    |version: &Version| parsed.iter().all(|range| range.matches(version));
                if let Some(current) = self.choices.get(key)
                    && accepted(current)
                {
                    next.insert(key.clone(), current.clone());
                    continue;
                }
                let versions = self.versions(key)?;
                let best = versions
                    .iter()
                    .map(|published| &published.version)
                    .filter(|version| accepted(version))
                    .max();
                match best {
                    Some(version) => next.insert(key.clone(), version.clone()),
                    None => return Err(conflict(key, ranges, &versions)),
                };
            }
            let settled = next == self.choices;
            self.choices = next;
            if settled {
                return Ok(());
            }
        }
        Err(PackageError::new(
            "the ranges the packages ask for keep changing which versions match them",
        )
        .help("name narrower ranges in sayon.toml, such as \"=1.2.3\", for the packages involved"))
    }

    /// Notes the ranges asked for by the dependencies of the package at
    /// `dir`, and walks on into the packages they reach.
    fn gather(
        &mut self,
        dir: &Path,
        home: &str,
        dependencies: &[Dependency],
        by: &str,
        wanted: &mut Wanted,
        seen: &mut HashSet<String>,
    ) -> Result<(), PackageError> {
        for dependency in dependencies {
            if let Source::Registry {
                registry,
                package,
                requirement,
            } = &dependency.source
            {
                let address = registry.clone().unwrap_or_else(|| home.to_string());
                let key = (address, package.clone());
                wanted
                    .entry(key.clone())
                    .or_default()
                    .push((requirement.clone(), by.to_string()));
                let Some(version) = self.choices.get(&key).cloned() else {
                    continue;
                };
                if !seen.insert(registry_source(&key.0, package, &version)) {
                    continue;
                }
                let inner = self.registry_dependencies(&key, &version)?;
                let by = format!("{package} {version}");
                self.gather(dir, &key.0, &inner, &by, wanted, seen)?;
            } else {
                let (source, package_dir) = self.locate(dir, home, dependency)?;
                if !seen.insert(source) {
                    continue;
                }
                let found = read_package_manifest(&package_dir, &dependency.alias)?;
                let default = self.default.clone();
                self.gather(
                    &package_dir,
                    &default,
                    &found.dependencies,
                    &found.name,
                    wanted,
                    seen,
                )?;
            }
        }
        Ok(())
    }

    /// The identity and directory of a dependency of the package at `from`,
    /// whose dependencies come from the registry `home` when they name none.
    fn locate(
        &mut self,
        from: &Path,
        home: &str,
        dependency: &Dependency,
    ) -> Result<(String, PathBuf), PackageError> {
        match &dependency.source {
            Source::Path(path) => {
                let dir = canonical(&from.join(path));
                if !dir.join(manifest::FILE_NAME).is_file() {
                    return Err(PackageError::new(format!(
                        "the dependency `{}` points to {}, which has no {}",
                        dependency.alias,
                        from.join(path).display(),
                        manifest::FILE_NAME
                    ))
                    .help(
                        "`path` is the dependency's folder, relative to the package that names it",
                    ));
                }
                Ok((format!("path:{}", relative(&self.root, &dir)), dir))
            }
            Source::Git { url, revision } => {
                let dir = git_dir(url, revision);
                let source = format!("git:{url}#{revision}");
                // A cached copy that no longer matches the lock is fetched again.
                if dir.is_dir()
                    && let Some(expected) = self.known.get(&source)
                    && digest(&dir)? != *expected
                {
                    if self.fetch == Fetch::Offline {
                        return Err(PackageError::new(format!(
                            "the cached copy of `{}` does not match sayon.lock",
                            dependency.alias
                        ))
                        .help("run `sayon install` without `--offline` to fetch it again"));
                    }
                    remove_tree(&dir)?;
                }
                if !dir.is_dir() {
                    if self.fetch == Fetch::Offline {
                        return Err(PackageError::new(format!(
                            "the dependency `{}` ({url} at {revision}) is not in the cache",
                            dependency.alias
                        ))
                        .help("run `sayon install` without `--offline` to fetch it"));
                    }
                    fetch(url, revision, &dir)?;
                }
                Ok((source, dir))
            }
            Source::Registry {
                registry,
                package,
                requirement,
            } => {
                let alias = &dependency.alias;
                let address = registry.clone().unwrap_or_else(|| home.to_string());
                let key = (address.clone(), package.clone());
                let Some(version) = self.choices.get(&key).cloned() else {
                    return Err(PackageError::new(format!(
                        "the dependency `{alias}` ({package} from {address}) is not in sayon.lock"
                    ))
                    .help("run `sayon install` to choose its version"));
                };
                if !Requirement::parse(requirement).is_ok_and(|range| range.matches(&version)) {
                    return Err(PackageError::new(format!(
                        "sayon.lock has {package} {version}, which `{alias} = \"{requirement}\"` does not accept"
                    ))
                    .help("run `sayon install` to choose another version"));
                }
                let source = registry_source(&address, package, &version);
                let dir = registry_dir(&address, package, &version);
                let mut expected = self.known.get(&source).cloned();
                if expected.is_none() && self.index.is_some() {
                    expected = Some(self.version(&key, &version)?.digest);
                }
                if dir.is_dir()
                    && let Some(expected) = &expected
                    && digest(&dir)? != *expected
                {
                    if self.fetch == Fetch::Offline {
                        return Err(PackageError::new(format!(
                            "the cached copy of `{alias}` ({package} {version}) does not match sayon.lock"
                        ))
                        .help("run `sayon install` without `--offline` to fetch it again"));
                    }
                    remove_tree(&dir)?;
                }
                if !dir.is_dir() {
                    let (Some(index), Some(expected)) = (self.index.as_deref_mut(), expected)
                    else {
                        return Err(PackageError::new(format!(
                            "the dependency `{alias}` ({package} {version} from {address}) is not in the cache"
                        ))
                        .help("run `sayon install` without `--offline` to fetch it"));
                    };
                    let archive = index.archive(&address, package, &version)?;
                    let found = crate::archive::digest(&archive);
                    if found != expected {
                        return Err(PackageError::new(format!(
                            "the archive {address} gave of {package} {version} has the digest {found}, not {expected}"
                        ))
                        .help("a published version never changes, so the copy was damaged on the way, or the registry is not the one sayon.lock was written with; try again, and if it persists, tell the registry's operator"));
                    }
                    store(&archive, &dir)?;
                }
                Ok((source, dir))
            }
        }
    }

    /// Resolves the dependencies of the package at `dir`, returning their
    /// `(alias, source)` pairs.
    fn dependencies(
        &mut self,
        dir: &Path,
        home: &str,
        dependencies: &[Dependency],
    ) -> Result<Vec<(String, String)>, PackageError> {
        let mut edges = Vec::new();
        for dependency in dependencies {
            let (source, package_dir) = self.locate(dir, home, dependency)?;
            edges.push((dependency.alias.clone(), source.clone()));
            if self.chain.contains(&source) {
                let mut cycle = self.chain.clone();
                cycle.push(source);
                return Err(PackageError::new(format!(
                    "packages depend on each other in a cycle: {}",
                    cycle.join(" -> ")
                )));
            }
            if self.packages.contains_key(&source) {
                continue;
            }
            let found = read_package_manifest(&package_dir, &dependency.alias)?;
            if let Some((address, name, version)) = registry_parts(&source)
                && (found.name != name || found.version != version.to_string())
            {
                return Err(PackageError::new(format!(
                    "{address} gave {} {} as {name} {version}",
                    found.name, found.version
                )));
            }
            let inner_home = self.home(&source);
            self.chain.push(source.clone());
            let inner = self.dependencies(&package_dir, &inner_home, &found.dependencies)?;
            self.chain.pop();
            let digest = if source.starts_with("git:") || source.starts_with("registry:") {
                digest(&package_dir)?
            } else {
                String::new()
            };
            self.dirs.insert(source.clone(), package_dir);
            self.packages.insert(
                source.clone(),
                Package {
                    source,
                    name: found.name,
                    version: found.version,
                    digest,
                    dependencies: inner,
                },
            );
        }
        edges.sort();
        Ok(edges)
    }
}

/// The error for a registry package no version of which every range
/// asking for it accepts.
fn conflict(key: &Key, ranges: &[(String, String)], versions: &[Published]) -> PackageError {
    let (address, name) = key;
    // The project's own range first, then the packages' by name.
    let mut ranges = ranges.to_vec();
    ranges.sort_by_key(|(range, by)| (by != "this project", by.clone(), range.clone()));
    ranges.dedup();
    let asked: Vec<String> = ranges
        .iter()
        .map(|(range, by)| format!("\"{range}\" ({by})"))
        .collect();
    let message = match asked.as_slice() {
        [one] => format!("no version of `{name}` in {address} matches {one}"),
        _ => format!(
            "no version of `{name}` in {address} matches every range asked for: {}",
            asked.join(", ")
        ),
    };
    let mut shown: Vec<String> = versions
        .iter()
        .rev()
        .take(5)
        .map(|published| published.version.to_string())
        .collect();
    if versions.len() > shown.len() {
        shown.push("...".to_string());
    }
    let help = if shown.is_empty() {
        format!("`{name}` has no published versions yet")
    } else {
        format!(
            "it has {}; change the ranges so that one version matches them all",
            shown.join(", ")
        )
    };
    PackageError::new(message).help(help)
}

/// Resolves the whole dependency graph of the package at `root`.
/// `previous` is the lock being replaced: cached sources must still match
/// its digests, and its versions of registry packages are kept while the
/// ranges accept them. `index` reaches the registries; offline, none is.
pub fn resolve(
    root: &Path,
    manifest: &Manifest,
    fetch: Fetch,
    previous: Option<&Lock>,
    index: &mut dyn Index,
) -> Result<Resolved, PackageError> {
    let root = canonical(root);
    let index = (fetch == Fetch::Allowed).then_some(index);
    let mut resolver = Resolver::new(root.clone(), fetch, index);
    if let Some(lock) = previous {
        resolver.remember(lock);
    }
    resolver.select(manifest)?;
    resolver.chain.push("path:.".to_string());
    let home = resolver.default.clone();
    let dependencies = resolver.dependencies(&root, &home, &manifest.dependencies)?;
    let dev_dependencies = resolver.dependencies(&root, &home, &manifest.dev_dependencies)?;
    Ok(Resolved {
        lock: Lock {
            dependencies,
            dev_dependencies,
            packages: resolver.packages.into_values().collect(),
        },
        dirs: resolver.dirs,
    })
}

// ----- the lock file -----------------------------------------------------------------

fn edges_text(edges: &[(String, String)]) -> String {
    let items: Vec<String> = edges
        .iter()
        .map(|(alias, source)| format!("\"{alias} = {}\"", escape(source)))
        .collect();
    format!("[{}]", items.join(", "))
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The lock as text. The same graph always gives the same text.
pub fn render(lock: &Lock) -> String {
    let mut out = String::new();
    out.push_str("# sayon.lock: the exact packages this project depends on.\n");
    out.push_str("# Written by `sayon install`; do not edit.\n");
    out.push_str(&format!("schema = {SCHEMA}\n"));
    out.push_str(&format!(
        "dependencies = {}\n",
        edges_text(&lock.dependencies)
    ));
    out.push_str(&format!(
        "dev_dependencies = {}\n",
        edges_text(&lock.dev_dependencies)
    ));
    for package in &lock.packages {
        out.push_str("\n[[package]]\n");
        out.push_str(&format!("source = \"{}\"\n", escape(&package.source)));
        out.push_str(&format!("name = \"{}\"\n", package.name));
        out.push_str(&format!("version = \"{}\"\n", package.version));
        if !package.digest.is_empty() {
            out.push_str(&format!("digest = \"{}\"\n", package.digest));
        }
        out.push_str(&format!(
            "dependencies = {}\n",
            edges_text(&package.dependencies)
        ));
    }
    out
}

fn parse_edges(items: &[String]) -> Result<Vec<(String, String)>, String> {
    items
        .iter()
        .map(|item| {
            item.split_once(" = ")
                .map(|(alias, source)| (alias.to_string(), source.to_string()))
                .ok_or_else(|| format!("`{item}` is not `alias = source`"))
        })
        .collect()
}

/// Reads a lock file written by `render`.
pub fn parse_lock(text: &str) -> Result<Lock, String> {
    let mut lock = Lock::default();
    let mut current: Option<Package> = None;
    let mut schema = None;
    for (number, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fail = |message: String| format!("line {}: {message}", number + 1);
        if line == "[[package]]" {
            if let Some(package) = current.take() {
                lock.packages.push(package);
            }
            current = Some(Package {
                source: String::new(),
                name: String::new(),
                version: String::new(),
                digest: String::new(),
                dependencies: Vec::new(),
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(fail("expected `key = value`".to_string()));
        };
        let value = manifest::parse_value(value.trim()).map_err(fail)?;
        let key = key.trim();
        match (&mut current, key, value) {
            (None, "schema", Value::Integer(number)) => schema = Some(number),
            (None, "dependencies", Value::Strings(items)) => {
                lock.dependencies = parse_edges(&items).map_err(fail)?
            }
            (None, "dev_dependencies", Value::Strings(items)) => {
                lock.dev_dependencies = parse_edges(&items).map_err(fail)?
            }
            (Some(package), "source", Value::String(text)) => package.source = text,
            (Some(package), "name", Value::String(text)) => package.name = text,
            (Some(package), "version", Value::String(text)) => package.version = text,
            (Some(package), "digest", Value::String(text)) => package.digest = text,
            (Some(package), "dependencies", Value::Strings(items)) => {
                package.dependencies = parse_edges(&items).map_err(fail)?
            }
            (_, key, _) => return Err(fail(format!("unexpected `{key}`"))),
        }
    }
    if let Some(package) = current {
        lock.packages.push(package);
    }
    match schema {
        Some(SCHEMA) => Ok(lock),
        Some(other) => Err(format!(
            "sayon.lock has schema {other}, but this version of Sayon reads schema {SCHEMA}"
        )),
        None => Err("sayon.lock has no schema line".to_string()),
    }
}

/// Writes the lock through a temporary file, so that it is never left half
/// written.
fn write_lock(root: &Path, text: &str) -> Result<(), PackageError> {
    let path = root.join(LOCK_FILE);
    let temporary = root.join(format!("{LOCK_FILE}.tmp"));
    std::fs::write(&temporary, text)
        .and_then(|_| std::fs::rename(&temporary, &path))
        .map_err(|e| PackageError::new(format!("cannot write {}: {e}", path.display())))
}

// ----- commands ------------------------------------------------------------------------

/// What `sayon install` did.
pub struct Installed {
    pub packages: usize,
    pub changed: bool,
}

/// `sayon install`: resolves and fetches the graph, and writes the lock.
/// With `locked`, the lock must already describe the graph exactly.
pub fn install(
    root: &Path,
    manifest: &Manifest,
    locked: bool,
    fetch: Fetch,
    index: &mut dyn Index,
) -> Result<Installed, PackageError> {
    let existing = std::fs::read_to_string(root.join(LOCK_FILE)).ok();
    if locked && existing.is_none() {
        return Err(PackageError::new("this project has no sayon.lock")
            .help("run `sayon install` without `--locked` to write it"));
    }
    let previous = existing.as_deref().and_then(|text| parse_lock(text).ok());
    let resolved = resolve(root, manifest, fetch, previous.as_ref(), index)?;
    let text = render(&resolved.lock);
    let changed = existing.as_deref() != Some(text.as_str());
    if changed && locked {
        return Err(PackageError::new(match existing {
            Some(_) => "sayon.lock does not match the dependencies in sayon.toml",
            None => "this project has no sayon.lock",
        })
        .help("run `sayon install` without `--locked` to write it"));
    }
    if changed {
        write_lock(root, &text)?;
    }
    Ok(Installed {
        packages: resolved.lock.packages.len(),
        changed,
    })
}

/// The packages a build of the project at `root` uses, from its lock. The
/// lock must match the manifests, and cached Git sources their digests.
/// `tests` includes the root package's development dependencies.
pub fn load(root: &Path, manifest: &Manifest, tests: bool) -> Result<Packages, PackageError> {
    if manifest.dependencies.is_empty() && manifest.dev_dependencies.is_empty() {
        return Ok(Packages::default());
    }
    let root = canonical(root);
    let text = std::fs::read_to_string(root.join(LOCK_FILE)).map_err(|_| {
        PackageError::new("this project's dependencies are not installed")
            .help("run `sayon install` to fetch them and write sayon.lock")
    })?;
    let lock = parse_lock(&text).map_err(|message| {
        install_hint(PackageError::new(format!(
            "sayon.lock is invalid: {message}"
        )))
    })?;

    // The sources each manifest names must be the ones locked. Checking
    // never fetches, so a Git or registry source must already be in the
    // cache; its digest is checked below.
    let mut checker = Resolver::new(root.clone(), Fetch::Offline, None);
    for package in &lock.packages {
        if let Some((address, name, version)) = registry_parts(&package.source) {
            checker
                .choices
                .insert((address.to_string(), name.to_string()), version);
        }
    }
    let declared =
        |checker: &mut Resolver, from: &Path, home: &str, dependencies: &[Dependency]| {
            let mut edges = Vec::new();
            for dependency in dependencies {
                let (source, dir) = checker
                    .locate(from, home, dependency)
                    .map_err(install_hint)?;
                edges.push((dependency.alias.clone(), source, dir));
            }
            edges.sort();
            Ok::<_, PackageError>(edges)
        };
    let changed = || {
        install_hint(PackageError::new(
            "the dependencies in sayon.toml changed since sayon.lock was written",
        ))
    };
    let home = checker.default.clone();
    let roots = declared(&mut checker, &root, &home, &manifest.dependencies)?;
    let dev_roots = declared(&mut checker, &root, &home, &manifest.dev_dependencies)?;
    let strip = |edges: &[(String, String, PathBuf)]| -> Vec<(String, String)> {
        edges
            .iter()
            .map(|(a, s, _)| (a.clone(), s.clone()))
            .collect()
    };
    if strip(&roots) != lock.dependencies || strip(&dev_roots) != lock.dev_dependencies {
        return Err(changed());
    }

    let mut packages = Packages::default();
    let mut root_aliases = HashMap::new();
    for (alias, _, dir) in roots
        .iter()
        .chain(if tests { &dev_roots[..] } else { &[][..] })
    {
        root_aliases.insert(alias.clone(), dir.clone());
    }
    packages.aliases.insert(root.clone(), root_aliases);

    let locked: HashMap<&str, &Package> = lock
        .packages
        .iter()
        .map(|p| (p.source.as_str(), p))
        .collect();
    let mut pending: Vec<(String, PathBuf)> = roots
        .iter()
        .chain(&dev_roots)
        .map(|(_, source, dir)| (source.clone(), dir.clone()))
        .collect();
    let mut seen = std::collections::HashSet::new();
    while let Some((source, dir)) = pending.pop() {
        if !seen.insert(source.clone()) {
            continue;
        }
        let Some(package) = locked.get(source.as_str()) else {
            return Err(changed());
        };
        if !package.digest.is_empty() && digest(&dir)? != package.digest {
            return Err(PackageError::new(format!(
                "the cached copy of `{}` ({source}) does not match sayon.lock",
                package.name
            ))
            .help(format!(
                "delete {} and run `sayon install` to fetch it again",
                dir.display()
            )));
        }
        let found = read_package_manifest(&dir, &package.name)?;
        let home = checker.home(&source);
        let edges = declared(&mut checker, &dir, &home, &found.dependencies)?;
        if strip(&edges) != package.dependencies {
            return Err(install_hint(PackageError::new(format!(
                "the dependencies of `{}` changed since sayon.lock was written",
                package.name
            ))));
        }
        let mut aliases = HashMap::new();
        for (alias, inner_source, inner_dir) in edges {
            aliases.insert(alias, inner_dir.clone());
            pending.push((inner_source, inner_dir));
        }
        packages.aliases.insert(canonical(&dir), aliases);
    }
    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        let base = Path::new("/work/app");
        assert_eq!(relative(base, Path::new("/work/utilities")), "../utilities");
        assert_eq!(relative(base, Path::new("/work/app/vendor/x")), "vendor/x");
        assert_eq!(relative(base, Path::new("/work/app")), ".");
    }

    #[test]
    fn locks_round_trip() {
        let lock = Lock {
            dependencies: vec![("utilities".into(), "path:../utilities".into())],
            dev_dependencies: Vec::new(),
            packages: vec![Package {
                source: "git:https://example.com/a.git#0123456789abcdef0123456789abcdef01234567"
                    .into(),
                name: "codec".into(),
                version: "1.0.0".into(),
                digest: "sha256:00".into(),
                dependencies: vec![("inner".into(), "path:../inner".into())],
            }],
        };
        let text = render(&lock);
        assert_eq!(parse_lock(&text).as_ref(), Ok(&lock));
        assert!(parse_lock("dependencies = []\n").is_err());
    }
}
