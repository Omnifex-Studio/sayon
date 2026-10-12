//! The commands.

use crate::compile::{
    Options, Target, WorkDir, compile, compile_native, current_project, stdlib_dir,
};
use crate::{status, usage_error};
use sayon_syntax::SourceMap;
use sayon_syntax::diagnostic::render;
use std::io::Write;
use std::path::{Path, PathBuf};

fn report_run_error(error: sayon_interp::RunError, options: &Options) -> u8 {
    match error {
        sayon_interp::RunError::Uncaught(error) => {
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
        sayon_interp::RunError::Fatal(message) => {
            eprintln!("fatal error: {message}");
            status::FATAL
        }
    }
}

pub fn check(target: &Target, options: &Options) -> u8 {
    match compile(&target.entry, target.root.as_deref(), options, false) {
        Ok(_) => {
            if !options.quiet {
                eprintln!("{}: no problems found", target.entry.display());
            }
            status::SUCCESS
        }
        Err(code) => code,
    }
}

pub fn run(target: &Target, options: &Options, program_args: &[String]) -> u8 {
    let compiled = match compile(&target.entry, target.root.as_deref(), options, false) {
        Ok(compiled) => compiled,
        Err(code) => return code,
    };
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    sayon_runtime::host::set_arguments(program_args.to_vec());
    let result = sayon_interp::Machine::new(&compiled.module, &compiled.sources, &mut out).run();
    let _ = out.flush();
    drop(out);
    match result {
        Ok(code) if (0..=255).contains(&code) => code as u8,
        Ok(code) => {
            eprintln!(
                "warning: `main` returned {code}; portable exit statuses are 0 to 255, so 255 is used"
            );
            255
        }
        Err(error) => report_run_error(error, options),
    }
}

pub fn run_native(
    target: &Target,
    options: &Options,
    release: bool,
    program_args: &[String],
) -> u8 {
    let compiled = match compile(&target.entry, target.root.as_deref(), options, false) {
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
    if let Err(message) = compile_native(&compiled, &executable, release) {
        eprintln!("error: {message}");
        return status::BUILD_FAILED;
    }
    match std::process::Command::new(&executable)
        .args(program_args)
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

fn with_executable_extension(mut path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        path.set_extension("exe");
    }
    path
}

/// Where `sayon build` writes: next to a standalone file, or under the
/// project's `build/debug` or `build/release` directory.
fn default_output(target: &Target, release: bool) -> PathBuf {
    match &target.project {
        Some(project) => {
            let profile = if release { "release" } else { "debug" };
            with_executable_extension(
                project
                    .root
                    .join("build")
                    .join(profile)
                    .join(&project.manifest.name),
            )
        }
        None => {
            let stem = target
                .entry
                .file_stem()
                .map(|s| s.to_os_string())
                .unwrap_or_else(|| "program".into());
            with_executable_extension(target.entry.with_file_name(stem))
        }
    }
}

pub fn build(
    target: &Target,
    options: &Options,
    output: Option<PathBuf>,
    release: bool,
    emit_llvm: bool,
) -> u8 {
    let compiled = match compile(&target.entry, target.root.as_deref(), options, false) {
        Ok(compiled) => compiled,
        Err(code) => return code,
    };
    if emit_llvm {
        let destination =
            output.unwrap_or_else(|| default_output(target, release).with_extension("ll"));
        let ir = sayon_codegen::emit_llvm(&compiled.module, &compiled.sources);
        if let Some(parent) = destination.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        return match std::fs::write(&destination, ir) {
            Ok(()) => {
                if !options.quiet {
                    eprintln!("wrote {}", destination.display());
                }
                status::SUCCESS
            }
            Err(error) => {
                eprintln!("error: cannot write `{}`: {error}", destination.display());
                status::BUILD_FAILED
            }
        };
    }
    let destination = output.unwrap_or_else(|| default_output(target, release));
    match compile_native(&compiled, &destination, release) {
        Ok(()) => {
            if !options.quiet {
                eprintln!("built {}", destination.display());
            }
            status::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            status::BUILD_FAILED
        }
    }
}

/// Collects `.sayon` files under a directory in a stable order, skipping
/// build output and hidden or version-control directories.
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
            if !matches!(name.as_str(), "target" | "build" | "node_modules")
                && !name.starts_with('.')
            {
                source_files(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "sayon") {
            out.push(path);
        }
    }
}

/// Runs the test blocks of each file in the interpreter. Each test runs with
/// freshly initialized constants, and its output is shown only if it fails.
/// Without files, a project's `src` and `tests` directories are used, or
/// else `tests` under the current directory.
pub fn test(files: &[String], options: &Options) -> u8 {
    let mut paths: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
    let mut root = None;
    if paths.is_empty() {
        match current_project(options) {
            Ok(Some(project)) => {
                source_files(&project.root.join("src"), &mut paths);
                source_files(&project.root.join("tests"), &mut paths);
                root = Some(project.root);
            }
            Ok(None) => source_files(Path::new("tests"), &mut paths),
            Err(code) => return code,
        }
        if paths.is_empty() {
            eprintln!(
                "error: no tests found; give test files, or add test blocks to files under tests/"
            );
            return status::USAGE;
        }
    }
    let (mut passed, mut failed, mut broken) = (0usize, 0usize, 0usize);
    for path in &paths {
        let compiled = match compile(path, root.as_deref(), options, true) {
            Ok(compiled) => compiled,
            Err(_) => {
                broken += 1;
                continue;
            }
        };
        if compiled.module.tests.is_empty() {
            continue;
        }
        let shown = match &root {
            Some(root) => path.strip_prefix(root).unwrap_or(path).to_path_buf(),
            None => path.clone(),
        };
        println!("{}", shown.display());
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

/// Formats files in place, or with `check_only` reports the files that are
/// not formatted. Files with syntax errors are reported and left unchanged.
pub fn format_files(paths: &[String], check_only: bool) -> u8 {
    let mut files = Vec::new();
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.iter().map(PathBuf::from).collect()
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

const STARTER_MAIN: &str = "\
# The program starts here. Run it with `sayon run`.
greetings = import(\"src/greetings.sayon\")

print(greetings.greeting(\"world\"))
";

const STARTER_MODULE: &str = "\
# Greetings, used by main.sayon. Run the tests with `sayon test`.

function greeting(name: string) -> string
    return \"Hello, {name}!\"

test \"greets by name\"
    assert(greeting(\"Sayon\") == \"Hello, Sayon!\")
";

/// Creates a project directory with a manifest, a small program with a
/// test, and ignore rules for build output.
pub fn init(name: &str) -> u8 {
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        return usage_error(&format!(
            "`{name}` is not a valid project name; use lowercase letters, digits, and underscores, starting with a letter"
        ));
    }
    let root = PathBuf::from(name);
    if root.exists()
        && std::fs::read_dir(&root)
            .map(|mut d| d.next().is_some())
            .unwrap_or(true)
    {
        eprintln!("error: `{name}` already exists and is not empty");
        return status::USAGE;
    }
    let manifest =
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nentry = \"src/main.sayon\"\n");
    let readme = format!(
        "# {name}\n\n```sh\nsayon run      # run the program\nsayon test     # run the tests\nsayon build    # build build/debug/{name}\nsayon fmt      # format the source files\n```\n"
    );
    let files: [(&str, &str); 5] = [
        ("sayon.toml", &manifest),
        ("src/main.sayon", STARTER_MAIN),
        ("src/greetings.sayon", STARTER_MODULE),
        (".gitignore", "/build/\n"),
        ("README.md", &readme),
    ];
    for (relative, content) in files {
        let path = root.join(relative);
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            eprintln!("error: cannot create {}: {error}", parent.display());
            return status::INPUT;
        }
        if let Err(error) = std::fs::write(&path, content) {
            eprintln!("error: cannot write {}: {error}", path.display());
            return status::INPUT;
        }
    }
    println!(
        "Created {name}/ with sayon.toml, src/main.sayon, src/greetings.sayon, .gitignore, and README.md"
    );
    println!();
    println!("Next steps:");
    println!("    cd {name}");
    println!("    sayon run       # prints Hello, world!");
    println!("    sayon test      # runs the test in src/greetings.sayon");
    println!("    sayon build     # builds build/debug/{name}");
    status::SUCCESS
}

/// Removes the project's `build` directory, and nothing else.
pub fn clean(options: &Options) -> u8 {
    let project = match current_project(options) {
        Ok(Some(project)) => project,
        Ok(None) => {
            eprintln!(
                "error: `sayon clean` works in a project; no sayon.toml was found here or above"
            );
            return status::USAGE;
        }
        Err(code) => return code,
    };
    let build = project.root.join("build");
    if !build.exists() {
        println!("nothing to clean");
        return status::SUCCESS;
    }
    match std::fs::remove_dir_all(&build) {
        Ok(()) => {
            println!("removed {}", build.display());
            status::SUCCESS
        }
        Err(error) => {
            eprintln!("error: cannot remove {}: {error}", build.display());
            status::INPUT
        }
    }
}

/// Reports where the standard library, runtime library, and clang are, and
/// whether native builds can work.
pub fn doctor() -> u8 {
    println!(
        "sayon {} ({}-{})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    let mut healthy = true;
    let mut line = |ok: bool, what: &str, detail: String| {
        healthy &= ok;
        println!(
            "  {:<8} {what:<18} {detail}",
            if ok { "ok" } else { "missing" }
        );
    };
    match stdlib_dir() {
        Some(dir) => line(true, "standard library", dir.display().to_string()),
        None => line(
            false,
            "standard library",
            "not found; set SAYON_STDLIB".to_string(),
        ),
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    match sayon_codegen::Toolchain::locate(exe_dir.as_deref()) {
        Ok(toolchain) => {
            line(
                true,
                "runtime library",
                toolchain.runtime.display().to_string(),
            );
            let version = std::process::Command::new(&toolchain.clang)
                .arg("--version")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .and_then(|text| text.lines().next().map(str::to_string))
                .unwrap_or_else(|| "version unknown".to_string());
            line(
                true,
                "clang",
                format!("{} ({version})", toolchain.clang.display()),
            );
        }
        Err(message) => {
            let first = message.lines().next().unwrap_or("").to_string();
            line(false, "native toolchain", first);
        }
    }
    if healthy {
        println!("Everything is in place: sayon run, test, and native builds are available.");
        status::SUCCESS
    } else {
        println!(
            "Some parts are missing; see above. `sayon run` works without the native toolchain."
        );
        status::PROGRAM_FAILED
    }
}
