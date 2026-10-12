//! Runs the Sayon programs under `tests/` and compares their behavior with
//! the recorded expectations next to them.
//!
//! - `tests/programs/NAME.sayon` runs with `sayon run`; its output must match
//!   `NAME.stdout`, plus `NAME.stderr` and `NAME.status` when present.
//! - `tests/errors/NAME.sayon` is checked with `sayon check`; the rendered
//!   diagnostics must match `NAME.stderr` and the status must be 2.
//!
//! Set `SAYON_BLESS=1` to write the current behavior as the expectation, then
//! review the changes before committing them.

use std::path::{Path, PathBuf};
use std::process::Command;

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
    let output = Command::new(env!("CARGO_BIN_EXE_sayon"))
        .args(args)
        .current_dir(dir)
        .env_remove("NO_COLOR")
        .output()
        .expect("run sayon");
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
    let dir = tests_dir().join("programs");
    let mut failures = Vec::new();
    for source in sources(&dir) {
        let name = source.file_name().unwrap().to_string_lossy().to_string();
        let outcome = sayon(&dir, &["run", "--color", "never", &name]);
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

#[test]
fn invalid_programs_report_their_recorded_diagnostics() {
    let dir = tests_dir().join("errors");
    let mut failures = Vec::new();
    for source in sources(&dir) {
        let name = source.file_name().unwrap().to_string_lossy().to_string();
        let outcome = sayon(&dir, &["check", "--color", "never", &name]);
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
