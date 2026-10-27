//! The server: messages read from the editor and answered, each open
//! document checked as the entry of a program.

use crate::json::Json;
use crate::text::{offset, path_of, position, uri_of};
use sayon_check::index::Index;
use sayon_syntax::{Diagnostic, FileId, Severity, SourceMap, Span};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// The driver's options for checking the program whose entry is the path,
/// as `sayon check` would have them, or why there are none.
pub type Configure = dyn Fn(&Path) -> Result<sayon_driver::Options, String>;

/// JSON-RPC's error codes.
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const REQUEST_FAILED: i32 = -32803;

/// An open document: where it is, and what the editor holds of it.
struct Document {
    path: PathBuf,
    text: String,
    version: i64,
}

/// What checking a document gave: its program's sources, among them the
/// document as it was, what the names in them refer to, and where its
/// program's modules are found.
pub(crate) struct Analysis {
    pub(crate) sources: SourceMap,
    pub(crate) index: Index,
    pub(crate) file: FileId,
    pub(crate) stdlib: Option<PathBuf>,
    /// The program's root: its project's, or its entry's directory.
    pub(crate) root: Option<PathBuf>,
}

struct Server<'c, W: Write> {
    output: W,
    configure: &'c Configure,
    documents: HashMap<String, Document>,
    analyses: HashMap<String, Analysis>,
    /// Documents changed since they were last checked.
    dirty: Vec<String>,
    shut_down: bool,
}

/// Serves an editor over `input` and `output` until it says to exit, and
/// gives the process's exit status: 0 after a shutdown, 1 otherwise.
pub fn serve(input: impl Read + Send + 'static, output: impl Write, configure: &Configure) -> i32 {
    let (sender, messages) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(input);
        while let Some(message) = read_message(&mut reader) {
            if sender.send(message).is_err() {
                break;
            }
        }
    });
    let mut server = Server {
        output,
        configure,
        documents: HashMap::new(),
        analyses: HashMap::new(),
        dirty: Vec::new(),
        shut_down: false,
    };
    loop {
        let Ok(message) = messages.recv() else {
            return if server.shut_down { 0 } else { 1 };
        };
        if let Some(status) = server.handle(message) {
            return status;
        }
        // What came meanwhile, such as each keystroke's change, before
        // checking again.
        while let Ok(message) = messages.try_recv() {
            if let Some(status) = server.handle(message) {
                return status;
            }
        }
        server.check_dirty();
    }
}

/// One message: headers, the length among them, a blank line, and JSON.
fn read_message(reader: &mut impl BufRead) -> Option<Json> {
    loop {
        let mut length = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let length = length?;
        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok()?;
        // A message that is not JSON is passed over.
        if let Some(message) = String::from_utf8(body)
            .ok()
            .and_then(|text| Json::parse(&text).ok())
        {
            return Some(message);
        }
    }
}

impl<W: Write> Server<'_, W> {
    fn send(&mut self, message: Json) {
        let body = message.to_string();
        let _ = write!(self.output, "Content-Length: {}\r\n\r\n{body}", body.len());
        let _ = self.output.flush();
    }

    fn notify(&mut self, method: &str, params: Json) {
        self.send(Json::object(vec![
            ("jsonrpc", Json::string("2.0")),
            ("method", Json::string(method)),
            ("params", params),
        ]));
    }

    fn log(&mut self, message: String) {
        self.notify(
            "window/logMessage",
            Json::object(vec![
                ("type", Json::number(3)),
                ("message", Json::String(message)),
            ]),
        );
    }

    /// Handles a message, and gives the exit status once it is time to exit.
    fn handle(&mut self, message: Json) -> Option<i32> {
        let method = message.get("method").as_str()?.to_string();
        let params = message.get("params").clone();
        let id = message.get("id").clone();
        if id.is_null() {
            return self.notification(&method, &params);
        }
        let answer = self.request(&method, &params);
        let mut response = vec![("jsonrpc", Json::string("2.0")), ("id", id)];
        match answer {
            Ok(result) => response.push(("result", result)),
            Err((code, text)) => response.push((
                "error",
                Json::object(vec![
                    ("code", Json::number(code)),
                    ("message", Json::String(text)),
                ]),
            )),
        }
        self.send(Json::object(response));
        None
    }

    fn notification(&mut self, method: &str, params: &Json) -> Option<i32> {
        let document = params.get("textDocument");
        let uri = document.get("uri").as_str().unwrap_or_default().to_string();
        match method {
            "exit" => return Some(if self.shut_down { 0 } else { 1 }),
            "textDocument/didOpen" => {
                // A document that is no file, such as an unsaved one, is
                // not checked.
                let path = path_of(&uri)?;
                self.documents.insert(
                    uri.clone(),
                    Document {
                        path,
                        text: document
                            .get("text")
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        version: version(document),
                    },
                );
                self.mark_all_dirty();
            }
            "textDocument/didChange" => {
                if let Some(open) = self.documents.get_mut(&uri) {
                    for change in params.get("contentChanges").as_array() {
                        let text = change.get("text").as_str().unwrap_or_default();
                        let range = change.get("range");
                        if range.is_null() {
                            open.text = text.to_string();
                        } else {
                            let (start, end) = (
                                range_offset(&open.text, range.get("start")),
                                range_offset(&open.text, range.get("end")),
                            );
                            if start <= end && end <= open.text.len() {
                                open.text.replace_range(start..end, text);
                            }
                        }
                    }
                    open.version = version(document);
                    self.mark_all_dirty();
                }
            }
            "textDocument/didClose" => {
                self.documents.remove(&uri);
                self.analyses.remove(&uri);
                self.dirty.retain(|d| *d != uri);
                self.publish(&uri, Vec::new(), None);
                self.mark_all_dirty();
            }
            _ => {}
        }
        None
    }

    /// Every open document is checked again: a change to one can change
    /// what another that imports it means.
    fn mark_all_dirty(&mut self) {
        let mut uris: Vec<String> = self.documents.keys().cloned().collect();
        uris.sort();
        self.dirty = uris;
    }

    fn check_dirty(&mut self) {
        for uri in std::mem::take(&mut self.dirty) {
            self.check(&uri);
        }
    }

    /// The analysis of a document, checked first if it changed.
    fn fresh(&mut self, uri: &str) -> Option<&Analysis> {
        if self.dirty.iter().any(|d| d == uri) || !self.analyses.contains_key(uri) {
            self.dirty.retain(|d| d != uri);
            self.check(uri);
        }
        self.analyses.get(uri)
    }

    /// Checks a document as the entry of a program, and publishes its
    /// diagnostics.
    fn check(&mut self, uri: &str) {
        let Some(document) = self.documents.get(uri) else {
            return;
        };
        let path = document.path.clone();
        let version = document.version;
        let mut options = match (self.configure)(&path) {
            Ok(options) => options,
            Err(problem) => {
                let whole = range_json("", Span::new(FileId(0), 0, 0));
                let diagnostic = Json::object(vec![
                    ("range", whole),
                    ("severity", Json::number(1)),
                    ("source", Json::string("sayon")),
                    ("message", Json::String(problem)),
                ]);
                self.publish(uri, vec![diagnostic], Some(version));
                return;
            }
        };
        options.overlays = self
            .documents
            .values()
            .map(|d| (sayon_driver::canonical(&d.path), d.text.clone()))
            .collect();
        options.index = true;
        options.lint = true;
        let stdlib = options.stdlib.clone();
        let root = options
            .root
            .clone()
            .or_else(|| path.parent().map(Path::to_path_buf));
        let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sayon_driver::compile(&path, &options)
        }));
        let output = match checked {
            Ok(Ok(output)) => output,
            Ok(Err(sayon_driver::EntryError(problem))) => {
                self.log(format!("{}: {problem}", path.display()));
                return;
            }
            Err(_) => {
                self.log(format!(
                    "checking {} failed inside the checker; this is a bug in Sayon",
                    path.display()
                ));
                return;
            }
        };
        let Some(file) = output
            .sources
            .files()
            .find(|f| f.path.as_deref() == Some(path.as_path()))
            .map(|f| f.id)
        else {
            return;
        };
        let diagnostics: Vec<Json> = output
            .diagnostics
            .iter()
            .filter(|d| d.primary_span().is_some_and(|span| span.file == file))
            .map(|d| self.diagnostic_json(d, &output.sources, file))
            .collect();
        self.publish(uri, diagnostics, Some(version));
        self.analyses.insert(
            uri.to_string(),
            Analysis {
                sources: output.sources,
                index: output.index.unwrap_or_default(),
                file,
                stdlib,
                root,
            },
        );
    }

    fn publish(&mut self, uri: &str, diagnostics: Vec<Json>, version: Option<i64>) {
        let mut params = vec![
            ("uri", Json::string(uri)),
            ("diagnostics", Json::Array(diagnostics)),
        ];
        if let Some(version) = version {
            params.push(("version", Json::number(version as f64)));
        }
        self.notify("textDocument/publishDiagnostics", Json::object(params));
    }

    /// The URI of a file: an open document's own, as the editor wrote it,
    /// or else one made from its path.
    fn uri_for(&self, path: &Path) -> String {
        let canonical = sayon_driver::canonical(path);
        self.documents
            .iter()
            .find(|(_, d)| d.path == path || sayon_driver::canonical(&d.path) == canonical)
            .map(|(uri, _)| uri.clone())
            .unwrap_or_else(|| uri_of(path))
    }

    fn location(&self, sources: &SourceMap, span: Span) -> Option<Json> {
        let file = sources.get(span.file);
        let path = file.path.as_deref()?;
        Some(Json::object(vec![
            ("uri", Json::String(self.uri_for(path))),
            ("range", range_json(&file.text, span)),
        ]))
    }

    fn diagnostic_json(&self, diagnostic: &Diagnostic, sources: &SourceMap, file: FileId) -> Json {
        let text = &sources.get(file).text;
        let span = diagnostic.primary_span().expect("a primary span");
        let mut message = diagnostic.message.clone();
        for label in diagnostic.labels.iter().filter(|l| l.primary) {
            if !label.message.is_empty() {
                message.push_str(&format!(" ({})", label.message));
            }
        }
        for note in &diagnostic.notes {
            message.push_str(&format!("\nnote: {note}"));
        }
        for help in &diagnostic.help {
            message.push_str(&format!("\nhelp: {help}"));
        }
        let related: Vec<Json> = diagnostic
            .labels
            .iter()
            .filter(|l| !l.primary)
            .filter_map(|label| {
                Some(Json::object(vec![
                    ("location", self.location(sources, label.span)?),
                    ("message", Json::String(label.message.clone())),
                ]))
            })
            .collect();
        let severity = match diagnostic.severity {
            Severity::Error => 1,
            _ => 2,
        };
        let mut members = vec![
            ("range", range_json(text, span)),
            ("severity", Json::number(severity)),
            ("code", Json::string(diagnostic.code)),
            ("source", Json::string("sayon")),
            ("message", Json::String(message)),
        ];
        if !related.is_empty() {
            members.push(("relatedInformation", Json::Array(related)));
        }
        Json::object(members)
    }

    fn request(&mut self, method: &str, params: &Json) -> Result<Json, (i32, String)> {
        match method {
            "initialize" => Ok(capabilities()),
            "shutdown" => {
                self.shut_down = true;
                Ok(Json::Null)
            }
            "textDocument/hover" => Ok(self.hover(params).unwrap_or(Json::Null)),
            "textDocument/definition" => Ok(self.definition(params).unwrap_or(Json::Null)),
            "textDocument/references" => Ok(self.references(params).unwrap_or(Json::Null)),
            "textDocument/prepareRename" => self.prepare_rename(params),
            "textDocument/rename" => self.rename(params),
            "textDocument/completion" => Ok(self.completion(params).unwrap_or(Json::Null)),
            "textDocument/signatureHelp" => Ok(self.signature_help(params).unwrap_or(Json::Null)),
            "textDocument/formatting" => Ok(self.formatting(params)),
            _ => Err((
                METHOD_NOT_FOUND,
                format!("Sayon's server does not answer `{method}`"),
            )),
        }
    }

    /// The document and byte offset a request's position names.
    fn at(&self, params: &Json) -> Option<(String, usize)> {
        let uri = params.get("textDocument").get("uri").as_str()?.to_string();
        let document = self.documents.get(&uri)?;
        let place = params.get("position");
        Some((uri, range_offset(&document.text, place)))
    }

    /// The symbol whose name is at a request's position, and the name as
    /// written there.
    fn symbol_at(&mut self, params: &Json) -> Option<(String, usize, Span)> {
        let (uri, offset) = self.at(params)?;
        let analysis = self.fresh(&uri)?;
        let reference = analysis.index.at(analysis.file, offset as u32)?;
        Some((uri, reference.symbol, reference.span))
    }

    fn hover(&mut self, params: &Json) -> Option<Json> {
        let (uri, symbol, span) = self.symbol_at(params)?;
        let analysis = self.analyses.get(&uri)?;
        let found = &analysis.index.symbols[symbol];
        let mut value = format!("```sayon\n{}\n```", found.detail);
        if let Some(doc) = crate::complete::documentation(&analysis.sources, found) {
            value.push_str("\n\n");
            value.push_str(&doc);
        }
        let text = &analysis.sources.get(analysis.file).text;
        Some(Json::object(vec![
            (
                "contents",
                Json::object(vec![
                    ("kind", Json::string("markdown")),
                    ("value", Json::String(value)),
                ]),
            ),
            ("range", range_json(text, span)),
        ]))
    }

    fn definition(&mut self, params: &Json) -> Option<Json> {
        let (uri, symbol, _) = self.symbol_at(params)?;
        let analysis = self.analyses.get(&uri)?;
        let found = &analysis.index.symbols[symbol];
        match found.target {
            Some(file) => self.location(&analysis.sources, Span::new(file, 0, 0)),
            None => self.location(&analysis.sources, found.span),
        }
    }

    fn references(&mut self, params: &Json) -> Option<Json> {
        let (uri, symbol, _) = self.symbol_at(params)?;
        let declaration = params
            .get("context")
            .get("includeDeclaration")
            .as_bool()
            .unwrap_or(true);
        let analysis = self.analyses.get(&uri)?;
        let declared = analysis.index.symbols[symbol].span;
        let locations = analysis
            .index
            .references_to(symbol)
            .into_iter()
            .filter(|span| declaration || *span != declared)
            .filter_map(|span| self.location(&analysis.sources, span))
            .collect();
        Some(Json::Array(locations))
    }

    /// Whether the symbol at a request's position can be renamed, and the
    /// name as written there.
    fn renameable(&mut self, params: &Json) -> Result<(String, usize, Span), (i32, String)> {
        let failed = |message: &str| (REQUEST_FAILED, message.to_string());
        let (uri, symbol, span) = self
            .symbol_at(params)
            .ok_or_else(|| failed("there is no name here that Sayon knows"))?;
        let analysis = &self.analyses[&uri];
        let found = &analysis.index.symbols[symbol];
        if found.standard {
            return Err(failed("the standard library's names cannot be renamed"));
        }
        if found.span.is_empty() {
            return Err(failed("this name is declared nowhere it could be renamed"));
        }
        Ok((uri, symbol, span))
    }

    fn prepare_rename(&mut self, params: &Json) -> Result<Json, (i32, String)> {
        let (uri, _, span) = self.renameable(params)?;
        let analysis = &self.analyses[&uri];
        let text = &analysis.sources.get(analysis.file).text;
        Ok(Json::object(vec![
            ("range", range_json(text, span)),
            (
                "placeholder",
                Json::string(text[span.start as usize..span.end as usize].to_string()),
            ),
        ]))
    }

    fn rename(&mut self, params: &Json) -> Result<Json, (i32, String)> {
        let (uri, symbol, _) = self.renameable(params)?;
        let new_name = params
            .get("newName")
            .as_str()
            .unwrap_or_default()
            .to_string();
        if !is_name(&new_name) {
            return Err((
                INVALID_PARAMS,
                format!(
                    "`{new_name}` is not a name: names are letters, digits, and `_`, not starting with a digit, and no keyword"
                ),
            ));
        }
        let analysis = &self.analyses[&uri];
        let old_name = analysis.index.symbols[symbol].name.clone();
        let mut changes: Vec<(String, Vec<Json>)> = Vec::new();
        for span in analysis.index.references_to(symbol) {
            let file = analysis.sources.get(span.file);
            let written = file.slice(span);
            // A method is written without its type's prefix, as `canvas.fill`
            // calls `canvas_fill`; the new name keeps that form.
            let replacement = if written == old_name {
                new_name.clone()
            } else if let Some(prefix) = old_name.strip_suffix(written)
                && prefix.ends_with('_')
                && let Some(rest) = new_name.strip_prefix(prefix)
                && is_name(rest)
            {
                rest.to_string()
            } else {
                return Err((
                    REQUEST_FAILED,
                    format!(
                        "`{old_name}` is called as the method `{written}`, so its new name keeps its type's prefix, as `{}…`",
                        old_name.strip_suffix(written).unwrap_or("")
                    ),
                ));
            };
            let Some(path) = file.path.as_deref() else {
                continue;
            };
            let target = self.uri_for(path);
            let edit = Json::object(vec![
                ("range", range_json(&file.text, span)),
                ("newText", Json::String(replacement)),
            ]);
            match changes.iter_mut().find(|(u, _)| *u == target) {
                Some((_, edits)) => edits.push(edit),
                None => changes.push((target, vec![edit])),
            }
        }
        Ok(Json::object(vec![(
            "changes",
            Json::Object(
                changes
                    .into_iter()
                    .map(|(uri, edits)| (uri, Json::Array(edits)))
                    .collect(),
            ),
        )]))
    }

    fn completion(&mut self, params: &Json) -> Option<Json> {
        let (uri, offset) = self.at(params)?;
        let text = self.documents.get(&uri)?.text.clone();
        let analysis = self.fresh(&uri)?;
        Some(crate::complete::completion(analysis, &text, offset))
    }

    fn signature_help(&mut self, params: &Json) -> Option<Json> {
        let (uri, offset) = self.at(params)?;
        let text = self.documents.get(&uri)?.text.clone();
        let analysis = self.fresh(&uri)?;
        crate::complete::signature_help(analysis, &text, offset)
    }

    fn formatting(&mut self, params: &Json) -> Json {
        let uri = params
            .get("textDocument")
            .get("uri")
            .as_str()
            .unwrap_or_default();
        let Some(document) = self.documents.get(uri) else {
            return Json::Null;
        };
        // A document with syntax errors is left as it is.
        let Ok(formatted) = sayon_syntax::format::format_source(FileId(0), &document.text) else {
            return Json::Null;
        };
        if formatted == document.text {
            return Json::Array(Vec::new());
        }
        let whole = range_json(&document.text, Span::new(FileId(0), 0, document.text.len()));
        Json::Array(vec![Json::object(vec![
            ("range", whole),
            ("newText", Json::String(formatted)),
        ])])
    }
}

fn version(document: &Json) -> i64 {
    match document.get("version") {
        Json::Number(value) => *value as i64,
        _ => 0,
    }
}

/// The byte offset of a `{ line, character }` position.
fn range_offset(text: &str, place: &Json) -> usize {
    offset(
        text,
        place.get("line").as_u32().unwrap_or(0),
        place.get("character").as_u32().unwrap_or(0),
    )
}

fn range_json(text: &str, span: Span) -> Json {
    let point = |offset: u32| {
        let (line, character) = position(text, offset as usize);
        Json::object(vec![
            ("line", Json::number(line)),
            ("character", Json::number(character)),
        ])
    };
    Json::object(vec![("start", point(span.start)), ("end", point(span.end))])
}

/// What the server can do, as `initialize` answers.
fn capabilities() -> Json {
    Json::object(vec![
        (
            "capabilities",
            Json::object(vec![
                ("positionEncoding", Json::string("utf-16")),
                (
                    "textDocumentSync",
                    Json::object(vec![
                        ("openClose", Json::Bool(true)),
                        // The whole text with each change.
                        ("change", Json::number(1)),
                    ]),
                ),
                ("hoverProvider", Json::Bool(true)),
                ("definitionProvider", Json::Bool(true)),
                ("referencesProvider", Json::Bool(true)),
                (
                    "renameProvider",
                    Json::object(vec![("prepareProvider", Json::Bool(true))]),
                ),
                (
                    "completionProvider",
                    Json::object(vec![(
                        "triggerCharacters",
                        Json::Array(vec![
                            Json::string("."),
                            Json::string("/"),
                            Json::string("\""),
                        ]),
                    )]),
                ),
                (
                    "signatureHelpProvider",
                    Json::object(vec![
                        (
                            "triggerCharacters",
                            Json::Array(vec![Json::string("("), Json::string(",")]),
                        ),
                        ("retriggerCharacters", Json::Array(vec![Json::string(",")])),
                    ]),
                ),
                ("documentFormattingProvider", Json::Bool(true)),
            ]),
        ),
        (
            "serverInfo",
            Json::object(vec![
                ("name", Json::string("sayon")),
                ("version", Json::string(env!("CARGO_PKG_VERSION"))),
            ]),
        ),
    ])
}

/// Whether `text` can name something: letters, digits, and `_`, not
/// starting with a digit, and no keyword.
fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
        && sayon_syntax::token::Keyword::lookup(text).is_none()
}
