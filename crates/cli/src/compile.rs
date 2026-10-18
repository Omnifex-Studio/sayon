//! Finding what to compile, compiling it, and reporting diagnostics.

use crate::{status, usage_error};
use sayon_driver::manifest::Manifest;
use sayon_syntax::diagnostic::render;
use sayon_syntax::{Diagnostics, SourceMap};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

pub struct Options {
    pub color: bool,
    pub quiet: bool,
}

pub fn color_default() -> bool {
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

pub struct Project {
    pub root: PathBuf,
    pub manifest: Manifest,
}

/// The program a command works on: a file named on the command line, or the
/// entry file of the project around the current directory.
pub struct Target {
    pub entry: PathBuf,
    /// The directory imports resolve from, for a project.
    pub root: Option<PathBuf>,
    pub project: Option<Project>,
}

impl Target {
    pub fn resolve(file: Option<&String>, command: &str, options: &Options) -> Result<Target, u8> {
        if let Some(file) = file {
            return Ok(Target {
                entry: PathBuf::from(file),
                root: None,
                project: None,
            });
        }
        let project = match current_project(options)? {
            Some(project) => project,
            None => {
                return Err(usage_error(&format!(
                    "`sayon {command}` needs a source file or a project; give a file, run it in a directory \
                     with sayon.toml, or create a project with `sayon init`"
                )));
            }
        };
        let entry = project.root.join(&project.manifest.entry);
        Ok(Target {
            entry,
            root: Some(project.root.clone()),
            project: Some(project),
        })
    }
}

/// The project around the current directory, if there is one. A manifest
/// with errors is reported and stops the command.
pub fn current_project(options: &Options) -> Result<Option<Project>, u8> {
    let cwd = std::env::current_dir().map_err(|_| status::INPUT)?;
    let Some(root) = sayon_driver::find_project(&cwd) else {
        return Ok(None);
    };
    let mut sources = SourceMap::new();
    match sayon_driver::read_manifest(&root, &mut sources) {
        Ok(manifest) => Ok(Some(Project { root, manifest })),
        Err(errors) => {
            let mut diagnostics = Diagnostics::new();
            diagnostics.extend(errors);
            print_diagnostics(&diagnostics, &sources, options, "the manifest has errors");
            Err(status::COMPILE_FAILED)
        }
    }
}

pub fn print_diagnostics(
    diagnostics: &Diagnostics,
    sources: &SourceMap,
    options: &Options,
    summary: &str,
) {
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
        let _ = writeln!(stderr, "{errors} {noun} found; {summary}");
        if let Some(first) = diagnostics.iter().find(|d| d.is_error()) {
            let _ = writeln!(
                stderr,
                "run `sayon explain {}` to learn more about an error",
                first.code
            );
        }
    }
}

pub struct Compiled {
    pub sources: SourceMap,
    pub module: sayon_ir::Module,
}

/// The standard library directory: `SAYON_STDLIB`, then `stdlib` next to
/// the executable or up to two levels above it (an installation's `bin`
/// directory, or a development build in `target/<profile>`).
pub fn stdlib_dir() -> Option<PathBuf> {
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

/// Prints a problem with the dependencies like a diagnostic.
pub fn print_package_error(error: &sayon_driver::packages::PackageError, options: &Options) {
    let label = if options.color {
        "\x1b[1;31merror\x1b[0m"
    } else {
        "error"
    };
    eprintln!("{label}: {}", error.message);
    if let Some(help) = &error.help {
        eprintln!("  = help: {help}");
    }
}

/// The project a file belongs to: the nearest directory above it with a
/// manifest. A file of a project imports from the project's root.
fn project_root(entry: &Path, root: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = root {
        return Some(root.to_path_buf());
    }
    let absolute = std::path::absolute(entry).ok()?;
    sayon_driver::find_project(absolute.parent()?)
}

/// The driver's options for a program: the project's root and the packages
/// its lock provides.
fn driver_options(
    entry: &Path,
    root: Option<&Path>,
    options: &Options,
    tests: bool,
    lint: bool,
) -> Result<sayon_driver::Options, u8> {
    let root = project_root(entry, root);
    let mut packages = sayon_driver::packages::Packages::default();
    if let Some(root) = &root {
        let mut sources = SourceMap::new();
        match sayon_driver::read_manifest(root, &mut sources) {
            Ok(manifest) => match sayon_driver::packages::load(root, &manifest, tests) {
                Ok(found) => packages = found,
                Err(error) => {
                    print_package_error(&error, options);
                    return Err(status::COMPILE_FAILED);
                }
            },
            Err(errors) => {
                let mut diagnostics = Diagnostics::new();
                diagnostics.extend(errors);
                print_diagnostics(&diagnostics, &sources, options, "the manifest has errors");
                return Err(status::COMPILE_FAILED);
            }
        }
    }
    Ok(sayon_driver::Options {
        stdlib: stdlib_dir(),
        tests,
        root,
        lint,
        packages,
    })
}

/// Loads, checks, and lowers a program, printing any diagnostics.
pub fn compile(
    entry: &Path,
    root: Option<&Path>,
    options: &Options,
    tests: bool,
) -> Result<Compiled, u8> {
    let driver_options = driver_options(entry, root, options, tests, false)?;
    let output = match sayon_driver::compile(entry, &driver_options) {
        Ok(output) => output,
        Err(sayon_driver::EntryError(message)) => {
            eprintln!("error: {message}");
            return Err(status::INPUT);
        }
    };
    print_diagnostics(
        &output.diagnostics,
        &output.sources,
        options,
        "nothing was run",
    );
    match output.module {
        Some(module) if !output.diagnostics.has_errors() => Ok(Compiled {
            sources: output.sources,
            module,
        }),
        _ => Err(status::COMPILE_FAILED),
    }
}

/// Checks a program with the lint warnings, printing the diagnostics.
/// Returns the error and warning counts, or the status for a missing entry.
pub fn lint(entry: &Path, root: Option<&Path>, options: &Options) -> Result<(usize, usize), u8> {
    let driver_options = driver_options(entry, root, options, false, true)?;
    let output = match sayon_driver::compile(entry, &driver_options) {
        Ok(output) => output,
        Err(sayon_driver::EntryError(message)) => {
            eprintln!("error: {message}");
            return Err(status::INPUT);
        }
    };
    print_diagnostics(
        &output.diagnostics,
        &output.sources,
        options,
        "nothing was linted",
    );
    let errors = output.diagnostics.error_count();
    let warnings = output.diagnostics.iter().filter(|d| !d.is_error()).count();
    Ok((errors, warnings))
}

/// Where native build intermediates go: a fresh directory under the system
/// temporary directory, removed afterwards.
pub struct WorkDir(pub PathBuf);

impl WorkDir {
    pub fn create() -> Result<WorkDir, String> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("sayon-build-{}-{stamp}", std::process::id()));
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

/// Compiles a checked program to a native executable at `output`.
pub fn compile_native(compiled: &Compiled, output: &Path, release: bool) -> Result<(), String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let toolchain = sayon_codegen::Toolchain::locate(exe_dir.as_deref())?;
    let work = WorkDir::create()?;
    let ir_path = work.0.join("program.ll");
    let ir = sayon_codegen::emit_llvm(&compiled.module, &compiled.sources);
    std::fs::write(&ir_path, ir).map_err(|e| format!("cannot write {}: {e}", ir_path.display()))?;
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let libraries = sayon_codegen::foreign_libraries(&compiled.module);
    sayon_codegen::link(&toolchain, &ir_path, output, release, &libraries)
}
