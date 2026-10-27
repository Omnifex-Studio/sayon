//! The registry end to end: `sayon login`, `publish`, `search`, `info`,
//! `logout`, and `install` of versions, against the registry itself, a
//! Sayon program, run by the `sayon` built here from its repository checked
//! out beside this one (`../packages.sayon-lang.org`, or in
//! `SAYON_REPOSITORIES`). Without it, the tests are skipped.

use sayon_driver::json::Json;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

struct Registry {
    child: Child,
    /// The address of its index, which names the registry.
    url: String,
    tokens: Json,
}

impl Drop for Registry {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a registry with the accounts `alice` and `bob`.
fn registry(base: &Path) -> Option<Registry> {
    let repositories = std::env::var_os("SAYON_REPOSITORIES")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."));
    let repository = repositories.join("packages.sayon-lang.org");
    if !repository.join("src/main.sayon").is_file() {
        eprintln!(
            "SKIPPED: the registry's tests (no packages.sayon-lang.org in {})",
            repositories.display()
        );
        return None;
    }
    let log = std::fs::File::create(base.join("registry.log")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_sayon"))
        .args(["run", "--", "--port", "0", "--data"])
        .arg(base.join("registry"))
        .args(["--account", "alice", "--account", "bob"])
        .current_dir(&repository)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()
        .expect("sayon runs the registry");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let started = Json::parse(line.trim()).unwrap_or_else(|_| {
        let log = std::fs::read_to_string(base.join("registry.log")).unwrap_or_default();
        panic!("the registry did not start: {line}{log}")
    });
    Some(Registry {
        url: started.get("index").as_str().unwrap().to_string(),
        tokens: started.get("accounts").clone(),
        child,
    })
}

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

struct Sayon<'r> {
    registry: &'r Registry,
    cache: PathBuf,
    credentials: PathBuf,
}

impl Sayon<'_> {
    fn run(&self, dir: &Path, args: &[&str], input: &str) -> Run {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sayon"))
            .args(args)
            .current_dir(dir)
            .env("SAYON_REGISTRY", &self.registry.url)
            .env("SAYON_CACHE", &self.cache)
            .env("SAYON_CREDENTIALS", &self.credentials)
            .env_remove("SAYON_TOKEN")
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sayon runs");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        Run {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
        }
    }

    fn ok(&self, dir: &Path, args: &[&str]) -> Run {
        let run = self.run(dir, args, "");
        assert_eq!(run.status, 0, "sayon {}: {}", args.join(" "), run.stderr);
        run
    }

    fn fails(&self, dir: &Path, args: &[&str], message: &str) {
        let run = self.run(dir, args, "");
        assert_ne!(run.status, 0, "sayon {} succeeded", args.join(" "));
        assert!(
            run.stderr.contains(message),
            "sayon {}: {}",
            args.join(" "),
            run.stderr
        );
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sr-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes the package `strings` at a version, its `shout` ending with `end`.
fn strings(dir: &Path, version: &str, end: &str) {
    write(
        &dir.join("sayon.toml"),
        &format!(
            "[package]\nname = \"strings\"\nversion = \"{version}\"\ndescription = \"Helpers for text\"\n"
        ),
    );
    write(
        &dir.join("src/shout.sayon"),
        &format!("function shout(text: string) -> string\n    return text + \"{end}\"\n"),
    );
}

#[test]
fn packages_are_published_found_and_installed() {
    let base = scratch("flow");
    let Some(registry) = registry(&base) else {
        return;
    };
    let alice = Sayon {
        registry: &registry,
        cache: base.join("cache"),
        credentials: base.join("alice-credentials"),
    };
    let bob = Sayon {
        registry: &registry,
        cache: base.join("cache"),
        credentials: base.join("bob-credentials"),
    };
    let token = |name: &str| format!("{}\n", registry.tokens.get(name).as_str().unwrap());

    // Publishing needs an account, and a token the registry knows.
    let package = base.join("strings");
    strings(&package, "0.2.0", "!");
    alice.fails(&package, &["publish"], "run `sayon login`");
    let wrong = alice.run(&base, &["login"], "sayon_not_a_token\n");
    assert_ne!(wrong.status, 0);
    assert!(
        wrong.stderr.contains("not one this registry knows"),
        "{}",
        wrong.stderr
    );
    let login = alice.run(&base, &["login"], &token("alice"));
    assert_eq!(login.status, 0, "{}", login.stderr);
    assert!(login.stderr.contains("as alice"), "{}", login.stderr);

    let dry = alice.ok(&package, &["publish", "--dry-run"]);
    assert!(
        dry.stdout.starts_with("strings 0.2.0: 2 files"),
        "{}",
        dry.stdout
    );
    assert!(
        dry.stdout.contains("    src/shout.sayon\n"),
        "{}",
        dry.stdout
    );
    alice.fails(&base, &["info", "strings"], "there is no package `strings`");
    alice.ok(&package, &["publish"]);
    alice.fails(&package, &["publish"], "already published");
    strings(&package, "0.2.1", "!");
    alice.ok(&package, &["publish"]);
    strings(&package, "0.3.0", "?");
    alice.ok(&package, &["publish"]);

    // A name belongs to the account that published it first.
    let login = bob.run(&base, &["login"], &token("bob"));
    assert_eq!(login.status, 0, "{}", login.stderr);
    strings(&package, "0.4.0", "?");
    bob.fails(&package, &["publish"], "belongs to the account `alice`");

    // A published package depends only on registry packages.
    let words = base.join("words");
    write(
        &words.join("sayon.toml"),
        "[package]\nname = \"words\"\nversion = \"1.0.0\"\n\n[dependencies]\nstrings = { path = \"../strings\" }\n",
    );
    write(
        &words.join("src/words.sayon"),
        "strings = import(\"dependencies/strings/src/shout.sayon\")\n\nfunction loud(text: string) -> string\n    return strings.shout(strings.shout(text))\n",
    );
    alice.fails(&words, &["publish"], "is not from a registry");
    write(
        &words.join("sayon.toml"),
        "[package]\nname = \"words\"\nversion = \"1.0.0\"\ndescription = \"Words, louder\"\n\n[dependencies]\nstrings = \"0.2\"\n",
    );
    alice.ok(&words, &["publish"]);

    let search = alice.ok(&base, &["search", "text"]);
    assert_eq!(search.stdout, "strings = \"0.3.0\"  # Helpers for text\n");
    let none = alice.ok(&base, &["search", "nothing"]);
    assert!(none.stderr.contains("no packages"), "{}", none.stderr);
    let info = alice.ok(&base, &["info", "words"]);
    assert!(
        info.stdout.starts_with("words 1.0.0\nWords, louder\n"),
        "{}",
        info.stdout
    );
    assert!(
        info.stdout.contains("    strings \"0.2\"\n"),
        "{}",
        info.stdout
    );
    let info = alice.ok(&base, &["info", "strings"]);
    let listed: Vec<&str> = info
        .stdout
        .lines()
        .skip_while(|line| *line != "versions:")
        .skip(1)
        .take(3)
        .map(|line| line.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(listed, ["0.3.0", "0.2.1", "0.2.0"]);

    // Installing chooses one version of each package that every range
    // accepts, and keeps it while they still do.
    let app = base.join("app");
    let manifest = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nwords = \"1\"\nstrings = \"0.2\"\n";
    write(&app.join("sayon.toml"), manifest);
    write(
        &app.join("src/main.sayon"),
        "words = import(\"dependencies/words/src/words.sayon\")\nstrings = import(\"dependencies/strings/src/shout.sayon\")\n\nprint(words.loud(\"hey\"), strings.shout(\"you\"))\n",
    );
    alice.fails(&app, &["install", "--offline"], "installing is offline");
    let installed = alice.ok(&app, &["install"]);
    assert!(
        installed.stderr.contains("2 packages; wrote sayon.lock"),
        "{}",
        installed.stderr
    );
    let lock = std::fs::read_to_string(app.join("sayon.lock")).unwrap();
    let url = &registry.url;
    assert!(
        lock.contains(&format!(
            "dependencies = [\"strings = registry:{url}#strings@0.2.1\", \"words = registry:{url}#words@1.0.0\"]"
        )),
        "{lock}"
    );
    assert!(lock.contains("digest = \"sha256:"), "{lock}");
    assert_eq!(alice.ok(&app, &["run"]).stdout, "hey!! you!\n");

    strings(&package, "0.2.2", "!");
    alice.ok(&package, &["publish"]);
    let again = alice.ok(&app, &["install"]);
    assert!(again.stderr.contains("up to date"), "{}", again.stderr);
    std::fs::remove_file(app.join("sayon.lock")).unwrap();
    alice.ok(&app, &["install"]);
    let lock = std::fs::read_to_string(app.join("sayon.lock")).unwrap();
    assert!(lock.contains("#strings@0.2.2\""), "{lock}");
    alice.ok(&app, &["install", "--offline", "--locked"]);

    // A cached copy that changed is refused, and installing fetches it again.
    let cached = std::fs::read_dir(base.join("cache/registry"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("strings-0.2.2/src/shout.sayon");
    let original = std::fs::read_to_string(&cached).unwrap();
    std::fs::write(&cached, format!("{original}# changed\n")).unwrap();
    alice.fails(&app, &["run"], "does not match sayon.lock");
    alice.ok(&app, &["install"]);
    assert_eq!(alice.ok(&app, &["run"]).stdout, "hey!! you!\n");

    // Ranges no one version matches are an error that names them.
    write(
        &app.join("sayon.toml"),
        &manifest.replace("strings = \"0.2\"", "strings = \"0.3\""),
    );
    alice.fails(&app, &["run"], "does not accept");
    alice.fails(
        &app,
        &["install"],
        "matches every range asked for: \"0.3\" (this project), \"0.2\" (words 1.0.0)",
    );
    write(
        &app.join("sayon.toml"),
        &manifest.replace("words = \"1\"", "words = \"2\""),
    );
    alice.fails(&app, &["install"], "no version of `words`");
    write(
        &app.join("sayon.toml"),
        &manifest.replace("words = \"1\"", "wordz = \"1\""),
    );
    alice.fails(&app, &["install"], "there is no package `wordz`");

    let out = alice.ok(&base, &["logout"]);
    assert!(out.stderr.contains("logged out"), "{}", out.stderr);
    alice.fails(&package, &["publish"], "run `sayon login`");
    let _ = std::fs::remove_dir_all(&base);
}
