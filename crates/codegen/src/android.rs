//! Android apps. A program for Android is a shared library, `libmain.so`,
//! which the package holds for its processor's ABI, and which Android's own
//! `NativeActivity` loads, so the package has no Java code at all; the
//! runtime starts the program when the activity is made (see the runtime's
//! `android`). Building one takes the official SDK's tools: the NDK's clang
//! compiles and links the library against Android's C library, `aapt2`
//! makes the package's manifest from the one written here, the library goes
//! in stored and aligned to 16 KiB pages, and `apksigner` signs the package
//! with the debug key Android's tools share.

use crate::zip;
use sayon_target::Target;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The oldest Android a package runs on: 8.0.
pub const MIN_API: u32 = 26;

/// The Android a package is made for, whose platform it is built against.
pub const TARGET_API: u32 = 36;

/// The versions of the SDK's packages the tests are checked with, used
/// when present; otherwise the newest installed.
pub const NDK_VERSION: &str = "30.0.16248370";
pub const BUILD_TOOLS_VERSION: &str = "37.0.0";

/// The SDK's packages a build uses.
pub struct Sdk {
    pub root: PathBuf,
    pub ndk: PathBuf,
}

/// The tools that make and sign a package, besides the NDK.
pub struct Packaging {
    pub aapt2: PathBuf,
    pub apksigner: PathBuf,
    pub platform: PathBuf,
    pub java: PathBuf,
    pub keytool: PathBuf,
}

/// Where Android's tools keep the SDK when nothing says otherwise.
fn default_root() -> Option<PathBuf> {
    let home = |name: &str| std::env::var_os(name).map(PathBuf::from);
    if cfg!(windows) {
        home("LOCALAPPDATA").map(|dir| dir.join("Android").join("Sdk"))
    } else if cfg!(target_os = "macos") {
        home("HOME").map(|dir| dir.join("Library").join("Android").join("sdk"))
    } else {
        home("HOME").map(|dir| dir.join("Android").join("Sdk"))
    }
}

/// The SDK: `ANDROID_HOME`, `ANDROID_SDK_ROOT`, or where Android Studio puts
/// it.
pub fn sdk_root() -> Option<PathBuf> {
    ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .find_map(|name| std::env::var_os(name).map(PathBuf::from))
        .or_else(default_root)
        .filter(|root| root.is_dir())
}

/// A directory's subdirectory of `wanted`, or else of the highest version.
fn version_in(directory: &Path, prefix: &str, wanted: &str) -> Option<PathBuf> {
    let exact = directory.join(format!("{prefix}{wanted}"));
    if exact.is_dir() {
        return Some(exact);
    }
    let version = |name: &str| -> Option<Vec<u64>> {
        name.strip_prefix(prefix)?
            .split('.')
            .map(|part| part.parse().ok())
            .collect()
    };
    std::fs::read_dir(directory)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            Some((version(&name)?, entry.path()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, path)| path)
}

/// The suffix of the host's programs, and of its batch-free tools.
fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn setup_help(target: &Target) -> String {
    format!("`sayon target setup {target}` shows what building for it needs")
}

impl Sdk {
    /// Finds the SDK and its NDK: `ANDROID_NDK_HOME` or `ANDROID_NDK_ROOT`,
    /// or the NDK the SDK holds.
    pub fn locate(target: &Target) -> Result<Sdk, String> {
        let root = sdk_root().ok_or_else(|| {
            format!(
                "the Android SDK was not found; set ANDROID_HOME to it. {}",
                setup_help(target)
            )
        })?;
        let ndk = ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT"]
            .iter()
            .find_map(|name| std::env::var_os(name).map(PathBuf::from))
            .or_else(|| version_in(&root.join("ndk"), "", NDK_VERSION))
            .filter(|ndk| ndk.is_dir())
            .ok_or_else(|| {
                format!(
                    "the Android NDK was not found in {}; install it with the SDK manager or set \
                     ANDROID_NDK_HOME. {}",
                    root.join("ndk").display(),
                    setup_help(target)
                )
            })?;
        Ok(Sdk { root, ndk })
    }

    /// The NDK's clang, which knows Android's C library.
    pub fn clang(&self) -> Result<PathBuf, String> {
        let host = if cfg!(windows) {
            "windows-x86_64"
        } else if cfg!(target_os = "macos") {
            "darwin-x86_64"
        } else {
            "linux-x86_64"
        };
        let clang = self
            .ndk
            .join("toolchains")
            .join("llvm")
            .join("prebuilt")
            .join(host)
            .join("bin")
            .join(exe("clang"));
        if clang.is_file() {
            Ok(clang)
        } else {
            Err(format!(
                "the NDK's clang was not found at {}",
                clang.display()
            ))
        }
    }

    /// The tools that make and sign packages: the SDK's build tools and
    /// platform, and Java, which `apksigner` runs on (`JAVA_HOME`, or on
    /// `PATH`).
    pub fn packaging(&self, target: &Target) -> Result<Packaging, String> {
        let missing = |what: &str, place: &Path| {
            format!(
                "the Android SDK's {what} were not found in {}; install them with the SDK \
                 manager. {}",
                place.display(),
                setup_help(target)
            )
        };
        let tools_dir = self.root.join("build-tools");
        let tools = version_in(&tools_dir, "", BUILD_TOOLS_VERSION)
            .ok_or_else(|| missing("build tools", &tools_dir))?;
        let platforms = self.root.join("platforms");
        let platform = version_in(&platforms, "android-", &TARGET_API.to_string())
            .map(|dir| dir.join("android.jar"))
            .filter(|jar| jar.is_file())
            .ok_or_else(|| missing("platforms", &platforms))?;
        let java_bin = std::env::var_os("JAVA_HOME")
            .map(|home| PathBuf::from(home).join("bin"))
            .filter(|bin| bin.join(exe("java")).is_file())
            .or_else(|| {
                std::env::split_paths(&std::env::var_os("PATH")?)
                    .find(|dir| dir.join(exe("java")).is_file())
            })
            .ok_or_else(|| {
                format!(
                    "Java was not found; signing a package runs the SDK's apksigner on it. Set \
                     JAVA_HOME to a JDK, 17 or newer. {}",
                    setup_help(target)
                )
            })?;
        Ok(Packaging {
            aapt2: tools.join(exe("aapt2")),
            apksigner: tools.join("lib").join("apksigner.jar"),
            platform,
            java: java_bin.join(exe("java")),
            keytool: java_bin.join(exe("keytool")),
        })
    }
}

/// The clang target of an Android target, with the oldest Android it runs
/// on.
pub fn clang_target(target: &Target) -> String {
    format!("{}{MIN_API}", target.triple)
}

/// The directory of a package that holds the library for a target.
pub fn abi(target: &Target) -> &'static str {
    match target.processor {
        "arm64" => "arm64-v8a",
        _ => "x86_64",
    }
}

/// The package name of a program: `sayon.app.` and its name, made of what a
/// package name may hold.
pub fn package_name(program: &str) -> String {
    let mut name: String = program
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        name.insert(0, 'p');
    }
    format!("sayon.app.{name}")
}

const MANIFEST: &str = include_str!("android/AndroidManifest.xml");

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The manifest of a program's package: an app of Android's
/// `NativeActivity`, with the internet permission when the program uses
/// the network.
pub fn manifest(package: &str, label: &str, internet: bool) -> String {
    let permissions = if internet {
        "<uses-permission android:name=\"android.permission.INTERNET\" />"
    } else {
        ""
    };
    MANIFEST
        .replace("__PACKAGE__", package)
        .replace("__LABEL__", &escape(label))
        .replace("__PERMISSIONS__", permissions)
}

/// Runs a tool, failing with what it said.
fn run(command: &mut Command, what: &str) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|error| format!("could not start {what}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{what} failed ({}):\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// The debug key Android's tools share, in `.android` in the home
/// directory, made the first time as they make it.
fn debug_key(tools: &Packaging) -> Result<PathBuf, String> {
    let home = std::env::var_os("ANDROID_USER_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|home| PathBuf::from(home).join(".android"))
        })
        .ok_or("no home directory for Android's debug key")?;
    let keystore = home.join("debug.keystore");
    if keystore.is_file() {
        return Ok(keystore);
    }
    std::fs::create_dir_all(&home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    run(
        Command::new(&tools.keytool)
            .args(["-genkeypair", "-keystore"])
            .arg(&keystore)
            .args(["-storepass", "android", "-alias", "androiddebugkey"])
            .args(["-keypass", "android", "-keyalg", "RSA", "-keysize", "2048"])
            .args([
                "-validity",
                "10000",
                "-dname",
                "CN=Android Debug,O=Android,C=US",
            ]),
        "keytool",
    )?;
    Ok(keystore)
}

/// Makes the package `output` holding `library`, the program built for
/// `target`, named `name`. A debuggable package lets `adb` look inside.
pub fn package(
    tools: &Packaging,
    library: &Path,
    target: &Target,
    name: &str,
    output: &Path,
    debuggable: bool,
    internet: bool,
) -> Result<(), String> {
    let work = library.parent().unwrap_or(Path::new("."));
    let manifest_path = work.join("AndroidManifest.xml");
    let package = package_name(name);
    std::fs::write(&manifest_path, manifest(&package, name, internet))
        .map_err(|e| format!("cannot write {}: {e}", manifest_path.display()))?;
    let linked = work.join("linked.apk");
    let mut command = Command::new(&tools.aapt2);
    command
        .arg("link")
        .arg("-o")
        .arg(&linked)
        .arg("-I")
        .arg(&tools.platform)
        .arg("--manifest")
        .arg(&manifest_path)
        .args(["--min-sdk-version", &MIN_API.to_string()])
        .args(["--target-sdk-version", &TARGET_API.to_string()])
        .args([
            "--version-code",
            "1",
            "--version-name",
            env!("CARGO_PKG_VERSION"),
        ]);
    if debuggable {
        command.arg("--debug-mode");
    }
    run(&mut command, "aapt2")?;
    let read = |path: &Path| {
        std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
    };
    let mut entries = zip::read(&read(&linked)?)?;
    let library_name = format!("lib/{}/libmain.so", abi(target));
    entries.push(zip::Entry::stored(&library_name, read(library)?));
    let unsigned = work.join("unsigned.apk");
    let aligned = zip::write(
        &entries,
        |entry| if entry.ends_with(".so") { 16384 } else { 4 },
    );
    std::fs::write(&unsigned, aligned)
        .map_err(|e| format!("cannot write {}: {e}", unsigned.display()))?;
    let keystore = debug_key(tools)?;
    run(
        Command::new(&tools.java)
            .arg("-jar")
            .arg(&tools.apksigner)
            .args(["sign", "--ks"])
            .arg(&keystore)
            .args(["--ks-pass", "pass:android", "--key-pass", "pass:android"])
            .args(["--ks-key-alias", "androiddebugkey", "--alignment-preserved"])
            .args(["--v4-signing-enabled", "false"])
            .args(["--min-sdk-version", &MIN_API.to_string(), "--out"])
            .arg(output)
            .arg(&unsigned),
        "apksigner",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_hold_only_what_android_allows() {
        assert_eq!(package_name("hello"), "sayon.app.hello");
        assert_eq!(package_name("Word Count-2"), "sayon.app.word_count_2");
        assert_eq!(package_name("3d"), "sayon.app.p3d");
    }

    #[test]
    fn the_manifest_names_the_package_and_escapes_the_label() {
        let text = manifest("sayon.app.a", "A & B", true);
        assert!(text.contains("package=\"sayon.app.a\""));
        assert!(text.contains("android:label=\"A &amp; B\""));
        assert!(text.contains("android.permission.INTERNET"));
        assert!(!manifest("sayon.app.a", "a", false).contains("INTERNET"));
    }
}
