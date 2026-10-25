//! The `sayon` command.

mod commands;
mod compile;
mod explain;

use std::collections::HashMap;
use std::process::ExitCode;

/// Exit statuses. A program's own `main` result is passed through unchanged.
pub mod status {
    pub const SUCCESS: u8 = 0;
    /// The program raised an error nothing caught, or a test failed.
    pub const PROGRAM_FAILED: u8 = 1;
    /// The source has errors; nothing ran.
    pub const COMPILE_FAILED: u8 = 2;
    /// Execution stopped without cleanup (for example runaway recursion).
    pub const FATAL: u8 = 3;
    /// The native build could not run (toolchain missing or linking failed).
    pub const BUILD_FAILED: u8 = 4;
    /// The command line was invalid.
    pub const USAGE: u8 = 64;
    /// An input file could not be read.
    pub const INPUT: u8 = 66;
}

const HELP: &str = "\
Sayon: a readable language for scripts, applications, and systems programs

Usage:
    sayon run [file] [-- args]       Check and run a program with the interpreter
    sayon run --native [file]        Compile to a native executable and run it
    sayon build [file]               Compile a native executable
    sayon check [file]               Report problems without running anything
    sayon lint [file]                Also report likely mistakes and naming problems
    sayon test [files...]            Run test blocks
    sayon fmt [paths...] [--check]   Format source files
    sayon init <name>                Create a project
    sayon install [--locked]         Fetch dependencies and write sayon.lock
    sayon clean                      Remove a project's build output
    sayon doctor                     Check the installation and native toolchain
    sayon target list                List the targets and what each provides
    sayon target setup <target>      Show what building for a target needs
    sayon explain <code>             Explain an error code, such as E_MOVED_VALUE
    sayon version                    Print the version
    sayon help                       Print this help

Without a file, run, build, check, and test use the project in the current
directory (or a parent): the one whose sayon.toml names its entry file.

Options:
    --color <auto|always|never>      Color diagnostics (default: auto)
    -q, --quiet                      Print only errors
    -o, --output <path>              Executable to write (build)
    --target <target>                Check or build for a target, such as
                                     linux-x64, web, or all (check, lint,
                                     build; repeatable for check and build)
    --release                        Optimize native code (build, run --native)
    --emit-llvm                      Write LLVM IR instead of an executable (build)
    --executable                     For Android, a program to run from adb shell
                                     instead of an app's package (build)
    --check                          Report unformatted files without changing them (fmt)
    --deny warnings                  Fail when there are warnings (lint)
    --locked                         Fail instead of changing sayon.lock (install)
    --offline                        Use only dependencies already fetched (install)

Native builds use clang from LLVM. Set SAYON_CLANG to choose a clang
executable, SAYON_RUNTIME_LIB the runtime library, and SAYON_STDLIB the
standard library directory.

Exit status:
    0   success, or the value returned by `main`
    1   the program raised an error that nothing caught, or a test failed
    2   the source has errors, so nothing ran
    3   execution stopped abnormally (for example runaway recursion)
    4   the native build failed (toolchain missing or linking failed)
    64  invalid command line
    66  an input file could not be read
";

/// Parsed command-line arguments for one command.
pub struct Args {
    pub positional: Vec<String>,
    pub flags: Vec<String>,
    /// The last value of each valued option.
    pub values: HashMap<String, String>,
    /// Every value of each valued option, in order, for those that repeat.
    pub repeated: Vec<(String, String)>,
    /// Words after `--`.
    pub rest: Vec<String>,
}

impl Args {
    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    /// Every value given for an option that can repeat.
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.repeated
            .iter()
            .filter(|(option, _)| option == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// Splits arguments into positionals, known flags, and known valued options
/// (written `--name value` or `--name=value`).
fn parse_args(
    args: &[String],
    flags: &[&str],
    valued: &[&str],
    allow_rest: bool,
) -> Result<Args, String> {
    let mut parsed = Args {
        positional: Vec::new(),
        flags: Vec::new(),
        values: HashMap::new(),
        repeated: Vec::new(),
        rest: Vec::new(),
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" && allow_rest {
            parsed.rest = iter.by_ref().cloned().collect();
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            parsed.positional.push(arg.clone());
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let canonical = match name {
            "-q" => "--quiet",
            "-o" => "--output",
            other => other,
        };
        if flags.contains(&canonical) && inline.is_none() {
            parsed.flags.push(canonical.to_string());
        } else if valued.contains(&canonical) {
            let value = match inline {
                Some(value) => value,
                None => iter
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))?,
            };
            parsed.repeated.push((canonical.to_string(), value.clone()));
            parsed.values.insert(canonical.to_string(), value);
        } else {
            return Err(format!("unknown option `{arg}`"));
        }
    }
    Ok(parsed)
}

pub fn usage_error(message: &str) -> u8 {
    eprintln!("error: {message}");
    eprintln!("run `sayon help` for usage");
    status::USAGE
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(dispatch(&args))
}

fn dispatch(args: &[String]) -> u8 {
    let Some(command) = args.first() else {
        print!("{HELP}");
        return status::SUCCESS;
    };
    let rest = &args[1..];
    let (flags, valued, allow_rest): (&[&str], &[&str], bool) = match command.as_str() {
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            return status::SUCCESS;
        }
        "version" | "--version" | "-V" => {
            println!("sayon {}", env!("CARGO_PKG_VERSION"));
            return status::SUCCESS;
        }
        "run" => (&["--quiet", "--native", "--release"], &["--color"], true),
        "check" => (&["--quiet"], &["--color", "--target"], false),
        "lint" => (&["--quiet"], &["--color", "--deny", "--target"], false),
        "build" => (
            &["--quiet", "--release", "--emit-llvm", "--executable"],
            &["--color", "--output", "--target"],
            false,
        ),
        "test" => (&["--quiet"], &["--color"], false),
        "fmt" => (&["--check"], &[], false),
        "install" => (&["--quiet", "--locked", "--offline"], &["--color"], false),
        "init" | "clean" | "doctor" | "explain" | "target" => (&[], &[], false),
        other => return usage_error(&format!("unknown command `{other}`")),
    };
    let parsed = match parse_args(rest, flags, valued, allow_rest) {
        Ok(parsed) => parsed,
        Err(message) => return usage_error(&message),
    };
    let color = match parsed
        .values
        .get("--color")
        .map(String::as_str)
        .unwrap_or("auto")
    {
        "always" => true,
        "never" => false,
        "auto" => compile::color_default(),
        other => {
            return usage_error(&format!(
                "unknown color choice `{other}`; use auto, always, or never"
            ));
        }
    };
    let options = compile::Options {
        color,
        quiet: parsed.flag("--quiet"),
    };
    match command.as_str() {
        "run" | "check" | "build" | "lint" => {
            if parsed.positional.len() > 1 {
                return usage_error(&format!("`sayon {command}` takes one source file"));
            }
            let source =
                match compile::Source::resolve(parsed.positional.first(), command, &options) {
                    Ok(source) => source,
                    Err(code) => return code,
                };
            let targets = match compile::targets(&parsed.all("--target"), &source, &options) {
                Ok(targets) => targets,
                Err(code) => return code,
            };
            match command.as_str() {
                "run" if parsed.flag("--native") => {
                    commands::run_native(&source, &options, parsed.flag("--release"), &parsed.rest)
                }
                "run" => commands::run(&source, &options, &parsed.rest),
                "check" => commands::check(&source, &options, &targets),
                "lint" => {
                    let deny = match parsed.values.get("--deny").map(String::as_str) {
                        None => false,
                        Some("warnings") => true,
                        Some(other) => {
                            return usage_error(&format!(
                                "`--deny {other}` is not supported; use `--deny warnings`"
                            ));
                        }
                    };
                    let [target] = targets.as_slice() else {
                        return usage_error("`sayon lint` checks one target at a time");
                    };
                    commands::lint(&source, &options, deny, target)
                }
                _ => commands::build(
                    &source,
                    &options,
                    parsed.values.get("--output").map(Into::into),
                    parsed.flag("--release"),
                    parsed.flag("--emit-llvm"),
                    parsed.flag("--executable"),
                    &targets,
                ),
            }
        }
        "test" => commands::test(&parsed.positional, &options),
        "fmt" => commands::format_files(&parsed.positional, parsed.flag("--check")),
        "init" => match parsed.positional.as_slice() {
            [name] => commands::init(name),
            _ => usage_error("`sayon init` takes a project name, for example `sayon init hello`"),
        },
        "clean" => commands::clean(&options),
        "doctor" => commands::doctor(),
        "target" => match parsed.positional.as_slice() {
            [list] if list == "list" => commands::target_list(),
            [setup, name] if setup == "setup" => commands::target_setup(name),
            _ => usage_error(
                "`sayon target` takes `list`, or `setup` and a target, such as `sayon target setup wasi`",
            ),
        },
        "explain" => explain::explain(&parsed.positional),
        "install" => {
            if !parsed.positional.is_empty() {
                return usage_error(
                    "`sayon install` installs the dependencies in sayon.toml and takes no names",
                );
            }
            commands::install(&options, parsed.flag("--locked"), parsed.flag("--offline"))
        }
        _ => unreachable!("checked above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn parses_flags_values_and_rest() {
        let parsed = parse_args(
            &args(&[
                "main.sayon",
                "-q",
                "--color=never",
                "-o",
                "out.exe",
                "--",
                "a",
                "--b",
            ]),
            &["--quiet"],
            &["--color", "--output"],
            true,
        )
        .ok()
        .unwrap();
        assert_eq!(parsed.positional, vec!["main.sayon"]);
        assert!(parsed.flag("--quiet"));
        assert_eq!(parsed.values["--color"], "never");
        assert_eq!(parsed.values["--output"], "out.exe");
        assert_eq!(parsed.rest, vec!["a", "--b"]);
        assert!(parse_args(&args(&["--nope"]), &[], &[], false).is_err());
        assert!(parse_args(&args(&["--color"]), &[], &["--color"], false).is_err());
    }
}
