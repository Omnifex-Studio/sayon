//! The registry commands, `sayon login`, `logout`, `publish`, `search`, and
//! `info`, and the registry client `sayon install` fetches packages with.
//! The protocol is described in the registry's repository,
//! packages.sayon-lang.org; the driver reads its answers
//! (`sayon_driver::registry`), and `http` carries its requests.

use crate::compile::{Options, current_project, print_package_error};
use crate::credentials::{self, Saved};
use crate::http::{self, Response};
use crate::{status, usage_error};
use sayon_driver::json::Json;
use sayon_driver::manifest::Source;
use sayon_driver::packages::PackageError;
use sayon_driver::registry::{self, Index, Published};
use sayon_driver::semver::Version;
use std::io::{BufRead, IsTerminal, Write};

/// The registry a command uses: `--registry`, or the default.
pub fn address(chosen: Option<&String>) -> Result<String, u8> {
    let address = match chosen {
        Some(address) => registry::normalize(address),
        None => registry::default_address(),
    };
    if !registry::valid_address(&address) {
        return Err(usage_error(&format!(
            "\"{address}\" is not a registry's address; it starts with `https://`, or `http://localhost` for one on this computer"
        )));
    }
    Ok(address)
}

fn error(message: &str, help: Option<&str>, options: &Options) {
    let error = PackageError {
        message: message.to_string(),
        help: help.map(str::to_string),
    };
    print_package_error(&error, options);
}

/// Percent-encodes a part of an address.
fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The registry's reason for refusing a request.
fn refusal(response: &Response) -> String {
    std::str::from_utf8(&response.body)
        .ok()
        .and_then(|text| Json::parse(text).ok())
        .and_then(|answer| answer.get("error").as_str().map(str::to_string))
        .unwrap_or_else(|| format!("the registry answered with status {}", response.status))
}

fn reach(registry: &str, error: String) -> String {
    let local = if registry == registry::DEFAULT {
        "; it is the registry run on this computer, which `node local.mjs --port 4100` in packages.sayon-lang.org starts, unless SAYON_REGISTRY names another"
    } else {
        ""
    };
    format!("cannot reach the registry {registry}: {error}{local}")
}

/// Asks the registry, reading a JSON answer.
fn ask(
    registry: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &[u8],
) -> Result<(u16, Json), String> {
    let bearer = token.map(|token| format!("Bearer {token}"));
    let mut headers = vec![("Accept", "application/json")];
    if let Some(bearer) = &bearer {
        headers.push(("Authorization", bearer));
    }
    if !body.is_empty() {
        headers.push(("Content-Type", "application/octet-stream"));
    }
    let url = format!("{registry}/api/v{}{path}", registry::PROTOCOL);
    let response = http::request(method, &url, &headers, body).map_err(|e| reach(registry, e))?;
    if !(200..300).contains(&response.status) {
        return Err(refusal(&response));
    }
    let answer = std::str::from_utf8(&response.body)
        .ok()
        .and_then(|text| Json::parse(text).ok())
        .ok_or_else(|| format!("{registry} gave an answer that is not the registry protocol's"))?;
    Ok((response.status, answer))
}

/// Reads a line from the terminal without showing it, or from whatever
/// standard input is.
fn read_secret(prompt: &str) -> std::io::Result<String> {
    let terminal = std::io::stdin().is_terminal();
    if terminal {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
    }
    let hidden = terminal && console::hide_input();
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if hidden {
        console::show_input();
        eprintln!();
    }
    read.map(|_| line.trim().to_string())
}

/// `sayon login`: takes a token for the registry, checks it, and keeps it.
pub fn login(registry: &str, options: &Options) -> u8 {
    let token = match read_secret(&format!("token for {registry}: ")) {
        Ok(token) if !token.is_empty() => token,
        Ok(_) => {
            error(
                "no token was given",
                Some("paste the token the registry's operator gave you, then press Enter"),
                options,
            );
            return status::USAGE;
        }
        Err(e) => {
            error(&format!("cannot read the token: {e}"), None, options);
            return status::INPUT;
        }
    };
    if token.chars().any(char::is_whitespace) {
        error("a token is one word, with no spaces", None, options);
        return status::USAGE;
    }
    let account = match ask(registry, "GET", "/me", Some(&token), &[]) {
        Ok((_, answer)) => answer.get("name").as_str().unwrap_or_default().to_string(),
        Err(message) => {
            error(&message, None, options);
            return status::PROGRAM_FAILED;
        }
    };
    if let Err(message) = credentials::save(
        registry,
        &Saved {
            account: account.clone(),
            token,
        },
    ) {
        error(&message, None, options);
        return status::PROGRAM_FAILED;
    }
    if !options.quiet {
        eprintln!(
            "logged in to {registry} as {account}; the token is kept in {}",
            credentials::place()
        );
    }
    status::SUCCESS
}

/// `sayon logout`: forgets the registry's token.
pub fn logout(registry: &str, options: &Options) -> u8 {
    match credentials::forget(registry) {
        Ok(true) => {
            if !options.quiet {
                eprintln!("logged out of {registry}");
            }
            status::SUCCESS
        }
        Ok(false) => {
            if !options.quiet {
                eprintln!("not logged in to {registry}");
            }
            status::SUCCESS
        }
        Err(message) => {
            error(&message, None, options);
            status::PROGRAM_FAILED
        }
    }
}

fn size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
    }
}

/// `sayon publish`: sends the project's package to the registry, or with
/// `dry_run` shows what it would send.
pub fn publish(registry: &str, dry_run: bool, options: &Options) -> u8 {
    let project = match current_project(options) {
        Ok(Some(project)) => project,
        Ok(None) => {
            return usage_error(
                "`sayon publish` publishes a project; run it in a directory with sayon.toml",
            );
        }
        Err(code) => return code,
    };
    let manifest = &project.manifest;
    for dependency in &manifest.dependencies {
        if !matches!(dependency.source, Source::Registry { .. }) {
            error(
                &format!(
                    "the dependency `{}` is not from a registry, so the package's users could not get it",
                    dependency.alias
                ),
                Some(
                    "publish it, and depend on it by version, as `name = \"1.0\"`; development dependencies may be local",
                ),
                options,
            );
            return status::COMPILE_FAILED;
        }
    }
    let (archive, files) = match sayon_driver::archive::pack(&project.root) {
        Ok(packed) => packed,
        Err(message) => {
            error(&message, None, options);
            return status::COMPILE_FAILED;
        }
    };
    let digest = sayon_driver::archive::digest(&archive);
    let what = format!("{} {}", manifest.name, manifest.version);
    if dry_run {
        println!("{what}: {} files, {}", files.len(), size(archive.len()));
        for file in &files {
            println!("    {file}");
        }
        println!("digest {digest}");
        if !options.quiet {
            eprintln!("nothing was published, as `--dry-run` asked");
        }
        return status::SUCCESS;
    }
    let token = match std::env::var("SAYON_TOKEN") {
        Ok(token) if !token.trim().is_empty() => token.trim().to_string(),
        _ => match credentials::load(registry) {
            Ok(Some(saved)) => saved.token,
            Ok(None) => {
                error(
                    &format!("publishing needs an account at {registry}"),
                    Some("run `sayon login` with the token of your account first"),
                    options,
                );
                return status::USAGE;
            }
            Err(message) => {
                error(&message, None, options);
                return status::PROGRAM_FAILED;
            }
        },
    };
    let path = format!(
        "/packages/{}/{}",
        encode(&manifest.name),
        encode(&manifest.version)
    );
    match ask(registry, "PUT", &path, Some(&token), &archive) {
        Ok(_) => {
            if !options.quiet {
                eprintln!(
                    "published {what} to {registry} ({} files, {})",
                    files.len(),
                    size(archive.len())
                );
            }
            status::SUCCESS
        }
        Err(message) => {
            error(
                &format!("{what} was not published: {message}"),
                None,
                options,
            );
            status::PROGRAM_FAILED
        }
    }
}

/// `sayon search`: the packages whose names or descriptions hold `query`.
pub fn search(registry: &str, query: &str, limit: u32, options: &Options) -> u8 {
    let path = format!("/search?q={}&limit={limit}", encode(query));
    let found = match ask(registry, "GET", &path, None, &[])
        .and_then(|(_, answer)| registry::search(&answer))
    {
        Ok(found) => found,
        Err(message) => {
            error(&message, None, options);
            return status::PROGRAM_FAILED;
        }
    };
    if found.is_empty() {
        if !options.quiet {
            eprintln!("no packages in {registry} match \"{query}\"");
        }
        return status::SUCCESS;
    }
    let width = found
        .iter()
        .map(|f| f.name.len() + f.latest.len() + 3)
        .max()
        .unwrap_or(0);
    for package in &found {
        let left = format!("{} = \"{}\"", package.name, package.latest);
        if package.description.is_empty() {
            println!("{left}");
        } else {
            println!("{left:width$}  # {}", package.description);
        }
    }
    status::SUCCESS
}

/// `sayon info`: what the registry knows of a package.
pub fn info(registry: &str, name: &str, options: &Options) -> u8 {
    let information = match ask(
        registry,
        "GET",
        &format!("/packages/{}", encode(name)),
        None,
        &[],
    )
    .and_then(|(_, answer)| registry::information(&answer))
    {
        Ok(information) => information,
        Err(message) => {
            error(
                &message,
                Some("`sayon search` finds packages by part of their names"),
                options,
            );
            return status::PROGRAM_FAILED;
        }
    };
    println!("{} {}", information.name, information.latest);
    if !information.description.is_empty() {
        println!("{}", information.description);
    }
    println!();
    println!("owner: {}", information.owner);
    println!("registry: {registry}");
    let latest = information
        .versions
        .iter()
        .find(|published| published.version.to_string() == information.latest);
    if let Some(latest) = latest {
        if latest.dependencies.is_empty() {
            println!("dependencies: none");
        } else {
            println!("dependencies:");
            for dependency in &latest.dependencies {
                if let Source::Registry {
                    package,
                    requirement,
                    registry: other,
                } = &dependency.source
                {
                    let from = other
                        .as_deref()
                        .map(|r| format!(" from {r}"))
                        .unwrap_or_default();
                    let renamed = if *package == dependency.alias {
                        String::new()
                    } else {
                        format!(" ({package})")
                    };
                    println!("    {}{renamed} \"{requirement}\"{from}", dependency.alias);
                }
            }
        }
    }
    println!("versions:");
    for published in information.versions.iter().rev() {
        let day = published
            .published
            .get(..10)
            .unwrap_or(&published.published);
        println!(
            "    {:<12} {day}  {}  {}",
            published.version.to_string(),
            size(published.size as usize),
            published.digest
        );
    }
    println!();
    println!(
        "in sayon.toml: {} = \"{}\"",
        information.name, information.latest
    );
    status::SUCCESS
}

/// The registries `sayon install` fetches packages from.
pub struct Client;

impl Index for Client {
    fn versions(&mut self, registry: &str, package: &str) -> Result<Vec<Published>, PackageError> {
        let url = format!(
            "{registry}/api/v{}/packages/{}",
            registry::PROTOCOL,
            encode(package)
        );
        let response = http::request("GET", &url, &[("Accept", "application/json")], &[])
            .map_err(|e| PackageError::new(reach(registry, e)))?;
        if response.status == 404 {
            return Err(PackageError::new(format!(
                "there is no package `{package}` in {registry}"
            ))
            .help("check its name in sayon.toml; `sayon search` finds packages"));
        }
        if response.status != 200 {
            return Err(PackageError::new(format!(
                "{registry} did not tell the versions of `{package}`: {}",
                refusal(&response)
            )));
        }
        std::str::from_utf8(&response.body)
            .ok()
            .and_then(|text| Json::parse(text).ok())
            .ok_or_else(|| "the answer is not JSON".to_string())
            .and_then(|answer| registry::information(&answer))
            .map(|information| information.versions)
            .map_err(|message| {
                PackageError::new(format!(
                    "{registry} gave an answer about `{package}` that Sayon cannot read: {message}"
                ))
            })
    }

    fn archive(
        &mut self,
        registry: &str,
        package: &str,
        version: &Version,
    ) -> Result<Vec<u8>, PackageError> {
        let url = format!(
            "{registry}/api/v{}/packages/{}/{}/archive",
            registry::PROTOCOL,
            encode(package),
            encode(&version.to_string())
        );
        let response = http::request("GET", &url, &[], &[])
            .map_err(|e| PackageError::new(reach(registry, e)))?;
        if response.status != 200 {
            return Err(PackageError::new(format!(
                "{registry} did not give {package} {version}: {}",
                refusal(&response)
            )));
        }
        Ok(response.body)
    }
}

/// Turning the terminal's echo off while a token is typed.
#[cfg(windows)]
mod console {
    use std::ffi::c_void;

    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn GetConsoleMode(console: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(console: *mut c_void, mode: u32) -> i32;
    }

    fn set_echo(on: bool) -> bool {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0;
            if GetConsoleMode(input, &mut mode) == 0 {
                return false;
            }
            let mode = if on {
                mode | ENABLE_ECHO_INPUT
            } else {
                mode & !ENABLE_ECHO_INPUT
            };
            SetConsoleMode(input, mode) != 0
        }
    }

    pub fn hide_input() -> bool {
        set_echo(false)
    }

    pub fn show_input() {
        set_echo(true);
    }
}

#[cfg(not(windows))]
mod console {
    use std::process::{Command, Stdio};

    fn stty(setting: &str) -> bool {
        Command::new("stty")
            .arg(setting)
            .stdin(Stdio::inherit())
            .status()
            .is_ok_and(|status| status.success())
    }

    pub fn hide_input() -> bool {
        stty("-echo")
    }

    pub fn show_input() {
        stty("echo");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_of_addresses_are_encoded() {
        assert_eq!(encode("json parser"), "json%20parser");
        assert_eq!(encode("a&b=c/é"), "a%26b%3Dc%2F%C3%A9");
        assert_eq!(encode("1.0.0-beta.1"), "1.0.0-beta.1");
    }
}
