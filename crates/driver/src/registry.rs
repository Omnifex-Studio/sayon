//! The package registry's protocol (its repository, packages.sayon-lang.org,
//! describes it), apart from how its requests travel: registries'
//! addresses, what their answers hold, and what `sayon install` asks of one
//! ([`Index`]). The `sayon` command carries the requests over HTTPS.

use crate::json::Json;
use crate::manifest::{Dependency, Source};
use crate::packages::PackageError;
use crate::semver::Version;
use sayon_syntax::{FileId, Span};

/// The protocol's version, which every address of it starts with:
/// `<registry>/api/v1/...`.
pub const PROTOCOL: u32 = 1;

/// The registry used when nothing names another: until Sayon's registry is
/// operated, one run on this computer from packages.sayon-lang.org's
/// `local.mjs --port 4100`.
pub const DEFAULT: &str = "http://127.0.0.1:4100";

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

/// One published version of a package, as the registry tells it.
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    pub version: Version,
    /// The digest of its archive, `sha256:...`.
    pub digest: String,
    pub size: u64,
    /// Its dependencies, which are all on registry packages.
    pub dependencies: Vec<Dependency>,
    pub published: String,
    pub publisher: String,
}

/// What the registry knows of a package.
#[derive(Debug, Clone, PartialEq)]
pub struct Information {
    pub name: String,
    pub owner: String,
    pub description: String,
    pub latest: String,
    /// Oldest first.
    pub versions: Vec<Published>,
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

/// Reads the answer to `GET /api/v1/packages/<name>`.
pub fn information(answer: &Json) -> Result<Information, String> {
    let mut versions = Vec::new();
    for item in answer.get("versions").as_array() {
        let shown = text(item, "version")?;
        let version = Version::parse(&shown)
            .ok_or_else(|| format!("the registry lists \"{shown}\", which is not a version"))?;
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
            digest: text(item, "digest")?,
            size: item.get("size").as_u32().unwrap_or(0) as u64,
            dependencies,
            published: text(item, "published").unwrap_or_default(),
            publisher: text(item, "publisher").unwrap_or_default(),
        });
    }
    versions.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(Information {
        name: text(answer, "name")?,
        owner: text(answer, "owner")?,
        description: text(answer, "description").unwrap_or_default(),
        latest: text(answer, "latest")?,
        versions,
    })
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
    fn answers_are_read() {
        let answer = Json::parse(
            r#"{"name":"json","owner":"ada","description":"JSON","latest":"1.1.0","versions":[
                {"version":"1.1.0","digest":"sha256:bb","size":10,"published":"2026-10-27","publisher":"ada",
                 "dependencies":[{"alias":"text","package":"strings","requirement":"^0.2"}]},
                {"version":"1.0.0","digest":"sha256:aa","size":9,"published":"2026-10-26","publisher":"ada","dependencies":[]}]}"#,
        )
        .unwrap();
        let read = information(&answer).unwrap();
        assert_eq!(read.versions[0].version.to_string(), "1.0.0");
        assert_eq!(
            read.versions[1].dependencies[0].source,
            Source::Registry {
                registry: None,
                package: "strings".into(),
                requirement: "^0.2".into()
            }
        );
        assert!(information(&Json::parse("{}").unwrap()).is_err());
    }
}
