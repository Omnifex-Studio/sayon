//! The package registry's protocol (PROTOCOL.md of packages.sayon-lang.org),
//! apart from how its requests travel. A registry is named by the address
//! of its index, which holds a file for each package, a line of JSON for
//! each version, and `config.json`, which says where the registry's API
//! and its archives are. This module reads what they hold, and says what
//! `sayon install` asks of a registry ([`Index`]); the `sayon` command
//! carries the requests over HTTPS.

use crate::json::Json;
use crate::manifest::{Dependency, Source};
use crate::packages::PackageError;
use crate::semver::Version;
use sayon_syntax::{FileId, Span};

/// The protocol's version, which every address of it starts with:
/// `<registry>/api/v1/...`.
pub const PROTOCOL: u32 = 1;

/// The registry used when nothing names another, by its index's address:
/// until Sayon's registry is operated, the one packages.sayon-lang.org
/// runs on this computer with `sayon run -- --port 4100`.
pub const DEFAULT: &str = "http://127.0.0.1:4100/index";

/// Whether an address can be a registry's: HTTPS, or plain HTTP to this
/// computer, for a registry run locally.
pub fn valid_address(address: &str) -> bool {
    let local = ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|prefix| {
            address
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/']))
        });
    let secure = address
        .strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'));
    (local || secure) && !address.contains(['?', '#', ' ', '"'])
}

/// An address as packages and locks write it: without a final `/`.
pub fn normalize(address: &str) -> String {
    address.trim().trim_end_matches('/').to_string()
}

/// The registry that dependencies naming none come from, and that the
/// registry commands use: `SAYON_REGISTRY`, or [`DEFAULT`].
pub fn default_address() -> String {
    match std::env::var("SAYON_REGISTRY") {
        Ok(address) if !address.trim().is_empty() => normalize(&address),
        _ => DEFAULT.to_string(),
    }
}

/// One published version of a package, as its line in the index says.
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    pub version: Version,
    /// The digest of its archive, `sha256:...`.
    pub digest: String,
    pub description: String,
    /// Its dependencies, which are all on registry packages.
    pub dependencies: Vec<Dependency>,
    pub published: String,
}

/// A registry's `config.json`: the address of its API, and the address of
/// each archive, with `{name}` and `{version}` in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub api: String,
    pub download: String,
}

impl Config {
    /// The address of a version's archive.
    pub fn archive(&self, package: &str, version: &Version) -> String {
        self.download
            .replace("{name}", package)
            .replace("{version}", &version.to_string())
    }
}

/// A package in the answer to a search.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub name: String,
    pub description: String,
    pub latest: String,
}

fn text(value: &Json, key: &str) -> Result<String, String> {
    value
        .get(key)
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("the registry's answer has no `{key}`"))
}

/// Where a package's file is in an index, as crates.io-index places them:
/// `1/a`, `2/ab`, `3/a/abc`, and `ab/cd/abcdef` for longer names.
pub fn index_path(name: &str) -> String {
    let characters: Vec<char> = name.chars().collect();
    let first = |count: usize| characters[..count].iter().collect::<String>();
    match characters.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", first(1)),
        _ => format!(
            "{}/{}/{name}",
            first(2),
            characters[2..4].iter().collect::<String>()
        ),
    }
}

/// Reads an index's `config.json`.
pub fn config(answer: &Json) -> Result<Config, String> {
    let config = Config {
        api: normalize(&text(answer, "api")?),
        download: text(answer, "download")?,
    };
    if !valid_address(&config.api) || !valid_address(&config.download) {
        return Err("the registry's config.json names addresses Sayon does not use".to_string());
    }
    Ok(config)
}

/// Reads a package's file of the index: a line of JSON for each version.
/// The versions come in order, oldest first.
pub fn entries(file: &str) -> Result<Vec<Published>, String> {
    let mut versions = Vec::new();
    for (number, line) in file.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let item =
            Json::parse(line).map_err(|e| format!("line {} is not JSON: {e}", number + 1))?;
        let shown = text(&item, "version")?;
        let version = Version::parse(&shown)
            .ok_or_else(|| format!("the index lists \"{shown}\", which is not a version"))?;
        let mut dependencies = Vec::new();
        for dependency in item.get("dependencies").as_array() {
            dependencies.push(Dependency {
                alias: text(dependency, "alias")?,
                source: Source::Registry {
                    registry: dependency.get("registry").as_str().map(normalize),
                    package: text(dependency, "package")?,
                    requirement: text(dependency, "requirement")?,
                },
                span: Span::new(FileId(0), 0, 0),
            });
        }
        versions.push(Published {
            version,
            digest: text(&item, "digest")?,
            description: text(&item, "description").unwrap_or_default(),
            dependencies,
            published: text(&item, "published").unwrap_or_default(),
        });
    }
    versions.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(versions)
}

/// The latest version: the greatest release, or, before the first release,
/// the greatest pre-release.
pub fn latest(versions: &[Published]) -> Option<&Published> {
    versions
        .iter()
        .filter(|published| published.version.pre.is_empty())
        .max_by(|a, b| a.version.cmp(&b.version))
        .or_else(|| versions.iter().max_by(|a, b| a.version.cmp(&b.version)))
}

/// Reads the answer to `GET /api/v1/search`.
pub fn search(answer: &Json) -> Result<Vec<Found>, String> {
    answer
        .get("packages")
        .as_array()
        .iter()
        .map(|item| {
            Ok(Found {
                name: text(item, "name")?,
                description: text(item, "description").unwrap_or_default(),
                latest: text(item, "latest")?,
            })
        })
        .collect()
}

/// What `sayon install` asks of registries: the versions of a package,
/// and the archive of one of them.
pub trait Index {
    fn versions(&mut self, registry: &str, package: &str) -> Result<Vec<Published>, PackageError>;
    fn archive(
        &mut self,
        registry: &str,
        package: &str,
        version: &Version,
    ) -> Result<Vec<u8>, PackageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_https_or_this_computer() {
        assert!(valid_address("https://registry.example.com"));
        assert!(valid_address("http://127.0.0.1:4100"));
        assert!(valid_address("http://localhost"));
        assert!(!valid_address("http://registry.example.com"));
        assert!(!valid_address("http://localhost.example.com"));
        assert!(!valid_address("https://"));
        assert!(!valid_address("ftp://registry.example.com"));
        assert_eq!(normalize("https://r.example/ "), "https://r.example");
    }

    #[test]
    fn packages_are_placed_as_crates_io_index_places_them() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("abc"), "3/a/abc");
        assert_eq!(index_path("strings"), "st/ri/strings");
    }

    #[test]
    fn index_files_are_read() {
        let file = concat!(
            r#"{"name":"json","version":"1.1.0-beta.1","digest":"sha256:cc","description":"JSON","dependencies":[],"published":"2026-10-28"}"#,
            "\n",
            r#"{"name":"json","version":"1.0.0","digest":"sha256:aa","description":"JSON","dependencies":[{"alias":"text","package":"strings","requirement":"^0.2","registry":null}],"published":"2026-10-26"}"#,
            "\n",
        );
        let versions = entries(file).unwrap();
        assert_eq!(versions[0].version.to_string(), "1.0.0");
        assert_eq!(
            versions[0].dependencies[0].source,
            Source::Registry {
                registry: None,
                package: "strings".into(),
                requirement: "^0.2".into()
            }
        );
        assert_eq!(latest(&versions).unwrap().version.to_string(), "1.0.0");
        assert!(entries("not json").is_err());
        let config = config(
            &Json::parse(r#"{"protocol":1,"api":"http://127.0.0.1:4100/","download":"http://127.0.0.1:4100/a/{name}/{version}"}"#).unwrap(),
        )
        .unwrap();
        assert_eq!(config.api, "http://127.0.0.1:4100");
        assert_eq!(
            config.archive("json", &Version::parse("1.0.0").unwrap()),
            "http://127.0.0.1:4100/a/json/1.0.0"
        );
    }
}
