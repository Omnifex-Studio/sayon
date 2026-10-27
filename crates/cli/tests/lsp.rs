//! `sayon lsp`, driven as an editor drives it: a program of two files is
//! opened, its diagnostics arrive, and hover, going to a definition,
//! references, renaming, completion, and formatting answer as they should.

use sayon_lsp::json::Json;
use sayon_lsp::text::{path_of, uri_of};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const MAIN: &str = r#"graphics = import("standard/graphics.sayon")
shapes = import("shapes.sayon")

# Doubles a number.
function double(value: integer) -> integer
    return value * 2

total = double(21)
square = shapes.Square(side = 3)
print(total, shapes.area(square), missing)
canvas = graphics.canvas(10, 10)
"#;

const SHAPES: &str = r#"type Square = [side: integer]

function area(square: Square) -> integer
    return square.side * square.side
"#;

struct Editor {
    child: Child,
    input: ChildStdin,
    messages: mpsc::Receiver<Json>,
    next_id: u32,
}

impl Editor {
    fn start() -> Editor {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sayon"))
            .arg("lsp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start sayon lsp");
        let input = child.stdin.take().expect("its input");
        let output = child.stdout.take().expect("its output");
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Content-Length:") {
                        length = value.trim().parse().expect("a length");
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                let message = Json::parse(&String::from_utf8(body).expect("UTF-8")).expect("JSON");
                if sender.send(message).is_err() {
                    return;
                }
            }
        });
        Editor {
            child,
            input,
            messages,
            next_id: 0,
        }
    }

    fn send(&mut self, message: Json) {
        let body = message.to_string();
        write!(self.input, "Content-Length: {}\r\n\r\n{body}", body.len()).expect("write");
        self.input.flush().expect("flush");
    }

    fn notify(&mut self, method: &str, params: Json) {
        self.send(Json::object(vec![
            ("jsonrpc", Json::string("2.0")),
            ("method", Json::string(method)),
            ("params", params),
        ]));
    }

    /// The next message `wanted` takes, passing over others.
    fn wait(&mut self, what: &str, wanted: impl Fn(&Json) -> bool) -> Json {
        loop {
            let message = self
                .messages
                .recv_timeout(Duration::from_secs(60))
                .unwrap_or_else(|_| panic!("no {what} from the server"));
            if wanted(&message) {
                return message;
            }
        }
    }

    fn request(&mut self, method: &str, params: Json) -> Json {
        self.next_id += 1;
        let id = self.next_id;
        self.send(Json::object(vec![
            ("jsonrpc", Json::string("2.0")),
            ("id", Json::number(id)),
            ("method", Json::string(method)),
            ("params", params),
        ]));
        let answer = self.wait(method, |m| m.get("id").as_u32() == Some(id));
        assert!(
            answer.get("error").is_null(),
            "{method} failed: {}",
            answer.get("error")
        );
        answer.get("result").clone()
    }

    fn diagnostics(&mut self, uri: &str) -> Vec<Json> {
        let uri = uri.to_string();
        let message = self.wait("diagnostics", |m| {
            m.get("method").as_str() == Some("textDocument/publishDiagnostics")
                && m.get("params").get("uri").as_str() == Some(uri.as_str())
        });
        message.get("params").get("diagnostics").as_array().to_vec()
    }
}

/// A request's parameters for a place in a document.
fn at(uri: &str, line: u32, character: u32) -> Json {
    Json::object(vec![
        (
            "textDocument",
            Json::object(vec![("uri", Json::string(uri))]),
        ),
        (
            "position",
            Json::object(vec![
                ("line", Json::number(line)),
                ("character", Json::number(character)),
            ]),
        ),
    ])
}

/// The line and character of the `nth` place `needle` is in `text`.
fn find(text: &str, needle: &str, nth: usize) -> (u32, u32) {
    let offset = text.match_indices(needle).nth(nth).expect(needle).0;
    let before = &text[..offset];
    let line = before.matches('\n').count() as u32;
    let character = (offset - before.rfind('\n').map_or(0, |n| n + 1)) as u32;
    (line, character)
}

fn labels(completion: &Json) -> Vec<String> {
    completion
        .get("items")
        .as_array()
        .iter()
        .filter_map(|item| item.get("label").as_str().map(str::to_string))
        .collect()
}

#[test]
fn an_editor_gets_answers_about_a_program() {
    let dir = std::env::temp_dir().join(format!("sayon-lsp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory");
    let dir = std::fs::canonicalize(&dir).expect("its path");
    std::fs::write(dir.join("main.sayon"), MAIN).expect("main.sayon");
    std::fs::write(dir.join("shapes.sayon"), SHAPES).expect("shapes.sayon");
    let main = uri_of(&dir.join("main.sayon"));
    let mut editor = Editor::start();

    let initialized = editor.request(
        "initialize",
        Json::object(vec![
            ("processId", Json::Null),
            ("rootUri", Json::string(uri_of(&dir))),
            ("capabilities", Json::object(vec![])),
        ]),
    );
    let capabilities = initialized.get("capabilities");
    for provider in [
        "hoverProvider",
        "definitionProvider",
        "referencesProvider",
        "documentFormattingProvider",
    ] {
        assert_eq!(capabilities.get(provider), &Json::Bool(true), "{provider}");
    }
    editor.notify("initialized", Json::object(vec![]));

    let open = |text: &str, version: u32| {
        Json::object(vec![(
            "textDocument",
            Json::object(vec![
                ("uri", Json::string(main.clone())),
                ("languageId", Json::string("sayon")),
                ("version", Json::number(version)),
                ("text", Json::string(text)),
            ]),
        )])
    };
    editor.notify("textDocument/didOpen", open(MAIN, 1));

    // The one error: `missing` is not defined.
    let diagnostics = editor.diagnostics(&main);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].get("code").as_str(),
        Some("E_UNDEFINED_NAME")
    );
    let (line, character) = find(MAIN, "missing", 0);
    assert_eq!(
        diagnostics[0]
            .get("range")
            .get("start")
            .get("line")
            .as_u32(),
        Some(line)
    );
    assert_eq!(
        diagnostics[0]
            .get("range")
            .get("start")
            .get("character")
            .as_u32(),
        Some(character)
    );

    // Hover shows the signature and the comment above it.
    let (line, character) = find(MAIN, "double(21)", 0);
    let hover = editor.request("textDocument/hover", at(&main, line, character + 2));
    let shown = hover
        .get("contents")
        .get("value")
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        shown.contains("function double(value: integer) -> integer"),
        "{shown}"
    );
    assert!(shown.contains("Doubles a number."), "{shown}");
    let (line, character) = find(MAIN, "square = shapes", 0);
    let hover = editor.request("textDocument/hover", at(&main, line, character + 1));
    let shown = hover
        .get("contents")
        .get("value")
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(shown.contains("square: Square"), "{shown}");

    // A definition in another file.
    let (line, character) = find(MAIN, "area(square)", 0);
    let definition = editor.request("textDocument/definition", at(&main, line, character + 1));
    let target = path_of(definition.get("uri").as_str().expect("a location")).expect("a path");
    assert_eq!(
        std::fs::canonicalize(target).unwrap(),
        dir.join("shapes.sayon")
    );
    assert_eq!(
        definition.get("range").get("start").get("line").as_u32(),
        Some(2)
    );
    // An import's alias goes to its module.
    let (line, character) = find(MAIN, "graphics.canvas", 0);
    let module = editor.request("textDocument/definition", at(&main, line, character + 1));
    assert!(
        module
            .get("uri")
            .as_str()
            .unwrap_or_default()
            .ends_with("graphics.sayon")
    );

    // References, and renaming them all.
    let (line, character) = find(MAIN, "total", 0);
    let mut params = at(&main, line, character);
    if let Json::Object(members) = &mut params {
        members.push((
            "context".to_string(),
            Json::object(vec![("includeDeclaration", Json::Bool(true))]),
        ));
    }
    let references = editor.request("textDocument/references", params);
    assert_eq!(references.as_array().len(), 2, "{references}");
    let mut params = at(&main, line, character);
    if let Json::Object(members) = &mut params {
        members.push(("newName".to_string(), Json::string("sum")));
    }
    let renamed = editor.request("textDocument/rename", params);
    let edits = renamed.get("changes").get(&main).as_array().to_vec();
    assert_eq!(edits.len(), 2, "{renamed}");
    assert!(
        edits
            .iter()
            .all(|e| e.get("newText").as_str() == Some("sum"))
    );
    // The standard library's names stay.
    let (line, character) = find(MAIN, "canvas(10", 0);
    editor.next_id += 1;
    let id = editor.next_id;
    let mut params = at(&main, line, character + 1);
    if let Json::Object(members) = &mut params {
        members.push(("newName".to_string(), Json::string("paper")));
    }
    editor.send(Json::object(vec![
        ("jsonrpc", Json::string("2.0")),
        ("id", Json::number(id)),
        ("method", Json::string("textDocument/rename")),
        ("params", params),
    ]));
    let refused = editor.wait("a refused rename", |m| m.get("id").as_u32() == Some(id));
    assert!(!refused.get("error").is_null(), "{refused}");

    // Completion after a module's alias and after a value of a type, as the
    // text is typed, with the syntax error the dot makes.
    let typed = format!("{MAIN}shapes.\ncanvas.\n");
    editor.notify(
        "textDocument/didChange",
        Json::object(vec![
            (
                "textDocument",
                Json::object(vec![
                    ("uri", Json::string(main.clone())),
                    ("version", Json::number(2)),
                ]),
            ),
            (
                "contentChanges",
                Json::Array(vec![Json::object(vec![(
                    "text",
                    Json::string(typed.clone()),
                )])]),
            ),
        ]),
    );
    let (line, character) = find(&typed, "shapes.\n", 0);
    let members =
        labels(&editor.request("textDocument/completion", at(&main, line, character + 7)));
    assert!(
        members.contains(&"area".to_string()) && members.contains(&"Square".to_string()),
        "{members:?}"
    );
    let (line, character) = find(&typed, "canvas.\n", 0);
    let members =
        labels(&editor.request("textDocument/completion", at(&main, line, character + 7)));
    for member in ["fill_circle", "smooth", "write", "png"] {
        assert!(
            members.contains(&member.to_string()),
            "{member} in {members:?}"
        );
    }
    let names = labels(&editor.request("textDocument/completion", at(&main, line, 0)));
    for name in ["double", "square", "graphics", "print", "function"] {
        assert!(names.contains(&name.to_string()), "{name} in {names:?}");
    }

    // Formatting a document that is not formatted.
    let untidy = "x   =  1\nprint( x )\n";
    editor.notify("textDocument/didOpen", {
        let mut message = open(untidy, 1);
        if let Json::Object(members) = &mut message
            && let Json::Object(document) = &mut members[0].1
        {
            document[0].1 = Json::string(uri_of(&dir.join("untidy.sayon")));
        }
        message
    });
    let untidy_uri = uri_of(&dir.join("untidy.sayon"));
    let edits = editor.request(
        "textDocument/formatting",
        Json::object(vec![
            (
                "textDocument",
                Json::object(vec![("uri", Json::string(untidy_uri))]),
            ),
            (
                "options",
                Json::object(vec![
                    ("tabSize", Json::number(4)),
                    ("insertSpaces", Json::Bool(true)),
                ]),
            ),
        ]),
    );
    assert_eq!(
        edits.as_array()[0].get("newText").as_str(),
        Some("x = 1\nprint(x)\n")
    );

    editor.request("shutdown", Json::Null);
    editor.notify("exit", Json::Null);
    let status = editor.child.wait().expect("the server ends");
    assert!(status.success(), "{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

const SHAPES_PROGRAM: &str = r#"graphics = import("standard/graphics.sayon")

# A shape of one size.
enum Shape
    circle(radius: float)
    square(side: float)

function area(shape: Shape) -> float
    match shape
        case circle(radius)
            return radius * radius * 3.0
        case square(side)
            return side * side

canvas = graphics.canvas(8, 8)
"#;

/// Changes the document to `text`, as the editor does with each keystroke.
fn change(editor: &mut Editor, uri: &str, version: u32, text: &str) {
    editor.notify(
        "textDocument/didChange",
        Json::object(vec![
            (
                "textDocument",
                Json::object(vec![
                    ("uri", Json::string(uri)),
                    ("version", Json::number(version)),
                ]),
            ),
            (
                "contentChanges",
                Json::Array(vec![Json::object(vec![("text", Json::string(text))])]),
            ),
        ]),
    );
}

/// The completion items at the end of the program with `typed` after it.
fn complete_after(editor: &mut Editor, uri: &str, version: u32, typed: &str) -> Vec<Json> {
    let text = format!("{SHAPES_PROGRAM}{typed}");
    change(editor, uri, version, &text);
    let lines: Vec<&str> = text.split('\n').collect();
    let line = lines.len() as u32 - 1;
    let character = lines.last().unwrap().encode_utf16().count() as u32;
    editor
        .request("textDocument/completion", at(uri, line, character))
        .get("items")
        .as_array()
        .to_vec()
}

fn label(item: &Json) -> String {
    item.get("label").as_str().unwrap_or_default().to_string()
}

fn item_named(items: &[Json], name: &str) -> Json {
    items
        .iter()
        .find(|item| item.get("label").as_str() == Some(name))
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "no `{name}` in {:?}",
                items.iter().map(label).collect::<Vec<_>>()
            )
        })
}

/// Whether an item comes first, as fitting the place.
fn fits(item: &Json) -> bool {
    item.get("sortText")
        .as_str()
        .unwrap_or_default()
        .starts_with('0')
}

/// Completion that reads the place: what fits an argument's type first,
/// fields to name, variants after `case`, types, import paths, modules to
/// import, and nothing in comments; and signature help.
#[test]
fn completion_fits_the_place() {
    let dir = std::env::temp_dir().join(format!("sayon-lsp-places-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory");
    let dir = std::fs::canonicalize(&dir).expect("its path");
    let file = dir.join("shapes.sayon");
    std::fs::write(&file, SHAPES_PROGRAM).expect("the program");
    let uri = uri_of(&file);
    let mut editor = Editor::start();
    editor.request(
        "initialize",
        Json::object(vec![
            ("processId", Json::Null),
            ("capabilities", Json::object(vec![])),
        ]),
    );
    editor.notify(
        "textDocument/didOpen",
        Json::object(vec![(
            "textDocument",
            Json::object(vec![
                ("uri", Json::string(uri.clone())),
                ("languageId", Json::string("sayon")),
                ("version", Json::number(1)),
                ("text", Json::string(SHAPES_PROGRAM)),
            ]),
        )]),
    );

    // A color goes in the fourth argument: the module's colors come first.
    let items = complete_after(&mut editor, &uri, 2, "canvas.fill_circle(1, 2, 3, ");
    assert!(fits(&item_named(&items, "graphics.BLACK")));
    assert!(fits(&item_named(&items, "graphics.rgb")));
    assert!(!fits(&item_named(&items, "canvas")));
    // A shape goes in `area`: its variants come first, with their fields.
    let items = complete_after(&mut editor, &uri, 3, "print(area(");
    let circle = item_named(&items, "circle");
    assert!(fits(&circle));
    assert_eq!(
        circle.get("insertText").as_str(),
        Some("circle(${1:radius})")
    );
    // The fields left to name in a structure being built.
    let items = complete_after(&mut editor, &uri, 4, "corner = graphics.Point(x = 1, ");
    assert_eq!(
        item_named(&items, "y").get("insertText").as_str(),
        Some("y = ")
    );
    assert!(!items.iter().any(|item| label(item) == "x"));
    // Variants after `case`.
    let items = complete_after(
        &mut editor,
        &uri,
        5,
        "function name(shape: Shape) -> string\n    match shape\n        case ",
    );
    let names: Vec<String> = items.iter().map(label).collect();
    assert!(
        names.contains(&"circle".to_string()) && names.contains(&"square".to_string()),
        "{names:?}"
    );
    // Types after `:`.
    let names: Vec<String> = complete_after(&mut editor, &uri, 6, "function f(value: ")
        .iter()
        .map(label)
        .collect();
    for name in ["integer", "Shape", "graphics"] {
        assert!(names.contains(&name.to_string()), "{name} in {names:?}");
    }
    assert!(!names.contains(&"area".to_string()), "{names:?}");
    // Paths in an import, replacing what was typed of one.
    let items = complete_after(&mut editor, &uri, 7, "strings = import(\"standard/st");
    let path = item_named(&items, "standard/strings.sayon");
    assert_eq!(
        path.get("textEdit").get("newText").as_str(),
        Some("standard/strings.sayon")
    );
    // Nothing in a comment.
    assert!(complete_after(&mut editor, &uri, 8, "# canvas.").is_empty());
    // A standard module not imported yet, with its import after the others.
    let items = complete_after(&mut editor, &uri, 9, "");
    let strings = item_named(&items, "strings");
    let edit = &strings.get("additionalTextEdits").as_array()[0];
    assert_eq!(
        edit.get("newText").as_str(),
        Some("strings = import(\"standard/strings.sayon\")\n")
    );
    assert_eq!(edit.get("range").get("start").get("line").as_u32(), Some(1));
    // A function comes with placeholders for its arguments.
    assert_eq!(
        item_named(&items, "area").get("insertText").as_str(),
        Some("area(${1:shape})")
    );
    assert!(!items.iter().any(|item| label(item) == "graphics.BLACK"));

    // Signature help, the third parameter marked.
    let text = format!("{SHAPES_PROGRAM}canvas.fill_circle(1, 2, ");
    change(&mut editor, &uri, 10, &text);
    let (line, character) = find(&text, "fill_circle(1, 2, ", 0);
    let help = editor.request("textDocument/signatureHelp", at(&uri, line, character + 18));
    let signature = &help.get("signatures").as_array()[0];
    assert_eq!(
        signature.get("label").as_str(),
        Some("canvas.fill_circle(x: integer, y: integer, radius: integer, color: Color)")
    );
    assert_eq!(help.get("activeParameter").as_u32(), Some(2));
    assert_eq!(signature.get("parameters").as_array().len(), 4);

    editor.request("shutdown", Json::Null);
    editor.notify("exit", Json::Null);
    assert!(editor.child.wait().expect("the server ends").success());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The VS Code extension's client (`editors/vscode/extension.js`), run in
/// Node with a stand-in for VS Code's API (`tests/vscode/client.cjs`),
/// answers through `sayon lsp`. Without Node, the check says it skipped.
#[test]
fn the_vscode_extension_talks_to_the_server() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let node = std::env::var_os("SAYON_NODE").unwrap_or_else(|| "node".into());
    let output = match Command::new(&node)
        .arg(root.join("tests/vscode/client.cjs"))
        .arg(env!("CARGO_BIN_EXE_sayon"))
        .arg(root.join("editors/vscode"))
        .output()
    {
        Ok(output) => output,
        Err(_) => {
            eprintln!("SKIPPED: the VS Code extension's check (no Node)");
            return;
        }
    };
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
