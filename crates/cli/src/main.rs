//! The `sayon` command.

use sayon_syntax::diagnostic::render;
use sayon_syntax::{Diagnostics, SourceMap};
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
    /// The native build could not run (toolchain missing or linking failed).
    pub const BUILD_FAILED: u8 = 4;
    /// An input file could not be read.
    pub const INPUT: u8 = 66;
}

const HELP: &str = "\
Sayon: a readable language for scripts, applications, and systems programs

Usage:
    sayon run <file.sayon> [-- args]     Check and run a program with the interpreter
    sayon run --native <file.sayon>      Compile to a native executable and run it
    sayon build <file.sayon> [options]   Compile a native executable
    sayon check <file.sayon> [options]   Report problems without running anything
    sayon test [files...]                Run test blocks (default: every file under tests/)
    sayon fmt [paths...] [--check]       Format source files (default: the current directory)
    sayon version                        Print the version
    sayon help                           Print this help

Options:
    --color <auto|always|never>          Color diagnostics (default: auto)
    -q, --quiet                          Print only errors
    -o, --output <path>                  Executable to write (build; default: next to the source)
    --release                            Optimize the native code (build, run --native)
    --emit-llvm                          Write the LLVM IR instead of an executable (build)

Native builds use clang from LLVM. Set SAYON_CLANG to choose a clang
executable and SAYON_RUNTIME_LIB to choose the runtime library.

Exit status:
    0   success, or the value returned by `main`
    1   the program raised an error that nothing caught
    2   the source has errors, so nothing ran
    3   execution stopped abnormally (for example runaway recursion)
    4   the native build failed (toolchain missing or linking failed)
    64  invalid command line
    66  an input file could not be read
";

struct Options {
    color: bool,
    quiet: bool,
    native: bool,
    release: bool,
    emit_llvm: bool,
    output: Option<PathBuf>,
    /// Arguments after `--`, passed to the program.
    program_args: Vec<String>,
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
        "fmt" => {
            let mut paths = Vec::new();
            let mut check_only = false;
            for arg in &args[1..] {
                match arg.as_str() {
                    "--check" => check_only = true,
                    other if other.starts_with('-') => {
                        return usage_error(&format!("unknown option `{other}`"));
                    }
                    other => paths.push(PathBuf::from(other)),
                }
            }
            format_files(paths, check_only)
        }
        "test" => {
            let mut files = Vec::new();
            let mut color_choice = "auto".to_string();
            let mut rest = args[1..].iter();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--color" => match rest.next() {
                        Some(value) => color_choice = value.clone(),
                        None => {
                            return usage_error("--color needs a value: auto, always, or never");
                        }
                    },
                    other if other.starts_with("--color=") => {
                        color_choice = other["--color=".len()..].to_string()
                    }
                    other if other.starts_with('-') => {
                        return usage_error(&format!("unknown option `{other}`"));
                    }
                    other => files.push(PathBuf::from(other)),
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
            let options = Options {
                color,
                quiet: false,
                native: false,
                release: false,
                emit_llvm: false,
                output: None,
                program_args: Vec::new(),
            };
            test(files, &options)
        }
        "run" | "check" | "build" => {
            let mut file = None;
            let mut color_choice = "auto".to_string();
            let mut quiet = false;
            let mut native = false;
            let mut release = false;
            let mut emit_llvm = false;
            let mut output = None;
            let mut program_args = Vec::new();
            let mut rest = args[1..].iter();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--" if command == "run" => {
                        program_args = rest.by_ref().cloned().collect();
                    }
                    "-q" | "--quiet" => quiet = true,
                    "--native" if command == "run" => native = true,
                    "--release" if command != "check" => release = true,
                    "--emit-llvm" if command == "build" => emit_llvm = true,
                    "-o" | "--output" if command == "build" => match rest.next() {
                        Some(value) => output = Some(PathBuf::from(value)),
                        None => return usage_error("--output needs a path"),
                    },
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
            let options = Options {
                color,
                quiet,
                native,
                release,
                emit_llvm,
                output,
                program_args,
            };
            match command.as_str() {
                "run" if options.native => run_native(&file, &options),
                "run" => run(&file, &options),
                "build" => build(&file, &options),
                _ => check(&file, &options),
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

/// The standard library directory: `SAYON_STDLIB`, then `stdlib` next to
/// the executable or up to two levels above it (an installation's `bin`
/// directory, or a development build in `target/<profile>`).
fn stdlib_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SAYON_STDLIB") {
        return Some(PathBuf::from(path));
    }
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    for _ in 0..3 {
        let candidate = dir.join("stdlib");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// Loads, checks, and lowers a program, printing any diagnostics.
fn compile(path: &Path, options: &Options) -> Result<Compiled, u8> {
    compile_program(path, options, false)
}

fn compile_program(path: &Path, options: &Options, tests: bool) -> Result<Compiled, u8> {
    let driver_options = sayon_driver::Options {
        stdlib: stdlib_dir(),
        tests,
    };
    let output = match sayon_driver::compile(path, &driver_options) {
        Ok(output) => output,
        Err(sayon_driver::EntryError(message)) => {
            eprintln!("error: {message}");
            return Err(status::INPUT);
        }
    };
    print_diagnostics(&output.diagnostics, &output.sources, options);
    match output.module {
        Some(module) if !output.diagnostics.has_errors() => Ok(Compiled {
            sources: output.sources,
            module,
        }),
        _ => Err(status::COMPILE_FAILED),
    }
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
    sayon_runtime::host::set_arguments(options.program_args.clone());
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

/// Where native build intermediates go: a fresh directory under the system
/// temporary directory, removed afterwards.
struct WorkDir(PathBuf);

impl WorkDir {
    fn create() -> Result<WorkDir, String> {
        let base = std::env::temp_dir();
        let path = base.join(format!(
            "sayon-build-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        Ok(WorkDir(path))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn executable_name(source: &Path) -> PathBuf {
    let stem = source
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| "program".into());
    let mut path = source.with_file_name(stem);
    if cfg!(windows) {
        path.set_extension("exe");
    }
    path
}

/// Compiles a checked program to a native executable at `output`.
fn compile_native(compiled: &Compiled, output: &Path, release: bool) -> Result<(), String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let toolchain = sayon_codegen::Toolchain::locate(exe_dir.as_deref())?;
    let work = WorkDir::create()?;
    let ir_path = work.0.join("program.ll");
    let ir = sayon_codegen::emit_llvm(&compiled.module, &compiled.sources);
    std::fs::write(&ir_path, ir).map_err(|e| format!("cannot write {}: {e}", ir_path.display()))?;
    sayon_codegen::link(&toolchain, &ir_path, output, release)
}

fn build(path: &Path, options: &Options) -> u8 {
    let compiled = match compile(path, options) {
        Ok(compiled) => compiled,
        Err(code) => return code,
    };
    if options.emit_llvm {
        let target = options
            .output
            .clone()
            .unwrap_or_else(|| path.with_extension("ll"));
        let ir = sayon_codegen::emit_llvm(&compiled.module, &compiled.sources);
        return match std::fs::write(&target, ir) {
            Ok(()) => {
                if !options.quiet {
                    eprintln!("wrote {}", target.display());
                }
                status::SUCCESS
            }
            Err(error) => {
                eprintln!("error: cannot write `{}`: {error}", target.display());
                status::BUILD_FAILED
            }
        };
    }
    let output = options
        .output
        .clone()
        .unwrap_or_else(|| executable_name(path));
    match compile_native(&compiled, &output, options.release) {
        Ok(()) => {
            if !options.quiet {
                eprintln!("built {}", output.display());
            }
            status::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            status::BUILD_FAILED
        }
    }
}

fn run_native(path: &Path, options: &Options) -> u8 {
    let compiled = match compile(path, options) {
        Ok(compiled) => compiled,
        Err(code) => return code,
    };
    let work = match WorkDir::create() {
        Ok(work) => work,
        Err(message) => {
            eprintln!("error: {message}");
            return status::BUILD_FAILED;
        }
    };
    let executable = work.0.join(if cfg!(windows) {
        "program.exe"
    } else {
        "program"
    });
    if let Err(message) = compile_native(&compiled, &executable, options.release) {
        eprintln!("error: {message}");
        return status::BUILD_FAILED;
    }
    match std::process::Command::new(&executable)
        .args(&options.program_args)
        .status()
    {
        Ok(exit) => exit
            .code()
            .map(|code| code.clamp(0, 255) as u8)
            .unwrap_or(status::FATAL),
        Err(error) => {
            eprintln!("error: could not start the compiled program: {error}");
            status::BUILD_FAILED
        }
    }
}

/// Every `.sayon` file under a directory, in a stable order.
fn sayon_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            sayon_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "sayon") {
            out.push(path);
        }
    }
}

/// Runs the test blocks of each file in the interpreter. Each test runs with
/// freshly initialized constants, and its output is shown only if it fails.
fn test(mut files: Vec<PathBuf>, options: &Options) -> u8 {
    if files.is_empty() {
        sayon_files(Path::new("tests"), &mut files);
        if files.is_empty() {
            eprintln!(
                "error: no tests found; give test files or add .sayon files with test blocks under tests/"
            );
            return status::USAGE;
        }
    }
    let (mut passed, mut failed, mut broken) = (0usize, 0usize, 0usize);
    for file in &files {
        let compiled = match compile_program(file, options, true) {
            Ok(compiled) => compiled,
            Err(_) => {
                broken += 1;
                continue;
            }
        };
        if compiled.module.tests.is_empty() {
            continue;
        }
        println!("{}", file.display());
        for (name, function) in &compiled.module.tests {
            let mut output = Vec::new();
            let result =
                sayon_interp::Machine::new(&compiled.module, &compiled.sources, &mut output)
                    .run_test(*function);
            match result {
                Ok(()) => {
                    passed += 1;
                    println!("  ok      {name}");
                }
                Err(error) => {
                    failed += 1;
                    let label = if options.color {
                        "\x1b[1;31mFAILED\x1b[0m"
                    } else {
                        "FAILED"
                    };
                    println!("  {label}  {name}");
                    match error {
                        sayon_interp::RunError::Uncaught(error) => {
                            println!("          {}: {}", error.code, error.message);
                            if let Some(location) = error.location {
                                println!("            --> {location}");
                            }
                        }
                        sayon_interp::RunError::Fatal(message) => {
                            println!("          fatal: {message}")
                        }
                    }
                    let captured = String::from_utf8_lossy(&output);
                    if !captured.is_empty() {
                        println!("          output:");
                        for line in captured.lines() {
                            println!("            {line}");
                        }
                    }
                }
            }
        }
    }
    let total = passed + failed;
    let mut summary = format!(
        "{total} test{}: {passed} passed, {failed} failed",
        if total == 1 { "" } else { "s" }
    );
    if broken > 0 {
        summary.push_str(&format!(
            ", {broken} file{} did not compile",
            if broken == 1 { "" } else { "s" }
        ));
    }
    println!("{summary}");
    if failed > 0 {
        status::PROGRAM_FAILED
    } else if broken > 0 {
        status::COMPILE_FAILED
    } else {
        status::SUCCESS
    }
}

/// Collects `.sayon` files under a directory, skipping build output and
/// version control directories.
fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if path.is_dir() {
            if !matches!(name.as_str(), "target" | ".git" | "node_modules")
                && !name.starts_with('.')
            {
                source_files(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "sayon") {
            out.push(path);
        }
    }
}

/// Formats files in place, or with `check_only` reports the files that are
/// not formatted. Files with syntax errors are reported and left unchanged.
fn format_files(paths: Vec<PathBuf>, check_only: bool) -> u8 {
    let mut files = Vec::new();
    let roots = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths
    };
    for root in roots {
        if root.is_dir() {
            source_files(&root, &mut files);
        } else {
            files.push(root);
        }
    }
    let (mut changed, mut failed) = (0usize, 0usize);
    for file in &files {
        let text = match std::fs::read(file) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    eprintln!(
                        "error: {} is not valid UTF-8; left unchanged",
                        file.display()
                    );
                    failed += 1;
                    continue;
                }
            },
            Err(error) => {
                eprintln!("error: cannot read `{}`: {error}", file.display());
                failed += 1;
                continue;
            }
        };
        let mut sources = SourceMap::new();
        let id = sources.add(file.display().to_string(), Some(file), text.clone());
        match sayon_syntax::format::format_source(id, &text) {
            Ok(formatted) => {
                if formatted == text {
                    continue;
                }
                changed += 1;
                if check_only {
                    println!("would reformat {}", file.display());
                } else if let Err(error) = std::fs::write(file, formatted) {
                    eprintln!("error: cannot write `{}`: {error}", file.display());
                    failed += 1;
                } else {
                    println!("formatted {}", file.display());
                }
            }
            Err(sayon_syntax::format::FormatError::Invalid(diagnostics)) => {
                failed += 1;
                for diagnostic in diagnostics.iter().filter(|d| d.is_error()) {
                    eprintln!("{}", render(diagnostic, &sources, false));
                }
                eprintln!("{} has syntax errors; left unchanged", file.display());
            }
            Err(sayon_syntax::format::FormatError::Changed) => {
                failed += 1;
                eprintln!(
                    "error: formatting {} would change its meaning; it was left unchanged (please report this)",
                    file.display()
                );
            }
        }
    }
    if failed > 0 {
        status::COMPILE_FAILED
    } else if check_only && changed > 0 {
        status::PROGRAM_FAILED
    } else {
        status::SUCCESS
    }
}
