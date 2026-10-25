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

#[derive(Clone)]
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
    prepare_runtime("wasm32-wasip1", WASI_C_LIBRARY);
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
    let mut built = build_programs(&dir, "wasi", "wasmtime");
    let mut failures = std::mem::take(&mut built.failures);
    for (source, wasm) in &built.programs {
        let input = read_expectation(&source.with_extension("stdin"));
        let outcome = run_wasm(&wasmtime, &dir, wasm, input.as_deref());
        compare_with_interpreter(&mut failures, source, outcome, "WASI");
    }
    built.finish("WASI", failures);
}

/// The Node that runs web programs in the tests, pinned like wasmtime.
const NODE_VERSION: &str = "v24.22.1";

/// Builds every example for web pages and runs it in Node through its
/// generated glue (`tests/web/node.mjs`): it must behave exactly like the
/// interpreter and release every allocation it made. Passing here does not
/// show that browsers run it the same; the browser check does. Needs clang
/// with its WebAssembly linker, the Rust target `wasm32-unknown-unknown`,
/// and Node on `PATH` or at `SAYON_NODE`; set `SAYON_SKIP_WEB=1` to skip.
#[test]
fn web_programs_match_the_interpreter_in_node() {
    if std::env::var_os("SAYON_SKIP_WEB").is_some() {
        eprintln!("SKIPPED: web parity checks in Node (SAYON_SKIP_WEB is set)");
        return;
    }
    prepare_runtime("wasm32-unknown-unknown", &[]);
    let node = std::env::var_os("SAYON_NODE").unwrap_or_else(|| "node".into());
    let version = Command::new(&node)
        .arg("--version")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|e| panic!("cannot run node ({e}); set SAYON_SKIP_WEB=1 to skip"));
    assert_eq!(
        version, NODE_VERSION,
        "the web checks are pinned to Node {NODE_VERSION}"
    );
    let runner = tests_dir().join("web").join("node.mjs");
    let dir = tests_dir().join("programs");
    let mut built = build_programs(&dir, "web", "node");
    let mut failures = std::mem::take(&mut built.failures);
    for (source, wasm) in &built.programs {
        let output = Command::new(&node)
            .arg(&runner)
            .arg(wasm.with_extension("js"))
            .env("SAYON_DEBUG_ALLOC", "1")
            .current_dir(&dir)
            .stdin(Stdio::null())
            .output()
            .expect("run node");
        let outcome = Outcome {
            stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
            status: output.status.code().unwrap_or(-1),
        };
        compare_with_interpreter(&mut failures, source, outcome, "web in Node");
    }
    built.finish("Web in Node", failures);
}

/// Builds every example for web pages and runs each in actual browsers:
/// Edge, Chrome, and Firefox where installed (or those `SAYON_BROWSERS`
/// lists, separated by `;`), headless, through an HTTP server on this
/// computer, as a page loads them: `tests/web/runner.html` runs each program
/// through its glue and posts what it wrote back. Each must behave exactly
/// like the interpreter and release every allocation it made. Set
/// `SAYON_SKIP_BROWSERS=1` to skip; with no browser found, the check says
/// it skipped.
#[test]
fn web_programs_match_the_interpreter_in_browsers() {
    if std::env::var_os("SAYON_SKIP_BROWSERS").is_some() {
        eprintln!("SKIPPED: web parity checks in browsers (SAYON_SKIP_BROWSERS is set)");
        return;
    }
    let browsers = browsers();
    if browsers.is_empty() {
        eprintln!("SKIPPED: web parity checks in browsers (no browser found)");
        return;
    }
    prepare_runtime("wasm32-unknown-unknown", &[]);
    let dir = tests_dir().join("programs");
    let mut built = build_programs(&dir, "web", "browsers");
    let mut failures = std::mem::take(&mut built.failures);
    std::fs::copy(
        tests_dir().join("web").join("runner.html"),
        built.work.join("runner.html"),
    )
    .expect("copy the runner page");
    let names: Vec<String> = built
        .programs
        .iter()
        .map(|(source, _)| source.file_stem().unwrap().to_string_lossy().to_string())
        .collect();
    std::fs::write(built.work.join("programs.txt"), names.join("\n")).expect("list the programs");
    for browser in &browsers {
        let results = run_in_browser(browser, &built.work, names.len());
        for (source, _) in &built.programs {
            let name = source.file_stem().unwrap().to_string_lossy().to_string();
            match results.iter().find(|(n, _)| *n == name) {
                Some((_, outcome)) => compare_with_interpreter(
                    &mut failures,
                    source,
                    outcome.clone(),
                    &format!("web in {}", browser.name),
                ),
                None => failures.push(format!("{name}: {} gave no result", browser.name)),
            }
        }
    }
    let used: Vec<&str> = browsers.iter().map(|b| b.name.as_str()).collect();
    built.finish(&format!("Web in {}", used.join(", ")), failures);
}

/// A browser to run web programs in.
struct Browser {
    name: String,
    path: PathBuf,
}

/// The browsers installed where they install themselves, or those that
/// `SAYON_BROWSERS` lists.
fn browsers() -> Vec<Browser> {
    let named = |path: PathBuf| {
        let file = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        let name = if file.contains("firefox") {
            "Firefox"
        } else if file.contains("edge") {
            "Edge"
        } else {
            "Chrome"
        };
        Browser {
            name: name.to_string(),
            path,
        }
    };
    if let Some(list) = std::env::var_os("SAYON_BROWSERS") {
        return std::env::split_paths(&list).map(named).collect();
    }
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
        PathBuf::from(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
        PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files\Mozilla Firefox\firefox.exe"),
        PathBuf::from("/usr/bin/microsoft-edge"),
        PathBuf::from("/usr/bin/google-chrome"),
        PathBuf::from("/usr/bin/firefox"),
    ];
    if let Some(local) = local {
        candidates.push(local.join(r"Google\Chrome\Application\chrome.exe"));
        candidates.push(local.join(r"Mozilla Firefox\firefox.exe"));
        // Chrome for Testing unpacked, and Firefox extracted from its
        // installer, uninstalled, as the developer setup has them.
        candidates.push(local.join(r"Programs\chrome-for-testing\chrome.exe"));
        candidates.push(local.join(r"Programs\firefox\core\firefox.exe"));
    }
    let mut found: Vec<Browser> = Vec::new();
    for path in candidates.into_iter().filter(|path| path.is_file()) {
        let browser = named(path);
        if !found.iter().any(|b| b.name == browser.name) {
            found.push(browser);
        }
    }
    found
}

/// Serves `work` over HTTP on this computer, opens its runner page in the
/// browser, headless, and collects the result of each of `count` programs.
fn run_in_browser(browser: &Browser, work: &Path, count: usize) -> Vec<(String, Outcome)> {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().unwrap().port();
    let profile = work.join(format!("profile-{}", browser.name.to_lowercase()));
    let url = format!("http://127.0.0.1:{port}/runner.html");
    let mut command = Command::new(&browser.path);
    if browser.name == "Firefox" {
        command
            .args(["-headless", "-no-remote", "-profile"])
            .arg(&profile);
    } else {
        command
            .args(["--headless=new", "--disable-gpu", "--no-first-run"])
            .args(["--no-default-browser-check", "--disable-extensions"])
            .arg(format!("--user-data-dir={}", profile.display()));
        if browser.name == "Chrome" {
            // Chrome for Testing, unpacked rather than installed, has no
            // exemption that lets its network service's sandbox reach this
            // computer's own addresses; the rest of its sandbox stays.
            command.arg("--disable-features=NetworkServiceSandbox");
        }
    }
    std::fs::create_dir_all(&profile).expect("create the browser profile");
    let mut child = command
        .arg(&url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("start {}: {e}", browser.path.display()));
    let (sender, receiver) = std::sync::mpsc::channel();
    let root = work.to_path_buf();
    // A connection each, since a browser may open one ahead and send
    // nothing on it for a while.
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (root, sender) = (root.clone(), sender.clone());
            std::thread::spawn(move || serve(stream, &root, &sender));
        }
    });
    let mut results = Vec::new();
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(60 + 10 * count as u64);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match receiver.recv_timeout(left) {
            Ok(Some(result)) => results.push(result),
            Ok(None) => break,
            Err(_) => {
                eprintln!(
                    "{}: timed out after {} results",
                    browser.name,
                    results.len()
                );
                break;
            }
        }
    }
    stop_browser(&mut child);
    results
}

/// What the browser check's server sends on: a program's result, or `None`
/// once the page has run them all.
type Results = std::sync::mpsc::Sender<Option<(String, Outcome)>>;

/// Answers one request of the runner page: a file of `root`, or a program's
/// result posted back, which goes to `results`.
fn serve(mut stream: std::net::TcpStream, root: &Path, results: &Results) {
    use std::io::{BufRead, BufReader, Read};
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some((key, value)) = header.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    let target = request.split_whitespace().nth(1).unwrap_or("/").to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let mut reply = |status: &str, kind: &str, data: &[u8]| {
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            data.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(data);
        // Closing at once can reset the connection before the browser has
        // read the reply: finish sending, then wait for the browser to close.
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let _ = std::io::copy(&mut stream, &mut std::io::sink());
    };
    if request.starts_with("POST") {
        reply("200 OK", "text/plain", b"");
        if path == "/done" {
            let _ = results.send(None);
            return;
        }
        let field = |key: &str| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
                .unwrap_or("")
                .to_string()
        };
        let split = field("stdout")
            .parse::<usize>()
            .unwrap_or(0)
            .min(body.len());
        let outcome = Outcome {
            stdout: String::from_utf8_lossy(&body[..split]).to_string(),
            stderr: String::from_utf8_lossy(&body[split..]).to_string(),
            status: field("status").parse().unwrap_or(-1),
        };
        let _ = results.send(Some((field("name"), outcome)));
        return;
    }
    let file = root.join(path.trim_start_matches('/'));
    let kind = match file.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "text/plain; charset=utf-8",
    };
    match std::fs::read(&file) {
        Ok(data) if file.starts_with(root) => reply("200 OK", kind, &data),
        _ => reply("404 Not Found", "text/plain", b"not found"),
    }
}

/// Stops a browser and the processes it started.
fn stop_browser(child: &mut std::process::Child) {
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The programs built for a target, in a work directory removed with them.
struct Built {
    /// Each program's source, with its module.
    programs: Vec<(PathBuf, PathBuf)>,
    /// The programs that need what the target lacks, which are left out.
    lacking: Vec<String>,
    failures: Vec<String>,
    work: PathBuf,
}

impl Built {
    /// Reports what ran, and fails with the problems found.
    fn finish(self, host: &str, failures: Vec<String>) {
        let _ = std::fs::remove_dir_all(&self.work);
        eprintln!(
            "{host}: {} programs ran; {} need what the target lacks: {}",
            self.programs.len(),
            self.lacking.len(),
            self.lacking.join(", ")
        );
        assert!(!self.programs.is_empty(), "no program ran ({host})");
        assert!(
            failures.is_empty(),
            "{} parity problem(s) ({host}):\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
}

/// Builds each program under `dir` for `target`, to run on `host`, in a
/// work directory of the host's own, since the checks run at once. A
/// program that needs a capability the target lacks is diagnosed
/// (`E_CAPABILITY`) and left out.
fn build_programs(dir: &Path, target: &str, host: &str) -> Built {
    let work = std::env::temp_dir().join(format!("sayon-{target}-{host}-{}", std::process::id()));
    std::fs::create_dir_all(&work).expect("create work directory");
    let mut built = Built {
        programs: Vec::new(),
        lacking: Vec::new(),
        failures: Vec::new(),
        work,
    };
    for source in sources(dir) {
        let name = source.file_name().unwrap().to_string_lossy().to_string();
        let wasm = built
            .work
            .join(source.with_extension("wasm").file_name().unwrap());
        let outcome = sayon(
            dir,
            &[
                "build",
                "--color",
                "never",
                "--quiet",
                "--target",
                target,
                "--output",
                &wasm.to_string_lossy(),
                &name,
            ],
        );
        if outcome.status == 0 {
            built.programs.push((source, wasm));
        } else if outcome.stderr.contains("E_CAPABILITY") {
            built.lacking.push(name);
        } else {
            built.failures.push(format!(
                "{name}: the {target} build failed:\n{}",
                outcome.stderr
            ));
        }
    }
    built
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

/// The files of WASI's C library that WASI programs link with.
const WASI_C_LIBRARY: &[&str] = &["crt1-command.o", "libc.a"];

/// Builds the runtime for `triple` and puts it, with the files of the C
/// library it needs from the Rust toolchain, where `sayon` looks for another
/// target's libraries: `lib/<triple>` beside the directory of the `sayon`
/// executable. The runtime builds in a directory of its own, so this cargo
/// never waits for the one running the tests.
fn prepare_runtime(triple: &str, c_library: &[&str]) {
    // Once a run for each target: the checks run at once, and replacing a
    // library while another check links with it fails on Windows.
    static PREPARED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut prepared = PREPARED.lock().unwrap_or_else(|e| e.into_inner());
    if prepared.iter().any(|t| t == triple) {
        return;
    }
    prepared.push(triple.to_string());
    let root = tests_dir().join("..");
    let target_dir = Path::new(env!("CARGO_BIN_EXE_sayon"))
        .parent()
        .and_then(Path::parent)
        .expect("the target directory");
    let build_dir = target_dir.join("wasm-runtime");
    let cargo = option_env!("CARGO").unwrap_or("cargo");
    let built = Command::new(cargo)
        .args([
            "build",
            "--quiet",
            "-p",
            "sayon-runtime",
            "--target",
            triple,
        ])
        .arg("--target-dir")
        .arg(&build_dir)
        .current_dir(&root)
        .status()
        .expect("run cargo");
    assert!(built.success(), "building the runtime for {triple} failed");
    let lib = target_dir.join("lib").join(triple);
    std::fs::create_dir_all(&lib).expect("create the library directory");
    let runtime = build_dir
        .join(triple)
        .join("debug")
        .join("libsayon_runtime.a");
    std::fs::copy(&runtime, lib.join("libsayon_runtime.a"))
        .unwrap_or_else(|e| panic!("copy {}: {e}", runtime.display()));
    if c_library.is_empty() {
        return;
    }
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(&root)
        .output()
        .expect("run rustc");
    let sysroot = PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim());
    let directory = sysroot
        .join("lib")
        .join("rustlib")
        .join(triple)
        .join("lib")
        .join("self-contained");
    for name in c_library {
        std::fs::copy(directory.join(name), lib.join(name))
            .unwrap_or_else(|e| panic!("copy {name} from {}: {e}", directory.display()));
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
