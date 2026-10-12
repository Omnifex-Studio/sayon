//! The `sayon` command.

use sayon_syntax::diagnostic::render;
use sayon_syntax::{Diagnostic, Diagnostics, SourceMap, Span};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Exit statuses. A program's own `main` result is passed through unchanged.
mod status {
    pub const SUCCESS: u8 = 0;
    /// The program raised an error nothing caught.
    pub const PROGRAM_FAILED: u8 = 1;
    /// The source has errors; nothing ran.
    pub const COMPILE_FAILED: u8 = 2;
    /// Execution stopped without cleanup (for example runaway recursion).
    pub const FATAL: u8 = 3;
    /// The command line was invalid.
    pub const USAGE: u8 = 64;
    /// An input file could not be read.
    pub const INPUT: u8 = 66;
}

const HELP: &str = "\
Sayon: a readable language for scripts, applications, and systems programs

Usage:
    sayon run <file.sayon> [options]     Check and run a program with the interpreter
    sayon check <file.sayon> [options]   Report problems without running anything
    sayon version                        Print the version
    sayon help                           Print this help

Options:
    --color <auto|always|never>          Color diagnostics (default: auto)
    -q, --quiet                          Print only errors

Exit status:
    0   success, or the value returned by `main`
    1   the program raised an error that nothing caught
    2   the source has errors, so nothing ran
    3   execution stopped abnormally (for example runaway recursion)
    64  invalid command line
    66  an input file could not be read
";

struct Options {
    color: bool,
    quiet: bool,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(dispatch(&args))
}

fn usage_error(message: &str) -> u8 {
    eprintln!("error: {message}");
    eprintln!("run `sayon help` for usage");
    status::USAGE
}

fn dispatch(args: &[String]) -> u8 {
    let Some(command) = args.first() else {
        print!("{HELP}");
        return status::SUCCESS;
    };
    match command.as_str() {
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            status::SUCCESS
        }
        "version" | "--version" | "-V" => {
            println!("sayon {}", env!("CARGO_PKG_VERSION"));
            status::SUCCESS
        }
        "run" | "check" => {
            let mut file = None;
            let mut color_choice = "auto".to_string();
            let mut quiet = false;
            let mut rest = args[1..].iter();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "-q" | "--quiet" => quiet = true,
                    "--color" => match rest.next() {
                        Some(value) => color_choice = value.clone(),
                        None => {
                            return usage_error("--color needs a value: auto, always, or never");
                        }
                    },
                    other if other.starts_with("--color=") => {
                        color_choice = other["--color=".len()..].to_string()
                    }
                    "-h" | "--help" => {
                        print!("{HELP}");
                        return status::SUCCESS;
                    }
                    other if other.starts_with('-') => {
                        return usage_error(&format!("unknown option `{other}`"));
                    }
                    other => {
                        if file.is_some() {
                            return usage_error(&format!(
                                "unexpected argument `{other}`; give one source file"
                            ));
                        }
                        file = Some(PathBuf::from(other));
                    }
                }
            }
            let color = match color_choice.as_str() {
                "always" => true,
                "never" => false,
                "auto" => color_default(),
                other => {
                    return usage_error(&format!(
                        "unknown color choice `{other}`; use auto, always, or never"
                    ));
                }
            };
            let Some(file) = file else {
                return usage_error(&format!(
                    "`sayon {command}` needs a source file, for example `sayon {command} main.sayon`"
                ));
            };
            let options = Options { color, quiet };
            if command == "run" {
                run(&file, &options)
            } else {
                check(&file, &options)
            }
        }
        other => usage_error(&format!("unknown command `{other}`")),
    }
}

fn color_default() -> bool {
    if std::env::var_os("NO_COLOR").is_some() || !std::io::stderr().is_terminal() {
        return false;
    }
    if cfg!(windows) {
        // Classic consoles show escape codes literally; modern terminals set these.
        std::env::var_os("WT_SESSION").is_some() || std::env::var_os("TERM").is_some()
    } else {
        true
    }
}

fn print_diagnostics(diagnostics: &Diagnostics, sources: &SourceMap, options: &Options) {
    let mut stderr = std::io::stderr().lock();
    for diagnostic in diagnostics.iter() {
        if options.quiet && !diagnostic.is_error() {
            continue;
        }
        let _ = writeln!(stderr, "{}", render(diagnostic, sources, options.color));
    }
    let errors = diagnostics.error_count();
    if errors > 0 {
        let noun = if errors == 1 { "error" } else { "errors" };
        let _ = writeln!(stderr, "{errors} {noun} found; nothing was run");
    }
}

struct Compiled {
    sources: SourceMap,
    module: sayon_ir::Module,
}

fn load(path: &Path, sources: &mut SourceMap) -> Result<sayon_syntax::FileId, u8> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            let reason = match error.kind() {
                std::io::ErrorKind::NotFound => "the file does not exist".to_string(),
                std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
                _ => error.to_string(),
            };
            eprintln!("error: cannot read `{}`: {reason}", path.display());
            return Err(status::INPUT);
        }
    };
    let name = path.display().to_string();
    match String::from_utf8(bytes) {
        Ok(text) => Ok(sources.add(name, Some(path), text)),
        Err(error) => {
            let offset = error.utf8_error().valid_up_to();
            let text = String::from_utf8_lossy(error.as_bytes()).into_owned();
            let file = sources.add(name, Some(path), text);
            let diagnostic =
                Diagnostic::error("E_INVALID_UTF8", "source files must be valid UTF-8")
                    .primary(
                        Span::new(file, offset, offset + 1),
                        format!("invalid byte at offset {offset}"),
                    )
                    .help("save the file with UTF-8 encoding");
            eprintln!("{}", render(&diagnostic, sources, false));
            Err(status::COMPILE_FAILED)
        }
    }
}

/// Parses, checks, and lowers a program, printing any diagnostics.
fn compile(path: &Path, options: &Options) -> Result<Compiled, u8> {
    let mut sources = SourceMap::new();
    let file = load(path, &mut sources)?;
    let text = sources.get(file).text.clone();
    let (program, mut diagnostics) = sayon_check::check_source(file, &text);
    let module = match program {
        Some(program) if !diagnostics.has_errors() => {
            let lowered = sayon_ir::lower(&program);
            diagnostics.extend(lowered.diagnostics);
            lowered.module
        }
        _ => {
            diagnostics.sort();
            print_diagnostics(&diagnostics, &sources, options);
            return Err(status::COMPILE_FAILED);
        }
    };
    diagnostics.sort();
    print_diagnostics(&diagnostics, &sources, options);
    if diagnostics.has_errors() {
        return Err(status::COMPILE_FAILED);
    }
    Ok(Compiled { sources, module })
}

fn check(path: &Path, options: &Options) -> u8 {
    match compile(path, options) {
        Ok(_) => {
            if !options.quiet {
                eprintln!("{}: no problems found", path.display());
            }
            status::SUCCESS
        }
        Err(code) => code,
    }
}

fn run(path: &Path, options: &Options) -> u8 {
    let compiled = match compile(path, options) {
        Ok(compiled) => compiled,
        Err(code) => return code,
    };
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let result = sayon_interp::Machine::new(&compiled.module, &compiled.sources, &mut out).run();
    let _ = out.flush();
    drop(out);
    match result {
        Ok(code) => {
            if (0..=255).contains(&code) {
                code as u8
            } else {
                eprintln!(
                    "warning: `main` returned {code}; portable exit statuses are 0 to 255, so 255 is used"
                );
                255
            }
        }
        Err(sayon_interp::RunError::Uncaught(error)) => {
            let label = if options.color {
                "\x1b[1;31merror\x1b[0m"
            } else {
                "error"
            };
            eprintln!("{label}: uncaught {}: {}", error.code, error.message);
            if let Some(location) = error.location {
                eprintln!("  --> {location}");
            }
            status::PROGRAM_FAILED
        }
        Err(sayon_interp::RunError::Fatal(message)) => {
            eprintln!("fatal error: {message}");
            status::FATAL
        }
    }
}
