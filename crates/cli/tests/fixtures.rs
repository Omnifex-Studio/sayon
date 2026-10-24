//! Runs the Sayon programs under `tests/` and compares their behavior with
//! the recorded expectations next to them.
//!
//! - `tests/programs/NAME.sayon` runs with `sayon run`; its output must match
//!   `NAME.stdout`, plus `NAME.stderr` and `NAME.status` when present.
//! - `tests/errors/NAME.sayon` is checked with `sayon check`; the rendered
//!   diagnostics must match `NAME.stderr` and the status must be 2.
//! - `tests/online/NAME.sayon` is like a program, and talks to public
//!   servers; it runs only with `cargo test -- --ignored`.
//!
//! Set `SAYON_BLESS=1` to write the current behavior as the expectation, then
//! review the changes before committing them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
}

fn bless() -> bool {
    std::env::var_os("SAYON_BLESS").is_some()
}

fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sayon"))
        .collect();
    files.sort();
    files
}

fn read_expectation(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.replace("\r\n", "\n"))
}

struct Outcome {
    stdout: String,
    stderr: String,
    status: i32,
}

fn sayon(dir: &Path, args: &[&str]) -> Outcome {
    sayon_with_env(dir, args, &[], None)
}

/// Runs sayon with `input` as its standard input (empty without it).
fn sayon_with_env(dir: &Path, args: &[&str], env: &[(&str, &str)], input: Option<&str>) -> Outcome {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sayon"));
    command
        .args(args)
        .current_dir(dir)
        .env_remove("NO_COLOR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("run sayon");
    let mut stdin = child.stdin.take().expect("standard input");
    if let Some(input) = input {
        stdin.write_all(input.as_bytes()).expect("write input");
    }
    drop(stdin);
    let output = child.wait_with_output().expect("run sayon");
    Outcome {
        stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
        status: output.status.code().unwrap_or(-1),
    }
}

/// Compares one expectation file, or records it when blessing.
fn compare(failures: &mut Vec<String>, name: &str, path: &Path, actual: &str, required: bool) {
    match read_expectation(path) {
        _ if bless() => {
            if required || !actual.is_empty() {
                std::fs::write(path, actual).expect("write expectation");
            } else if path.exists() {
                std::fs::remove_file(path).expect("remove expectation");
            }
        }
        Some(expected) if expected == actual => {}
        Some(expected) => failures.push(format!(
            "{name}: {} differs\n--- expected\n{expected}\n--- actual\n{actual}",
            path.file_name().unwrap().to_string_lossy()
        )),
        None if !required && actual.is_empty() => {}
        None => failures.push(format!(
            "{name}: missing {}; actual output was\n{actual}",
            path.file_name().unwrap().to_string_lossy()
        )),
    }
}

#[test]
fn programs_produce_their_recorded_output() {
    run_programs(&tests_dir().join("programs"));
}

/// The programs under `tests/online`, which need the internet: their HTTPS
/// requests go to public servers. Run them with `cargo test -- --ignored`.
#[test]
#[ignore = "needs the internet"]
fn online_programs_produce_their_recorded_output() {
    let dir = tests_dir().join("online");
    run_programs(&dir);
    if std::env::var_os("SAYON_SKIP_NATIVE").is_none() {
        run_native_programs(&dir);
    }
}

/// Runs each program under `dir` and compares its output, errors, and status
/// with the recorded ones.
fn run_programs(dir: &Path) {
    let mut failures = Vec::new();
    for source in sources(dir) {
        let name = source.file_name().unwrap().to_string_lossy().to_string();
        // A program's `.stdin` file, when it has one, is its input.
        let input = read_expectation(&source.with_extension("stdin"));
        let outcome = sayon_with_env(
            dir,
            &["run", "--color", "never", &name],
            &[],
            input.as_deref(),
        );
        compare(
            &mut failures,
            &name,
            &source.with_extension("stdout"),
            &outcome.stdout,
            true,
        );
        compare(
            &mut failures,
            &name,
            &source.with_extension("stderr"),
            &outcome.stderr,
            false,
        );
        let status = if outcome.status == 0 {
            String::new()
        } else {
            format!("{}\n", outcome.status)
        };
        compare(
            &mut failures,
            &name,
            &source.with_extension("status"),
            &status,
            false,
        );
    }
    assert!(
        failures.is_empty(),
        "{} fixture problem(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Compiles every example natively and checks that it behaves exactly like
/// the interpreter and releases every allocation it made, including on error
/// paths. Needs clang; set `SAYON_SKIP_NATIVE=1` to skip explicitly on a
/// machine without it (the skip is reported, never silent).
#[test]
fn native_programs_match_the_interpreter() {
    if std::env::var_os("SAYON_SKIP_NATIVE").is_some() {
        eprintln!("SKIPPED: native parity checks (SAYON_SKIP_NATIVE is set)");
        return;
    }
    run_native_programs(&tests_dir().join("programs"));
}

/// Builds each program under `dir` natively and compares its behavior with
/// the interpreter's recorded one.
fn run_native_programs(dir: &Path) {
    let mut failures = Vec::new();
    for source in sources(dir) {
        let name = source.file_name().unwrap().to_string_lossy().to_string();
        let outcome = sayon_with_env(
            dir,
            &["run", "--native", "--color", "never", &name],
            &[("SAYON_DEBUG_ALLOC", "1")],
            read_expectation(&source.with_extension("stdin")).as_deref(),
        );
        // Status 4 means the build failed, unless the program itself returns 4.
        if outcome.status == 4 && expected_status(&source) != 4 {
            failures.push(format!(
                "{name}: the native build failed:\n{}",
                outcome.stderr
            ));
            continue;
        }
        compare_with_interpreter(&mut failures, &source, outcome, "native");
    }
    assert!(
        failures.is_empty(),
        "{} native parity problem(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// The WASI host the WebAssembly programs run under, pinned: a program's
/// behavior is checked against this version.
const WASMTIME_VERSION: &str = "wasmtime 50.0.0";

/// Builds every example for WASI and runs it under wasmtime: it must behave
/// exactly like the interpreter and release every allocation it made. A
/// program that needs what WASI lacks, such as sockets, is diagnosed when
/// it is built, and is left out. Needs clang with its WebAssembly linker,
/// the Rust target `wasm32-wasip1` (`rust-toolchain.toml` installs it), and
/// wasmtime on `PATH` or at `SAYON_WASMTIME`; set `SAYON_SKIP_WASI=1` to
/// skip explicitly on a machine without them.
#[test]
fn wasi_programs_match_the_interpreter() {
    if std::env::var_os("SAYON_SKIP_WASI").is_some() {
        eprintln!("SKIPPED: WASI parity checks (SAYON_SKIP_WASI is set)");
        return;
    }
    prepare_wasi_runtime();
    let wasmtime = std::env::var_os("SAYON_WASMTIME").unwrap_or_else(|| "wasmtime".into());
    let version = Command::new(&wasmtime)
        .arg("--version")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_else(|e| panic!("cannot run wasmtime ({e}); set SAYON_SKIP_WASI=1 to skip"));
    assert!(
        version.starts_with(WASMTIME_VERSION),
        "the WASI checks are pinned to {WASMTIME_VERSION}, but found {version}"
    );
    let dir = tests_dir().join("programs");
    let work = std::env::temp_dir().join(format!("sayon-wasi-{}", std::process::id()));
    std::fs::create_dir_all(&work).expect("create work directory");
    let mut failures = Vec::new();
    let mut lacking = Vec::new();
    let mut ran = 0;
    for source in sources(&dir) {
        let name = source.file_name().unwrap().to_string_lossy().to_string();
        let wasm = work.join(source.with_extension("wasm").file_name().unwrap());
        let built = sayon(
            &dir,
            &[
                "build",
                "--color",
                "never",
                "--quiet",
                "--target",
                "wasi",
                "--output",
                &wasm.to_string_lossy(),
                &name,
            ],
        );
        if built.status != 0 {
            if built.stderr.contains("E_CAPABILITY") {
                lacking.push(name);
            } else {
                failures.push(format!("{name}: the WASI build failed:\n{}", built.stderr));
            }
            continue;
        }
        let input = read_expectation(&source.with_extension("stdin"));
        let outcome = run_wasm(&wasmtime, &dir, &wasm, input.as_deref());
        compare_with_interpreter(&mut failures, &source, outcome, "WASI");
        ran += 1;
    }
    let _ = std::fs::remove_dir_all(&work);
    eprintln!(
        "WASI: {ran} programs ran; {} need what WASI lacks: {}",
        lacking.len(),
        lacking.join(", ")
    );
    assert!(ran > 0, "no program ran under WASI");
    assert!(
        failures.is_empty(),
        "{} WASI parity problem(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Runs a WebAssembly program under wasmtime in `dir`, which it may use,
/// with `SAYON_DEBUG_ALLOC` set and `input` as its standard input.
fn run_wasm(wasmtime: &std::ffi::OsStr, dir: &Path, wasm: &Path, input: Option<&str>) -> Outcome {
    let mut child = Command::new(wasmtime)
        .args(["run", "--dir", ".", "--env", "SAYON_DEBUG_ALLOC=1"])
        .arg(wasm)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run wasmtime");
    let mut stdin = child.stdin.take().expect("standard input");
    if let Some(input) = input {
        stdin.write_all(input.as_bytes()).expect("write input");
    }
    drop(stdin);
    let output = child.wait_with_output().expect("run wasmtime");
    Outcome {
        stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
        status: output.status.code().unwrap_or(-1),
    }
}

/// Builds the runtime for WASI and puts it, with WASI's C library from the
/// Rust toolchain, where `sayon` looks for another target's libraries:
/// `lib/wasm32-wasip1` beside the directory of the `sayon` executable. The
/// runtime builds in a directory of its own, so this cargo never waits for
/// the one running the tests.
fn prepare_wasi_runtime() {
    const TRIPLE: &str = "wasm32-wasip1";
    let root = tests_dir().join("..");
    let target_dir = Path::new(env!("CARGO_BIN_EXE_sayon"))
        .parent()
        .and_then(Path::parent)
        .expect("the target directory");
    let build_dir = target_dir.join("wasi-runtime");
    let cargo = option_env!("CARGO").unwrap_or("cargo");
    let built = Command::new(cargo)
        .args([
            "build",
            "--quiet",
            "-p",
            "sayon-runtime",
            "--target",
            TRIPLE,
        ])
        .arg("--target-dir")
        .arg(&build_dir)
        .current_dir(&root)
        .status()
        .expect("run cargo");
    assert!(built.success(), "building the runtime for {TRIPLE} failed");
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(&root)
        .output()
        .expect("run rustc");
    let sysroot = PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim());
    let c_library = sysroot
        .join("lib")
        .join("rustlib")
        .join(TRIPLE)
        .join("lib")
        .join("self-contained");
    let lib = target_dir.join("lib").join(TRIPLE);
    std::fs::create_dir_all(&lib).expect("create the library directory");
    let runtime = build_dir
        .join(TRIPLE)
        .join("debug")
        .join("libsayon_runtime.a");
    std::fs::copy(&runtime, lib.join("libsayon_runtime.a"))
        .unwrap_or_else(|e| panic!("copy {}: {e}", runtime.display()));
    for name in ["crt1-command.o", "libc.a"] {
        std::fs::copy(c_library.join(name), lib.join(name))
            .unwrap_or_else(|e| panic!("copy {name} from {}: {e}", c_library.display()));
    }
}

/// The status a program's recorded behavior ends with.
fn expected_status(source: &Path) -> i32 {
    read_expectation(&source.with_extension("status"))
        .map(|text| text.trim().parse().expect("status"))
        .unwrap_or(0)
}

/// Compares how a compiled program behaved, run with `SAYON_DEBUG_ALLOC`,
/// with the interpreter's recorded behavior, and checks that it released
/// every allocation it made. `mode` names how it was compiled.
fn compare_with_interpreter(
    failures: &mut Vec<String>,
    source: &Path,
    mut outcome: Outcome,
    mode: &str,
) {
    let name = source.file_name().unwrap().to_string_lossy().to_string();
    const REPORT: &str = "sayon: allocations live at exit: ";
    // A program stopped by a fatal error releases nothing, and reports
    // nothing.
    let fatal = expected_status(source) == 3;
    let mut stderr_lines: Vec<&str> = outcome.stderr.lines().collect();
    match stderr_lines
        .iter()
        .position(|line| line.starts_with(REPORT))
    {
        Some(index) => {
            let live = &stderr_lines[index][REPORT.len()..];
            if live != "0" {
                failures.push(format!(
                    "{name}: {live} allocations were never released ({mode})"
                ));
            }
            stderr_lines.remove(index);
        }
        None if fatal => {}
        None => failures.push(format!(
            "{name}: no allocation report ({mode})\n{}",
            outcome.stderr
        )),
    }
    outcome.stderr = stderr_lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect();
    let expected_stdout = read_expectation(&source.with_extension("stdout")).unwrap_or_default();
    let expected_stderr = read_expectation(&source.with_extension("stderr")).unwrap_or_default();
    let expected_status = expected_status(source);
    if outcome.stdout != expected_stdout {
        failures.push(format!(
            "{name}: {mode} output differs\n--- interpreter\n{expected_stdout}\n--- {mode}\n{}",
            outcome.stdout
        ));
    }
    if outcome.stderr != expected_stderr {
        failures.push(format!(
            "{name}: {mode} errors differ\n--- interpreter\n{expected_stderr}\n--- {mode}\n{}",
            outcome.stderr
        ));
    }
    if outcome.status != expected_status {
        failures.push(format!(
            "{name}: {mode} status {} differs from {expected_status}",
            outcome.status
        ));
    }
}

/// Runs `sayon test` on each file under `tests/test_runner` and compares
/// the report and exit status with `NAME.stdout` and `NAME.status`.
#[test]
fn test_runner_reports_results() {
    let dir = tests_dir().join("test_runner");
    let mut failures = Vec::new();
    for source in sources(&dir) {
        let name = source.file_name().unwrap().to_string_lossy().to_string();
        let outcome = sayon(&dir, &["test", "--color", "never", &name]);
        compare(
            &mut failures,
            &name,
            &source.with_extension("stdout"),
            &outcome.stdout,
            true,
        );
        compare(
            &mut failures,
            &name,
            &source.with_extension("stderr"),
            &outcome.stderr,
            false,
        );
        let status = if outcome.status == 0 {
            String::new()
        } else {
            format!(
                "{}
",
                outcome.status
            )
        };
        compare(
            &mut failures,
            &name,
            &source.with_extension("status"),
            &status,
            false,
        );
    }
    assert!(
        failures.is_empty(),
        "{} fixture problem(s):

{}",
        failures.len(),
        failures.join(
            "

"
        )
    );
}

/// The Sayon sources in this repository are formatted, so formatting them
/// again changes nothing.
#[test]
fn repository_sources_are_formatted() {
    let root = tests_dir().join("..");
    let outcome = sayon(
        &root,
        &[
            "fmt",
            "--check",
            "tests/programs",
            "tests/online",
            "tests/lint",
            "tests/test_runner",
            "stdlib",
            "examples",
        ],
    );
    assert_eq!(outcome.status, 0, "{}{}", outcome.stdout, outcome.stderr);
}

#[test]
fn lint_reports_its_recorded_warnings() {
    let dir = tests_dir().join("lint");
    let mut failures = Vec::new();
    for source in sources(&dir) {
        let name = source.file_name().unwrap().to_string_lossy().to_string();
        let outcome = sayon(&dir, &["lint", "--color", "never", &name]);
        if outcome.status != 0 {
            failures.push(format!("{name}: expected status 0, got {}", outcome.status));
        }
        let denied = sayon(
            &dir,
            &["lint", "--color", "never", "--deny", "warnings", &name],
        );
        if denied.status != 2 {
            failures.push(format!(
                "{name}: expected status 2 with --deny warnings, got {}",
                denied.status
            ));
        }
        compare(
            &mut failures,
            &name,
            &source.with_extension("stderr"),
            &outcome.stderr,
            true,
        );
    }
    assert!(
        failures.is_empty(),
        "{} fixture problem(s):

{}",
        failures.len(),
        failures.join(
            "

"
        )
    );
}

#[test]
fn invalid_programs_report_their_recorded_diagnostics() {
    let dir = tests_dir().join("errors");
    let mut failures = Vec::new();
    for source in sources(&dir) {
        let name = source.file_name().unwrap().to_string_lossy().to_string();
        // A first line `# check: --target web` adds its words to the command.
        let text = std::fs::read_to_string(&source).unwrap();
        let extra: Vec<&str> = text
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("# check:"))
            .map(|words| words.split_whitespace().collect())
            .unwrap_or_default();
        let mut args = vec!["check", "--color", "never"];
        args.extend(extra);
        args.push(&name);
        let outcome = sayon(&dir, &args);
        if outcome.status != 2 {
            failures.push(format!("{name}: expected status 2, got {}", outcome.status));
        }
        compare(
            &mut failures,
            &name,
            &source.with_extension("stderr"),
            &outcome.stderr,
            true,
        );
    }
    assert!(
        failures.is_empty(),
        "{} fixture problem(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
