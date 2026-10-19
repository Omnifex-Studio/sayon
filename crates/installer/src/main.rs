//! The Sayon installer. Each release has one per device (operating system
//! and processor), with that device's Sayon files inside. It installs Sayon
//! for the current user, makes `sayon` available in new terminals, and
//! leaves an uninstaller in the installation folder.

mod archive;
mod install;
#[cfg_attr(windows, path = "windows.rs")]
#[cfg_attr(unix, path = "unix.rs")]
mod platform;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

include!(concat!(env!("OUT_DIR"), "/payload.rs"));

const VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASES: &str = "https://github.com/Omnifex-Studio/sayon/releases";

const HELP: &str = "\
Usage:
    sayon-setup [options]              Install Sayon for the current user
    sayon-setup uninstall [options]    Remove Sayon

Options:
    --prefix <folder>    Install into (or remove from) this folder instead
    -y, --yes            Do not ask for confirmation
    --no-path            Do not make `sayon` available in new terminals
    -h, --help           Print this help
    -V, --version        Print the version
";

/// The device the installer (and the Sayon inside it) is built for.
fn device() -> String {
    let target = env!("SAYON_INSTALLER_TARGET");
    let system = if target.contains("windows") {
        "Windows"
    } else if target.contains("apple") {
        "macOS"
    } else if target.contains("linux") {
        "Linux"
    } else {
        target
    };
    let processor = match target.split('-').next() {
        Some("x86_64") => "x64",
        Some("aarch64") => "ARM64",
        Some(other) => other,
        None => "",
    };
    format!("{system} {processor}")
}

struct Options {
    uninstall: bool,
    prefix: Option<PathBuf>,
    yes: bool,
    modify_path: bool,
}

fn parse(args: &[String]) -> Result<Option<Options>, String> {
    // The copy left in the installation folder uninstalls by default.
    let as_uninstaller = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().to_lowercase()))
        .is_some_and(|stem| stem == "uninstall");
    let mut options = Options {
        uninstall: as_uninstaller,
        prefix: None,
        yes: false,
        modify_path: true,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "install" => options.uninstall = false,
            "uninstall" => options.uninstall = true,
            "-y" | "--yes" => options.yes = true,
            "--no-path" => options.modify_path = false,
            "--prefix" => {
                let value = iter.next().ok_or("--prefix needs a folder")?;
                options.prefix = Some(PathBuf::from(value));
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("sayon-setup {VERSION} ({})", device());
                return Ok(None);
            }
            other => match other.strip_prefix("--prefix=") {
                Some(value) => options.prefix = Some(PathBuf::from(value)),
                None => return Err(format!("unknown option `{other}`")),
            },
        }
    }
    Ok(Some(options))
}

/// Asks a yes-or-no question; `default` is the answer to an empty line.
/// Without an answer (no input at all) nothing is changed.
fn confirm(question: &str, default: bool) -> bool {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{question} {hint} ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => {
            println!();
            println!("No answer was given; nothing was changed. Pass --yes to answer yes.");
            false
        }
        Ok(_) => match line.trim().to_lowercase().as_str() {
            "" => default,
            "y" | "yes" => true,
            _ => false,
        },
    }
}

fn install(options: &Options) -> Result<(), String> {
    if PAYLOAD.is_empty() {
        return Err(format!(
            "this copy of the installer has no Sayon files in it\ndownload the installer for your device from {RELEASES}, or build one with {}",
            if cfg!(windows) {
                "scripts/package.ps1"
            } else {
                "scripts/package.sh"
            }
        ));
    }
    let prefix = match &options.prefix {
        Some(prefix) => absolute(prefix)?,
        None => platform::default_prefix().ok_or("cannot find your home folder; pass --prefix")?,
    };
    install::check_destination(&prefix)?;

    println!("Sayon {VERSION} for {}", device());
    println!();
    match install::installed_version(&prefix) {
        Some(old) if old == VERSION => {
            println!("Sayon {old} is already installed; it will be reinstalled.")
        }
        Some(old) => println!("Sayon {old} is installed; it will be replaced with {VERSION}."),
        None => {}
    }
    println!("This installs Sayon for your user account:");
    println!("  - into {}", prefix.display());
    for change in platform::changes(&prefix, options.modify_path) {
        println!("  - {change}");
    }
    println!();
    if !options.yes && !confirm("Install Sayon now?", true) {
        println!("Nothing was installed.");
        return Ok(());
    }

    print!("Unpacking...");
    let _ = std::io::stdout().flush();
    let files = install::layout(archive::read_zip(PAYLOAD)?);
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the installer: {e}"))?;
    let bytes = install::unpack(
        &files,
        &prefix,
        VERSION,
        Some((&exe, platform::UNINSTALLER)),
    )?;
    println!(" {} files", files.len());
    platform::register(&prefix, VERSION, bytes, options.modify_path)?;

    println!();
    let sayon = prefix
        .join("bin")
        .join(format!("sayon{}", platform::EXECUTABLE_SUFFIX));
    // `sayon doctor` reports what works now and what native builds still need.
    let _ = std::process::Command::new(&sayon).arg("doctor").status();
    println!();
    println!("Sayon {VERSION} is installed.");
    if options.modify_path {
        println!("Open a new terminal, then try:");
        println!("    sayon init hello");
        println!("    cd hello");
        println!("    sayon run");
    } else {
        println!("Run it as {}", sayon.display());
    }
    let uninstaller = prefix.join(platform::UNINSTALLER);
    match platform::APP_SETTINGS {
        Some(settings) => println!(
            "To remove it later, run {} or use {settings}.",
            uninstaller.display()
        ),
        None => println!("To remove it later, run {}.", uninstaller.display()),
    }
    Ok(())
}

fn uninstall(options: &Options) -> Result<(), String> {
    let prefix = match &options.prefix {
        Some(prefix) => absolute(prefix)?,
        None => locate_installation().ok_or("no Sayon installation was found; pass --prefix")?,
    };
    let Some(version) = install::installed_version(&prefix) else {
        return Err(format!("{} is not a Sayon installation", prefix.display()));
    };
    println!("This removes Sayon {version} from {}.", prefix.display());
    if !options.yes && !confirm("Uninstall Sayon?", false) {
        println!("Nothing was removed.");
        return Ok(());
    }
    let moved = platform::move_running_uninstaller(&prefix);
    let running = std::env::current_exe().ok();
    let keep = if moved.is_some() {
        None
    } else {
        running.as_deref()
    };
    let gone = match install::remove(&prefix, keep) {
        Ok(gone) => gone,
        Err(message) => {
            // Keep the installation registered so that removing it can be retried.
            if let Some(moved) = &moved {
                let _ = std::fs::rename(moved, prefix.join(platform::UNINSTALLER));
            }
            return Err(message);
        }
    };
    platform::unregister(&prefix)?;
    match (moved, gone) {
        (Some(moved), _) => platform::delete_after_exit(&moved, None),
        (None, false) => {
            if let Some(running) = &running {
                platform::delete_after_exit(running, Some(&prefix));
            }
        }
        (None, true) => {}
    }
    println!("Sayon {version} was removed.");
    Ok(())
}

/// The installation to remove: the one this uninstaller belongs to, the
/// one recorded at installation, or the default location.
fn locate_installation() -> Option<PathBuf> {
    let own = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    [
        own,
        platform::registered_prefix(),
        platform::default_prefix(),
    ]
    .into_iter()
    .flatten()
    .find(|prefix| install::installed_version(prefix).is_some())
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|dir| dir.join(path))
        .map_err(|e| format!("cannot read the current folder: {e}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (status, unattended) = match parse(&args) {
        Ok(None) => (0, true),
        Ok(Some(options)) => {
            let result = if options.uninstall {
                uninstall(&options)
            } else {
                install(&options)
            };
            match result {
                Ok(()) => (0, options.yes),
                Err(message) => {
                    eprintln!();
                    eprintln!("error: {message}");
                    (1, options.yes)
                }
            }
        }
        Err(message) => {
            eprintln!("error: {message}");
            eprint!("{HELP}");
            (64, false)
        }
    };
    // Keep a window opened just for the installer readable until it closes.
    if !unattended && platform::owns_console() {
        print!("\nPress Enter to close this window.");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
    }
    ExitCode::from(status)
}
