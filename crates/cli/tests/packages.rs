//! Dependencies end to end: `sayon install`, the lock, and imports of
//! `dependencies/...`, with local packages and a Git repository on disk.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

fn sayon(dir: &Path, cache: &Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_sayon"))
        .args(args)
        .current_dir(dir)
        .env("SAYON_CACHE", cache)
        .env("NO_COLOR", "1")
        .output()
        .expect("sayon runs");
    Run {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A fresh, short directory under the system's temporary folder.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn local_packages(base: &Path) {
    write(
        &base.join("utilities/sayon.toml"),
        "[package]\nname = \"utilities\"\nversion = \"0.2.0\"\n\n[dependencies]\ninner = { path = \"../inner\" }\n",
    );
    write(
        &base.join("utilities/src/library.sayon"),
        "helpers = import(\"src/helpers.sayon\")\ninner = import(\"dependencies/inner/math.sayon\")\n\nfunction shout(text: string) -> string\n    return helpers.exclaim(text)\n\nfunction twice(n: integer) -> integer\n    return inner.double(n)\n",
    );
    write(
        &base.join("utilities/src/helpers.sayon"),
        "function exclaim(text: string) -> string\n    return text + \"!\"\n",
    );
    write(
        &base.join("inner/sayon.toml"),
        "[package]\nname = \"inner\"\nversion = \"1.0.0\"\n",
    );
    write(
        &base.join("inner/math.sayon"),
        "function double(n: integer) -> integer\n    return n * 2\n",
    );
}

#[test]
fn local_dependencies_are_locked_and_imported() {
    let base = scratch("local");
    let cache = base.join("cache");
    local_packages(&base);
    let app = base.join("app");
    let manifest = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nutilities = { path = \"../utilities\" }\n";
    write(&app.join("sayon.toml"), manifest);
    write(
        &app.join("src/main.sayon"),
        "utilities = import(\"dependencies/utilities/src/library.sayon\")\n\nprint(utilities.shout(\"hello\"), utilities.twice(21))\n",
    );

    let before = sayon(&app, &cache, &["run"]);
    assert_eq!(before.status, 2, "{}", before.stderr);
    assert!(before.stderr.contains("not installed"), "{}", before.stderr);

    let installed = sayon(&app, &cache, &["install"]);
    assert_eq!(installed.status, 0, "{}", installed.stderr);
    assert!(installed.stderr.contains("2 packages; wrote sayon.lock"));
    let lock = std::fs::read_to_string(app.join("sayon.lock")).unwrap();
    assert!(
        lock.contains("dependencies = [\"utilities = path:../utilities\"]"),
        "{lock}"
    );
    assert!(
        lock.contains("dependencies = [\"inner = path:../inner\"]"),
        "{lock}"
    );

    let run = sayon(&app, &cache, &["run"]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert_eq!(run.stdout, "hello! 42\n");
    let again = sayon(&app, &cache, &["install", "--locked"]);
    assert_eq!(again.status, 0, "{}", again.stderr);
    assert!(again.stderr.contains("up to date"));

    // A changed manifest needs a new lock.
    write(
        &app.join("sayon.toml"),
        &format!("{manifest}inner = {{ path = \"../inner\" }}\n"),
    );
    let stale = sayon(&app, &cache, &["check"]);
    assert_eq!(stale.status, 2);
    assert!(
        stale
            .stderr
            .contains("changed since sayon.lock was written"),
        "{}",
        stale.stderr
    );
    let locked = sayon(&app, &cache, &["install", "--locked"]);
    assert_eq!(locked.status, 2, "{}", locked.stderr);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn git_dependencies_are_fetched_and_verified() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git is not available; skipping");
        return;
    }
    let base = scratch("git");
    let cache = base.join("cache");
    let repo = base.join("codec");
    write(
        &repo.join("sayon.toml"),
        "[package]\nname = \"codec\"\nversion = \"1.4.0\"\n",
    );
    write(
        &repo.join("src/codec.sayon"),
        "function encode(text: string) -> string\n    return \"<\" + text + \">\"\n",
    );
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(&["init", "--quiet"]);
    git(&["add", "-A"]);
    git(&["commit", "--quiet", "-m", "codec"]);
    let revision = git(&["rev-parse", "HEAD"]);

    let app = base.join("app");
    let url = repo.to_string_lossy().replace('\\', "/");
    write(
        &app.join("sayon.toml"),
        &format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\ncodec = {{ git = \"{url}\", revision = \"{revision}\" }}\n"
        ),
    );
    write(
        &app.join("src/main.sayon"),
        "codec = import(\"dependencies/codec/src/codec.sayon\")\n\nprint(codec.encode(\"hi\"))\n",
    );

    let offline = sayon(&app, &cache, &["install", "--offline"]);
    assert_eq!(offline.status, 2);
    assert!(
        offline.stderr.contains("not in the cache"),
        "{}",
        offline.stderr
    );
    let installed = sayon(&app, &cache, &["install"]);
    assert_eq!(installed.status, 0, "{}", installed.stderr);
    let lock = std::fs::read_to_string(app.join("sayon.lock")).unwrap();
    assert!(lock.contains(&format!("git:{url}#{revision}")), "{lock}");
    assert!(lock.contains("digest = \"sha256:"), "{lock}");
    let run = sayon(&app, &cache, &["run"]);
    assert_eq!(run.stdout, "<hi>\n", "{}", run.stderr);

    // A changed cached copy is refused, and `install` fetches it again.
    let entry = std::fs::read_dir(cache.join("git"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .unwrap();
    let file = entry.join("src/codec.sayon");
    let original = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, format!("{original}# changed\n")).unwrap();
    let tampered = sayon(&app, &cache, &["run"]);
    assert_eq!(tampered.status, 2);
    assert!(
        tampered.stderr.contains("does not match sayon.lock"),
        "{}",
        tampered.stderr
    );
    let refetched = sayon(&app, &cache, &["install"]);
    assert_eq!(refetched.status, 0, "{}", refetched.stderr);
    assert_eq!(sayon(&app, &cache, &["run"]).stdout, "<hi>\n");

    let _ = std::fs::remove_dir_all(&base);
}
