//! The commands.

use crate::compile::{
    Options, Source, WorkDir, compile, compile_android_app, compile_native, current_project,
    stdlib_dir,
};
use crate::{status, usage_error};
use sayon_syntax::SourceMap;
use sayon_syntax::diagnostic::render;
use sayon_target::Target;
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
        sayon_interp::RunError::TaskFailures(failures) => {
            let label = if options.color {
                "[1;31merror[0m"
            } else {
                "error"
            };
            for error in failures {
                eprintln!("{label}: a task failed: {}: {}", error.code, error.message);
                if let Some(location) = error.location {
                    eprintln!("  --> {location}");
                }
            }
            status::PROGRAM_FAILED
        }
        sayon_interp::RunError::Fatal(message) => {
            eprintln!("fatal error: {message}");
            status::FATAL
        }
    }
}

/// `sayon check`: checks the program for each target, which selects its
/// platform branches and finds the capabilities it lacks.
pub fn check(source: &Source, options: &Options, targets: &[&'static Target]) -> u8 {
    let named = targets.len() > 1 || targets.iter().any(|t| !t.is_host());
    let mut result = status::SUCCESS;
    for target in targets {
        match compile(
            &source.entry,
            source.root.as_deref(),
            options,
            false,
            target,
        ) {
            Ok(_) => {
                if !options.quiet {
                    let suffix = if named {
                        format!(" for {target}")
                    } else {
                        String::new()
                    };
                    eprintln!("{}: no problems found{suffix}", source.entry.display());
                }
            }
            Err(code) => result = result.max(code),
        }
    }
    result
}

/// `sayon install`: resolves the project's dependencies, fetches Git sources,
/// and writes sayon.lock.
pub fn install(options: &Options, locked: bool, offline: bool) -> u8 {
    let project = match current_project(options) {
        Ok(Some(project)) => project,
        Ok(None) => {
            return usage_error(
                "`sayon install` works on a project; run it in a directory with sayon.toml",
            );
        }
        Err(code) => return code,
    };
    let fetch = if offline {
        sayon_driver::packages::Fetch::Offline
    } else {
        sayon_driver::packages::Fetch::Allowed
    };
    match sayon_driver::packages::install(&project.root, &project.manifest, locked, fetch) {
        Ok(installed) => {
            if !options.quiet {
                let noun = if installed.packages == 1 {
                    "package"
                } else {
                    "packages"
                };
                let lock = if installed.changed {
                    "wrote sayon.lock"
                } else {
                    "sayon.lock is up to date"
                };
                eprintln!("{} {noun}; {lock}", installed.packages);
            }
            status::SUCCESS
        }
        Err(error) => {
            crate::compile::print_package_error(&error, options);
            status::COMPILE_FAILED
        }
    }
}

/// `sayon lint`: the compiler's checks and the lint warnings. With
/// `deny_warnings`, any warning fails the command.
pub fn lint(
    source: &Source,
    options: &Options,
    deny_warnings: bool,
    target: &'static Target,
) -> u8 {
    match crate::compile::lint(&source.entry, source.root.as_deref(), options, target) {
        Err(code) => code,
        Ok((errors, _)) if errors > 0 => status::COMPILE_FAILED,
        Ok((_, 0)) => {
            if !options.quiet {
                eprintln!("{}: no problems found", source.entry.display());
            }
            status::SUCCESS
        }
        Ok((_, warnings)) => {
            let noun = if warnings == 1 { "warning" } else { "warnings" };
            if deny_warnings {
                eprintln!("{warnings} {noun} found; `--deny warnings` treats them as errors");
                status::COMPILE_FAILED
            } else {
                if !options.quiet {
                    eprintln!("{warnings} {noun} found");
                }
                status::SUCCESS
            }
        }
    }
}

pub fn run(source: &Source, options: &Options, program_args: &[String]) -> u8 {
    let compiled = match compile(
        &source.entry,
        source.root.as_deref(),
        options,
        false,
        sayon_target::host(),
    ) {
        Ok(compiled) => compiled,
        Err(code) => return code,
    };
    // Not locked for the run: tasks on other threads write too.
    let mut out = std::io::BufWriter::new(std::io::stdout());
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
    source: &Source,
    options: &Options,
    release: bool,
    program_args: &[String],
) -> u8 {
    let compiled = match compile(
        &source.entry,
        source.root.as_deref(),
        options,
        false,
        sayon_target::host(),
    ) {
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
    if let Err(message) = compile_native(
        &compiled,
        &executable,
        release,
        sayon_codegen::Product::Executable,
    ) {
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

/// Where `sayon build` writes for a target: next to a standalone file, or
/// under the project's `build/debug` or `build/release` directory. A target
/// other than this computer's has a name of its own there: `hello-web.wasm`,
/// or `build/debug/web/hello.wasm`.
/// The name of the program: its project's, or its file's.
fn program_name(source: &Source) -> String {
    match &source.project {
        Some(project) => project.manifest.name.clone(),
        None => source
            .entry
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "program".into()),
    }
}

fn default_output(source: &Source, release: bool, target: &Target) -> PathBuf {
    let suffix = target.executable_suffix();
    match &source.project {
        Some(project) => {
            let profile = if release { "release" } else { "debug" };
            let mut directory = project.root.join("build").join(profile);
            if !target.is_host() {
                directory = directory.join(target.name);
            }
            directory.join(format!("{}{suffix}", project.manifest.name))
        }
        None => {
            let stem = source
                .entry
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "program".into());
            let name = if target.is_host() {
                format!("{stem}{suffix}")
            } else {
                format!("{stem}-{}{suffix}", target.name)
            };
            source.entry.with_file_name(name)
        }
    }
}

/// `sayon build`: an executable for each target, or for Android an app's
/// package, unless `executable` asks for a program to run from a shell.
#[allow(clippy::too_many_arguments)]
pub fn build(
    source: &Source,
    options: &Options,
    output: Option<PathBuf>,
    release: bool,
    emit_llvm: bool,
    executable: bool,
    targets: &[&'static Target],
) -> u8 {
    if output.is_some() && targets.len() > 1 {
        return usage_error("`--output` names one file, so it builds one target at a time");
    }
    let mut result = status::SUCCESS;
    for target in targets {
        let built = build_for(
            source,
            options,
            output.clone(),
            release,
            emit_llvm,
            executable,
            target,
        );
        result = result.max(built);
    }
    result
}

fn build_for(
    source: &Source,
    options: &Options,
    output: Option<PathBuf>,
    release: bool,
    emit_llvm: bool,
    executable: bool,
    target: &'static Target,
) -> u8 {
    let compiled = match compile(
        &source.entry,
        source.root.as_deref(),
        options,
        false,
        target,
    ) {
        Ok(compiled) => compiled,
        Err(code) => return code,
    };
    if !target.builds_here() {
        let reason = if target.state >= sayon_target::State::Buildable {
            format!("so far, programs for {target} are built on {target} itself")
        } else {
            format!(
                "building programs for {target} is not available yet; it is planned for milestone {}",
                target.planned
            )
        };
        eprintln!("error: {reason}");
        eprintln!("  = note: checking found no problems in the program for {target}");
        eprintln!("  = help: `sayon target list` shows the targets this computer builds for");
        return status::BUILD_FAILED;
    }
    if emit_llvm {
        let destination =
            output.unwrap_or_else(|| default_output(source, release, target).with_extension("ll"));
        // Coroutines take the form the clang that would compile them reads.
        let dialect = match sayon_codegen::clang() {
            Some(clang) if sayon_codegen::uses_coroutines(&compiled.module) => {
                sayon_codegen::dialect(&clang)
            }
            _ => sayon_codegen::Dialect::default(),
        };
        let ir = sayon_codegen::emit_llvm(&compiled.module, &compiled.sources, dialect);
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
    let android_app = target.platform == "android" && !executable;
    let destination = output.unwrap_or_else(|| {
        let path = default_output(source, release, target);
        // A program for `adb shell` has no suffix.
        if target.platform == "android" && executable {
            path.with_extension("")
        } else {
            path
        }
    });
    let built = if android_app {
        compile_android_app(&compiled, &destination, release, &program_name(source))
    } else {
        compile_native(
            &compiled,
            &destination,
            release,
            sayon_codegen::Product::Executable,
        )
    };
    match built {
        Ok(()) => {
            if target.platform == "web"
                && let Err(message) = write_web_files(&destination)
            {
                eprintln!("error: {message}");
                return status::BUILD_FAILED;
            }
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

/// Beside a module built for web pages, the glue that runs it (`.js`) and a
/// page that shows its output (`.html`).
fn write_web_files(module: &Path) -> Result<(), String> {
    let name = |path: &Path| {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let glue = module.with_extension("js");
    let page = module.with_extension("html");
    let title = module
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    std::fs::write(&glue, sayon_codegen::web::glue(&name(module)))
        .map_err(|e| format!("cannot write {}: {e}", glue.display()))?;
    std::fs::write(&page, sayon_codegen::web::page(&title, &name(&glue)))
        .map_err(|e| format!("cannot write {}: {e}", page.display()))
}

/// `sayon target list`: every target, how far its support has come, and
/// what it lacks.
pub fn target_list() -> u8 {
    println!("Targets (* is this computer):");
    let width = sayon_target::TARGETS
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(0)
        + 2;
    let triple_width = sayon_target::TARGETS
        .iter()
        .map(|t| t.triple.len())
        .max()
        .unwrap_or(0);
    for target in sayon_target::TARGETS {
        let mark = if target.is_host() { "*" } else { " " };
        let state = if target.state >= sayon_target::State::Buildable {
            target.state.name().to_string()
        } else {
            format!(
                "{}; building planned for {}",
                target.state.name(),
                target.planned
            )
        };
        println!(
            "  {mark} {:<width$}{:<triple_width$}  {state}",
            target.name, target.triple
        );
        let lacks = target.lacks();
        if !lacks.is_empty() {
            let names: Vec<&str> = lacks.iter().map(|c| c.name()).collect();
            println!("    {:<width$}lacks {}", "", names.join(", "));
        }
    }
    let aliases: Vec<String> = sayon_target::ALIASES
        .iter()
        .map(|(alias, name)| format!("{alias} = {name}"))
        .collect();
    println!(
        "Short names: native = {}, {}",
        sayon_target::host(),
        aliases.join(", ")
    );
    println!(
        "Capability manifest {} (`sayon target manifest` prints it).",
        sayon_target::CAPABILITY_MANIFEST
    );
    println!("`sayon check --target <target>` checks a program for any target;");
    println!("`sayon build --target <target>` builds for the ones this computer builds for.");
    status::SUCCESS
}

/// `sayon target setup <target>`: what building for a target needs.
pub fn target_setup(name: &str) -> u8 {
    let target = match sayon_target::resolve(name) {
        Ok(target) => target,
        Err(message) => return usage_error(&message),
    };
    println!("{target} ({}, {})", target.triple, target.state.name());
    if target.is_host() {
        println!(
            "This computer's own target: native builds need clang, which `sayon doctor` checks."
        );
    } else if target.platform == "wasi" && target.built_anywhere() {
        println!(
            "Built on any computer with clang and its WebAssembly linker (wasm-ld), and the runtime \
             for {}; the programs run under a WASI host such as wasmtime, with the folders they may \
             use given to them (`wasmtime run --dir . program.wasm`).",
            target.triple
        );
    } else if target.platform == "web" && target.built_anywhere() {
        println!(
            "Built on any computer with clang and its WebAssembly linker (wasm-ld), and the runtime \
             for {}: a module, the JavaScript glue that runs it (.js), and a page that shows its \
             output (.html). Serve the folder over HTTP and open the page; browsers load modules \
             only from a server.",
            target.triple
        );
    } else if target.platform == "android" && target.built_anywhere() {
        android_setup(target);
    } else if target.state >= sayon_target::State::Buildable {
        println!("So far, programs for {target} are built on {target} itself.");
    } else {
        println!(
            "Building programs for {target} is planned for milestone {}; `sayon check --target {target}` checks them now.",
            target.planned
        );
    }
    let provides: Vec<&str> = target.capabilities.iter().map(|c| c.name()).collect();
    println!("Provides: {}", provides.join(", "));
    let lacks: Vec<&str> = target.lacks().iter().map(|c| c.name()).collect();
    if !lacks.is_empty() {
        println!("Lacks: {}", lacks.join(", "));
    }
    status::SUCCESS
}

/// What building for Android needs, and which of it this computer has.
fn android_setup(target: &'static Target) {
    use sayon_codegen::android;
    println!(
        "Built on any computer with the Android SDK (ANDROID_HOME, or where Android Studio puts \
         it) and its NDK, build tools, and platform android-{}; a JDK, 17 or newer (JAVA_HOME), \
         for signing; and the runtime for {}.",
        android::TARGET_API,
        target.triple,
    );
    println!(
        "`sayon build` writes an app's package (.apk), signed with Android's debug key, for \
         `adb install`; with `--executable`, a program to run from `adb shell` instead. Apps run \
         on Android 8.0 (API {}) and newer.",
        android::MIN_API,
    );
    println!("The SDK's own manager installs what is missing, as Sayon's tests use it:");
    println!(
        "  sdkmanager \"ndk;{}\" \"build-tools;{}\" \"platforms;android-{}\"",
        android::NDK_VERSION,
        android::BUILD_TOOLS_VERSION,
        android::TARGET_API,
    );
    let show = |ok: bool, what: &str, detail: String| {
        println!(
            "  {:<8} {what:<12} {detail}",
            if ok { "ok" } else { "missing" }
        );
    };
    match android::Sdk::locate(target) {
        Ok(sdk) => {
            show(true, "SDK", sdk.root.display().to_string());
            show(true, "NDK", sdk.ndk.display().to_string());
            match sdk.packaging(target) {
                Ok(tools) => {
                    show(
                        true,
                        "build tools",
                        tools
                            .aapt2
                            .parent()
                            .unwrap_or(&tools.aapt2)
                            .display()
                            .to_string(),
                    );
                    show(true, "platform", tools.platform.display().to_string());
                    show(true, "Java", tools.java.display().to_string());
                }
                Err(message) => show(false, "packaging", message),
            }
        }
        Err(message) => show(false, "SDK", message),
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
        let compiled = match compile(path, root.as_deref(), options, true, sayon_target::host()) {
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
                        sayon_interp::RunError::TaskFailures(failures) => {
                            for error in failures {
                                println!(
                                    "          a task failed: {}: {}",
                                    error.code, error.message
                                );
                                if let Some(location) = error.location {
                                    println!("            --> {location}");
                                }
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
    println!(
        "  Unicode {}",
        sayon_runtime::unicode_tables::UNICODE_VERSION
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
    match sayon_codegen::Toolchain::locate(exe_dir.as_deref(), sayon_target::host()) {
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
    // Building for WebAssembly and Android is optional: without it, the
    // rest works.
    for (name, label) in [
        ("wasi", "WASI runtime"),
        ("web", "web runtime"),
        ("android-x64", "Android x64"),
        ("android-arm64", "Android ARM64"),
    ] {
        let Some(target) = sayon_target::find(name) else {
            continue;
        };
        let (state, detail) = match sayon_codegen::Toolchain::locate(exe_dir.as_deref(), target) {
            Ok(toolchain) => (
                "ok",
                toolchain
                    .runtime
                    .parent()
                    .unwrap_or(&toolchain.runtime)
                    .display()
                    .to_string(),
            ),
            Err(message) => ("absent", message.lines().next().unwrap_or("").to_string()),
        };
        println!("  {state:<8} {label:<18} {detail}");
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
