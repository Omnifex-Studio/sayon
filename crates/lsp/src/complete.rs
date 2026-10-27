//! Completion and signature help: what fits where the editor's cursor is.
//!
//! Completion reads the place first (`context`): nothing in comments and
//! strings, module paths in `import("...")`, variants after `case`, types
//! after `:`, `->`, and `<`, and members after a dot. Elsewhere it offers
//! the names in scope, the built-in functions, keywords with snippets of
//! their blocks, and the standard modules not imported yet, with their
//! imports. In a call's arguments, what fits the parameter's type comes
//! first: values of that type, also from the imported modules, the variants
//! of an expected enum, and the fields left to name in a structure being
//! built. Functions are inserted with placeholders for their arguments.

use crate::context::{Call, context};
use crate::json::Json;
use crate::server::Analysis;
use crate::text::position;
use sayon_check::index::{Owner, Param, Symbol, SymbolKind};
use sayon_syntax::SourceMap;
use std::collections::HashSet;
use std::path::Path;

/// The ordering of items, as their `sortText` begins: what fits the place
/// first, then locals, definitions, built-ins, keywords, and modules to
/// import.
const FITS: &str = "0";
const LOCAL: &str = "1";
const DEFINED: &str = "2";
const BUILT_IN: &str = "3";
const KEYWORD: &str = "4";
const TO_IMPORT: &str = "5";

/// The blocks keywords start, as snippets: the keyword, the text, and what
/// it makes.
const SNIPPETS: &[(&str, &str, &str)] = &[
    ("function", "function ${1:name}(${2})\n\t$0", "a function"),
    ("if", "if ${1:condition}\n\t$0", "an `if` block"),
    (
        "elseif",
        "elseif ${1:condition}\n\t$0",
        "an `elseif` branch",
    ),
    ("else", "else\n\t$0", "an `else` branch"),
    ("for", "for ${1:item} in ${2:items}\n\t$0", "a `for` loop"),
    ("while", "while ${1:condition}\n\t$0", "a `while` loop"),
    (
        "match",
        "match ${1:value}\n\tcase ${2:pattern}\n\t\t$0",
        "a `match` with a case",
    ),
    (
        "try",
        "try\n\t$1\ncatch ${2:problem}\n\t$0",
        "a `try` with a `catch`",
    ),
    (
        "type",
        "type ${1:Name} = [${2:field}: ${3:integer}]",
        "a structure",
    ),
    ("enum", "enum ${1:Name}\n\t${2:variant}", "an enum"),
    (
        "import",
        "${1:name} = import(\"${2:standard/strings.sayon}\")",
        "an import",
    ),
    ("test", "test \"${1:what it checks}\"\n\t$0", "a test block"),
];

/// The types every module can name, besides the declared ones.
const TYPES: &[&str] = &[
    "integer",
    "float",
    "string",
    "boolean",
    "byte",
    "nothing",
    "unsigned",
    "Array",
    "Dict",
    "View",
    "MutableView",
    "Shared",
    "Weak",
    "Cell",
    "Mutex",
    "Outcome",
    "Error",
    "Range",
    "integer8",
    "integer16",
    "integer32",
    "integer64",
    "unsigned8",
    "unsigned16",
    "unsigned32",
    "unsigned64",
    "float32",
    "float64",
    "pointer",
    "c_string",
];

/// Where the name that ends `text` starts.
pub fn name_start(text: &str) -> usize {
    text.char_indices()
        .rev()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
        .map_or(0, |(index, c)| index + c.len_utf8())
}

/// The comment lines just above a definition, which describe it.
pub fn documentation(sources: &SourceMap, symbol: &Symbol) -> Option<String> {
    if matches!(
        symbol.kind,
        SymbolKind::Variable | SymbolKind::Parameter | SymbolKind::Module
    ) {
        return None;
    }
    let file = sources.get(symbol.span.file);
    let mut line = file.line_index(symbol.span.start);
    let mut lines = Vec::new();
    while line > 0 {
        line -= 1;
        let text = file.line_text(line).trim_start();
        let Some(comment) = text.strip_prefix('#') else {
            break;
        };
        lines.push(comment.strip_prefix(' ').unwrap_or(comment).to_string());
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

/// A snippet's text with `$`, `}`, and `\` escaped.
fn escaped(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('$', "\\$")
        .replace('}', "\\}")
}

/// The locals the place can see: those of the bodies around it declared
/// before it, or the script's top level's outside every body.
fn visible_locals(analysis: &Analysis, offset: usize) -> Vec<usize> {
    let index = &analysis.index;
    let file = analysis.file;
    let inside: Vec<_> = index
        .locals
        .iter()
        .filter(|scope| scope.file == file)
        .filter(|scope| {
            scope
                .body
                .is_some_and(|b| b.start as usize <= offset && offset <= b.end as usize)
        })
        .collect();
    let scopes: Vec<_> = if inside.is_empty() {
        index
            .locals
            .iter()
            .filter(|scope| scope.file == file && scope.body.is_none())
            .collect()
    } else {
        inside
    };
    scopes
        .into_iter()
        .flat_map(|scope| scope.locals.iter().copied())
        .filter(|&s| (index.symbols[s].span.start as usize) < offset)
        .collect()
}

/// What `name` means, written before `end`: what the nearest such name
/// before it refers to, or else a definition of the file or a visible local
/// by that name.
fn symbol_named(analysis: &Analysis, name: &str, end: usize) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    let file = analysis.sources.get(analysis.file);
    let index = &analysis.index;
    // A name after a dot is a member of something else, as `canvas` in
    // `graphics.canvas(...)`.
    let member = |start: u32| file.text[..start as usize].trim_end().ends_with('.');
    if let Some(reference) = index
        .references
        .iter()
        .filter(|r| r.span.file == analysis.file && r.span.end as usize <= end)
        .rfind(|r| file.slice(r.span) == name && !member(r.span.start))
    {
        return Some(reference.symbol);
    }
    index
        .scopes
        .get(&analysis.file)
        .into_iter()
        .flatten()
        .copied()
        .find(|&s| index.symbols[s].name == name)
        .or_else(|| {
            visible_locals(analysis, end)
                .into_iter()
                .find(|&s| index.symbols[s].name == name)
        })
}

/// What names joined by dots, ending at `end`, mean, and whether the last
/// is a method of the value before its dot.
fn resolve(analysis: &Analysis, chain: &str, end: usize) -> Option<(usize, bool)> {
    let index = &analysis.index;
    let parts: Vec<&str> = chain.split('.').collect();
    let first = symbol_named(analysis, parts[0], end)?;
    let value_first = !matches!(
        index.symbols[first].kind,
        SymbolKind::Module | SymbolKind::Type
    );
    // What the last name was resolved to, where it is written.
    if let Some(reference) = index.at(analysis.file, end as u32)
        && analysis.sources.get(analysis.file).slice(reference.span) == *parts.last()?
    {
        let found = &index.symbols[reference.symbol];
        let method = parts.len() > 1 && found.kind == SymbolKind::Function && value_first;
        return Some((reference.symbol, method));
    }
    let mut symbol = first;
    let mut method = false;
    for part in &parts[1..] {
        let found = &index.symbols[symbol];
        method = !matches!(found.kind, SymbolKind::Module | SymbolKind::Type);
        let owner = found.members?;
        symbol = index
            .members
            .get(&owner)?
            .iter()
            .find(|(name, _)| name == part)
            .map(|(_, s)| *s)?;
    }
    let method = method && index.symbols[symbol].kind == SymbolKind::Function;
    Some((symbol, method))
}

/// The parameters a call of a symbol writes: a method's but the value
/// before its dot.
fn written_params(symbol: &Symbol, method: bool) -> &[Param] {
    if method && !symbol.params.is_empty() {
        &symbol.params[1..]
    } else {
        &symbol.params
    }
}

/// The parameter the argument at a call's place is for.
fn parameter<'s>(symbol: &'s Symbol, method: bool, call: &Call) -> Option<(usize, &'s Param)> {
    let params = written_params(symbol, method);
    match &call.naming {
        Some(name) => params.iter().enumerate().find(|(_, p)| &p.name == name),
        None => params.get(call.argument).map(|p| (call.argument, p)),
    }
}

/// The protocol's kind of completion item for a symbol.
fn kind_of(symbol: &Symbol, method: bool) -> u32 {
    match symbol.kind {
        SymbolKind::Variable | SymbolKind::Parameter => 6,
        SymbolKind::Constant => 21,
        SymbolKind::Function if method => 2,
        SymbolKind::Function => 3,
        SymbolKind::Type if symbol.detail.starts_with("enum") => 13,
        SymbolKind::Type => 22,
        SymbolKind::Field => 5,
        SymbolKind::Variant => 20,
        SymbolKind::Module => 9,
    }
}

/// A completion item being made.
struct Item {
    label: String,
    kind: u32,
    sort: String,
    detail: Option<String>,
    documentation: Option<String>,
    insert: Option<String>,
    snippet: bool,
    /// Show the parameters once it is inserted.
    hints: bool,
    /// What it replaces, when not the name the cursor is in: a byte range.
    replaces: Option<(usize, usize)>,
    edits: Vec<Json>,
    preselect: bool,
}

impl Item {
    fn new(label: impl Into<String>, kind: u32, sort: &str) -> Item {
        let label = label.into();
        Item {
            sort: format!("{sort}{label}"),
            label,
            kind,
            detail: None,
            documentation: None,
            insert: None,
            snippet: false,
            hints: false,
            replaces: None,
            edits: Vec::new(),
            preselect: false,
        }
    }

    fn json(self, text: &str) -> Json {
        let mut members = vec![
            ("label", Json::String(self.label.clone())),
            ("kind", Json::number(self.kind)),
            ("sortText", Json::String(self.sort)),
        ];
        if let Some(detail) = self.detail {
            members.push(("detail", Json::String(detail)));
        }
        if let Some(doc) = self.documentation {
            members.push((
                "documentation",
                Json::object(vec![
                    ("kind", Json::string("markdown")),
                    ("value", Json::String(doc)),
                ]),
            ));
        }
        let insert = self.insert.unwrap_or_else(|| self.label.clone());
        if let Some((start, end)) = self.replaces {
            members.push((
                "textEdit",
                Json::object(vec![
                    ("range", range_of(text, start, end)),
                    ("newText", Json::String(insert)),
                ]),
            ));
            members.push(("filterText", Json::String(self.label)));
        } else {
            members.push(("insertText", Json::String(insert)));
        }
        if self.snippet {
            members.push(("insertTextFormat", Json::number(2)));
        }
        if self.hints {
            members.push((
                "command",
                Json::object(vec![
                    ("title", Json::string("Show the parameters")),
                    (
                        "command",
                        Json::string("editor.action.triggerParameterHints"),
                    ),
                ]),
            ));
        }
        if !self.edits.is_empty() {
            members.push(("additionalTextEdits", Json::Array(self.edits)));
        }
        if self.preselect {
            members.push(("preselect", Json::Bool(true)));
        }
        Json::object(members)
    }
}

fn point(text: &str, offset: usize) -> Json {
    let (line, character) = position(text, offset);
    Json::object(vec![
        ("line", Json::number(line)),
        ("character", Json::number(character)),
    ])
}

fn range_of(text: &str, start: usize, end: usize) -> Json {
    Json::object(vec![
        ("start", point(text, start)),
        ("end", point(text, end)),
    ])
}

/// Whether a name at the end of `head`, the line's text before it, is in a
/// type: after `:` or `->`, or among a type's arguments, after `Array<` or a
/// comma inside such brackets.
fn type_position(head: &str) -> bool {
    let head = head.trim_end();
    if head.ends_with("->") || (head.ends_with(':') && !head.ends_with("::")) {
        return true;
    }
    // An unclosed `<` right after a name.
    let mut depth = 0usize;
    for (index, c) in head.char_indices().rev() {
        match c {
            '>' => depth += 1,
            '<' if depth > 0 => depth -= 1,
            '<' => {
                return head[..index]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            }
            '(' | ')' | '=' => return false,
            _ => {}
        }
    }
    false
}

/// The line new imports go on: after the last import, or else after the
/// comments the file starts with.
fn import_line(text: &str) -> usize {
    let lines: Vec<&str> = text.split('\n').collect();
    let import = |line: &str| {
        let line = line.trim_start();
        line.split_once(" = import(")
            .is_some_and(|(name, _)| name.chars().all(|c| c.is_alphanumeric() || c == '_'))
    };
    if let Some(last) = lines.iter().rposition(|l| import(l)) {
        return last + 1;
    }
    let comments = lines
        .iter()
        .take_while(|l| l.trim_start().starts_with('#'))
        .count();
    if comments > 0 && lines.get(comments).is_some_and(|l| l.trim().is_empty()) {
        comments + 1
    } else {
        comments
    }
}

/// The `#` lines a module's file starts with, which say what it is.
fn module_summary(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<String> = text
        .lines()
        .take_while(|l| l.starts_with('#'))
        .take(8)
        .map(|l| l.trim_start_matches('#').trim_start().to_string())
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The standard library's modules, by their names.
fn standard_modules(analysis: &Analysis) -> Vec<(String, std::path::PathBuf)> {
    let Some(dir) = &analysis.stdlib else {
        return Vec::new();
    };
    let mut modules: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "sayon"))
        .filter_map(|path| Some((path.file_stem()?.to_string_lossy().to_string(), path)))
        .collect();
    modules.sort();
    modules
}

struct Completer<'a> {
    analysis: &'a Analysis,
    text: &'a str,
    offset: usize,
    /// Whether a call's `(` already follows the name at the place.
    called: bool,
    /// Whether the functions being offered are methods, after a value's
    /// dot.
    methods: bool,
    /// The type of the parameter the argument at the place is for.
    expected: Option<(String, Option<Owner>)>,
    seen: HashSet<String>,
    items: Vec<Item>,
}

impl Completer<'_> {
    fn symbol(&self, symbol: usize) -> &Symbol {
        &self.analysis.index.symbols[symbol]
    }

    fn fits(&self, symbol: &Symbol) -> bool {
        matches!((&self.expected, &symbol.ty), (Some((expected, _)), Some(ty)) if expected == ty)
    }

    /// An item for a symbol, written as `label`, in `sort`'s place, or
    /// first when it fits: a function with placeholders for its arguments.
    fn item(&self, label: &str, symbol: usize, sort: &str) -> Item {
        let found = self.symbol(symbol);
        let fits = self.fits(found)
            && !matches!(
                found.kind,
                SymbolKind::Type | SymbolKind::Module | SymbolKind::Field
            );
        let method = self.methods && found.kind == SymbolKind::Function;
        let mut item = Item::new(
            label,
            kind_of(found, method),
            if fits { FITS } else { sort },
        );
        item.detail = Some(found.detail.clone());
        item.documentation = documentation(&self.analysis.sources, found);
        item.preselect = fits;
        let callable = found.kind == SymbolKind::Function
            || (found.kind == SymbolKind::Variant && !found.params.is_empty());
        if callable && !self.called {
            let params = written_params(found, method);
            let required: Vec<String> = params
                .iter()
                .filter(|p| !p.default)
                .enumerate()
                .map(|(n, p)| format!("${{{}:{}}}", n + 1, escaped(&p.name)))
                .collect();
            let arguments = if required.is_empty() && !params.is_empty() {
                "$1".to_string()
            } else {
                required.join(", ")
            };
            item.insert = Some(format!("{}({arguments})", escaped(label)));
            item.snippet = true;
            item.hints = !params.is_empty();
        }
        item
    }

    fn push(&mut self, item: Item) {
        if self.seen.insert(item.label.clone()) {
            self.items.push(item);
        }
    }

    fn finish(self) -> Json {
        let text = self.text;
        Json::object(vec![
            ("isIncomplete", Json::Bool(false)),
            (
                "items",
                Json::Array(self.items.into_iter().map(|i| i.json(text)).collect()),
            ),
        ])
    }

    /// The modules an import's path can name: the standard library's, and
    /// the program's own files under its root.
    fn import_paths(&mut self, start: usize) {
        for (name, path) in standard_modules(self.analysis) {
            let mut item = Item::new(format!("standard/{name}.sayon"), 17, BUILT_IN);
            item.documentation = module_summary(&path);
            item.replaces = Some((start, self.offset));
            self.push(item);
        }
        let Some(root) = self.analysis.root.clone() else {
            return;
        };
        let mut found = Vec::new();
        files_under(&root, &root, 0, &mut found);
        let own = self.analysis.sources.get(self.analysis.file).path.clone();
        for (relative, path) in found {
            if own
                .as_deref()
                .is_some_and(|own| sayon_driver::canonical(own) == sayon_driver::canonical(&path))
            {
                continue;
            }
            let mut item = Item::new(relative, 17, DEFINED);
            item.replaces = Some((start, self.offset));
            self.push(item);
        }
    }

    /// The members of what is before the dot at `dot`: a module's
    /// definitions, a value's fields and methods, or an enum's variants; in a
    /// type, only types.
    fn members(&mut self, dot: usize, in_type: bool) {
        let receiver_start = name_start(&self.text[..dot]);
        let receiver = &self.text[receiver_start..dot];
        let Some((symbol, _)) = resolve(self.analysis, receiver, dot) else {
            return;
        };
        let found = self.symbol(symbol).clone();
        let Some(owner) = found.members else {
            return;
        };
        self.methods = !matches!(found.kind, SymbolKind::Module | SymbolKind::Type);
        let members = self
            .analysis
            .index
            .members
            .get(&owner)
            .cloned()
            .unwrap_or_default();
        for (name, member) in members {
            let kind = self.symbol(member).kind;
            if in_type && kind != SymbolKind::Type {
                continue;
            }
            if found.kind == SymbolKind::Type && kind != SymbolKind::Variant {
                continue;
            }
            let item = self.item(&name, member, DEFINED);
            self.push(item);
        }
    }

    /// The types a type can name.
    fn types(&mut self) {
        let scope = self
            .analysis
            .index
            .scopes
            .get(&self.analysis.file)
            .cloned()
            .unwrap_or_default();
        for symbol in scope {
            if matches!(
                self.symbol(symbol).kind,
                SymbolKind::Type | SymbolKind::Module
            ) {
                let name = self.symbol(symbol).name.clone();
                let item = self.item(&name, symbol, DEFINED);
                self.push(item);
            }
        }
        for name in TYPES {
            let mut item = Item::new(*name, 7, BUILT_IN);
            item.detail = Some("built in".to_string());
            self.push(item);
        }
    }

    /// The variants of what the `match` around the place matches, for a
    /// `case`.
    fn cases(&mut self, line_begin: usize) {
        let indent = |line: &str| line.len() - line.trim_start().len();
        let here = indent(&self.text[line_begin..self.offset]);
        let mut end = line_begin;
        let mut scrutinee = None;
        while end > 0 {
            let begin = self.text[..end - 1].rfind('\n').map_or(0, |n| n + 1);
            let line = &self.text[begin..end - 1];
            if indent(line) < here
                && let Some(rest) = line.trim_start().strip_prefix("match ")
            {
                let value = rest.trim_end();
                let value_end = begin + indent(line) + "match ".len() + value.len();
                scrutinee = Some((value.to_string(), value_end));
                break;
            }
            end = begin;
        }
        if let Some((value, value_end)) = scrutinee
            && let Some((symbol, _)) = resolve(self.analysis, &value, value_end)
            && let Some(owner) = self.symbol(symbol).members
        {
            let members = self
                .analysis
                .index
                .members
                .get(&owner)
                .cloned()
                .unwrap_or_default();
            for (name, member) in members {
                if self.symbol(member).kind == SymbolKind::Variant {
                    let item = self.item(&name, member, FITS);
                    self.push(item);
                }
            }
        }
        let mut item = Item::new("_", 14, KEYWORD);
        item.detail = Some("any other value".to_string());
        self.push(item);
    }

    /// What fits the argument at the place: the fields left to name in a
    /// structure being built, the variants of an expected enum, `true` and
    /// `false`, and values of the expected type from the imported modules.
    fn arguments(&mut self, call: &Call) {
        let Some((symbol, method)) = resolve(self.analysis, &call.callee, call.callee_end) else {
            return;
        };
        let found = self.symbol(symbol).clone();
        if found.kind == SymbolKind::Type && call.naming.is_none() {
            for param in &found.params {
                if call.named.contains(&param.name) {
                    continue;
                }
                let mut item = Item::new(param.name.clone(), 5, FITS);
                item.detail = Some(format!("{}: {}", param.name, param.ty));
                item.insert = Some(format!("{} = ", param.name));
                self.push(item);
            }
        }
        let Some((_, param)) = parameter(&found, method, call) else {
            return;
        };
        self.expected = Some((param.ty.clone(), param.owner));
        if let Some(owner @ Owner::Type(_)) = param.owner {
            let members = self
                .analysis
                .index
                .members
                .get(&owner)
                .cloned()
                .unwrap_or_default();
            for (name, member) in members {
                if self.symbol(member).kind == SymbolKind::Variant {
                    let item = self.item(&name, member, FITS);
                    self.push(item);
                }
            }
        }
        if param.ty == "boolean" {
            for word in ["true", "false"] {
                let mut item = Item::new(word, 14, FITS);
                item.preselect = word == "true";
                self.push(item);
            }
        }
        // Values of the type from the imported modules, as `graphics.BLACK`.
        let scope = self
            .analysis
            .index
            .scopes
            .get(&self.analysis.file)
            .cloned()
            .unwrap_or_default();
        let mut offered = 0;
        for alias in scope {
            let alias_symbol = self.symbol(alias).clone();
            let (SymbolKind::Module, Some(owner)) = (alias_symbol.kind, alias_symbol.members)
            else {
                continue;
            };
            let members = self
                .analysis
                .index
                .members
                .get(&owner)
                .cloned()
                .unwrap_or_default();
            for (name, member) in members {
                let candidate = self.symbol(member);
                if offered < 40
                    && matches!(candidate.kind, SymbolKind::Constant | SymbolKind::Function)
                    && self.fits(candidate)
                {
                    let item = self.item(&format!("{}.{name}", alias_symbol.name), member, FITS);
                    self.push(item);
                    offered += 1;
                }
            }
        }
    }

    /// The names a place in code can write: locals, the file's definitions
    /// and imports, built-in functions, keywords, and modules to import.
    fn names(&mut self) {
        for symbol in visible_locals(self.analysis, self.offset) {
            let name = self.symbol(symbol).name.clone();
            let item = self.item(&name, symbol, LOCAL);
            self.push(item);
        }
        let scope = self
            .analysis
            .index
            .scopes
            .get(&self.analysis.file)
            .cloned()
            .unwrap_or_default();
        for symbol in scope {
            let name = self.symbol(symbol).name.clone();
            let item = self.item(&name, symbol, DEFINED);
            self.push(item);
        }
        for name in sayon_check::builtins::NAMES {
            let mut item = Item::new(*name, 3, BUILT_IN);
            item.detail = Some("built in".to_string());
            if !self.called {
                item.insert = Some(format!("{name}($1)"));
                item.snippet = true;
            }
            self.push(item);
        }
        for (keyword, body, what) in SNIPPETS {
            let mut item = Item::new(*keyword, 15, KEYWORD);
            item.detail = Some(what.to_string());
            item.insert = Some(body.to_string());
            item.snippet = true;
            self.push(item);
        }
        for keyword in sayon_syntax::token::Keyword::ALL {
            self.push(Item::new(keyword.as_str(), 14, KEYWORD));
        }
        // The standard modules this file does not import, with their imports.
        let line = import_line(self.text);
        let at = point(self.text, line_offset(self.text, line));
        for (name, path) in standard_modules(self.analysis) {
            let written = format!("\"standard/{name}.sayon\"");
            if self.seen.contains(&name) || self.text.contains(&written) {
                continue;
            }
            let mut item = Item::new(name.clone(), 9, TO_IMPORT);
            item.detail = Some(format!("{name} = import({written}), added at the top"));
            item.documentation = module_summary(&path);
            item.edits.push(Json::object(vec![
                (
                    "range",
                    Json::object(vec![("start", at.clone()), ("end", at.clone())]),
                ),
                (
                    "newText",
                    Json::String(format!("{name} = import({written})\n")),
                ),
            ]));
            self.push(item);
        }
    }
}

/// The byte offset where a line starts.
fn line_offset(text: &str, line: usize) -> usize {
    text.split_inclusive('\n').take(line).map(str::len).sum()
}

/// The `.sayon` files under a directory, as paths from `root` with `/`, to
/// four levels deep, leaving out build output and hidden directories.
fn files_under(root: &Path, dir: &Path, depth: u32, found: &mut Vec<(String, std::path::PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if path.is_dir() {
            if depth < 4 && !name.starts_with('.') && name != "build" && name != "target" {
                files_under(root, &path, depth + 1, found);
            }
        } else if name.ends_with(".sayon")
            && found.len() < 200
            && let Ok(relative) = path.strip_prefix(root)
        {
            let relative = relative.to_string_lossy().replace('\\', "/");
            found.push((relative, path));
        }
    }
}

/// What can be written at a place in a document.
pub fn completion(analysis: &Analysis, text: &str, offset: usize) -> Json {
    let place = context(text, offset);
    let after = text[offset..].trim_start_matches(|c: char| c.is_alphanumeric() || c == '_');
    let mut completer = Completer {
        analysis,
        text,
        offset,
        called: after.starts_with('('),
        methods: false,
        expected: None,
        seen: HashSet::new(),
        items: Vec::new(),
    };
    if place.comment {
        return completer.finish();
    }
    if let Some(start) = place.import_path {
        completer.import_paths(start);
        return completer.finish();
    }
    if place.string {
        return completer.finish();
    }
    let before = &text[..offset];
    let start = name_start(before);
    let line_begin = before[..start].rfind('\n').map_or(0, |n| n + 1);
    let head = &before[line_begin..start];
    if head.trim_start() == "case " {
        completer.cases(line_begin);
        return completer.finish();
    }
    if before[..start].ends_with('.') {
        let receiver = name_start(&before[..start - 1]);
        let in_type = type_position(&before[line_begin.min(receiver)..receiver]);
        completer.members(start - 1, in_type);
        return completer.finish();
    }
    if type_position(head) {
        completer.types();
        return completer.finish();
    }
    if let Some(call) = place.calls.last() {
        completer.arguments(call);
    }
    completer.names();
    completer.finish()
}

/// The signature of the call whose arguments hold a place, its parameter
/// there marked.
pub fn signature_help(analysis: &Analysis, text: &str, offset: usize) -> Option<Json> {
    let place = context(text, offset);
    if place.comment || place.string {
        return None;
    }
    let call = place.calls.last()?;
    let (symbol, method) = resolve(analysis, &call.callee, call.callee_end)?;
    let found = &analysis.index.symbols[symbol];
    if !matches!(
        found.kind,
        SymbolKind::Function | SymbolKind::Type | SymbolKind::Variant
    ) {
        return None;
    }
    let params = written_params(found, method);
    let units = |text: &str| text.encode_utf16().count();
    let mut label = format!("{}(", call.callee);
    let mut ranges = Vec::new();
    for (index, param) in params.iter().enumerate() {
        if index > 0 {
            label.push_str(", ");
        }
        let start = units(&label);
        if let Some(mode) = param.mode {
            label.push_str(mode);
            label.push(' ');
        }
        label.push_str(&format!("{}: {}", param.name, param.ty));
        if param.default {
            label.push_str(" = …");
        }
        ranges.push(Json::object(vec![(
            "label",
            Json::Array(vec![
                Json::number(start as f64),
                Json::number(units(&label) as f64),
            ]),
        )]));
    }
    label.push(')');
    if found.kind == SymbolKind::Function
        && let Some(ty) = &found.ty
        && ty != "nothing"
    {
        label.push_str(&format!(" -> {ty}"));
    }
    let active = parameter(found, method, call).map_or(call.argument, |(index, _)| index);
    let mut signature = vec![
        ("label", Json::String(label)),
        ("parameters", Json::Array(ranges)),
    ];
    if let Some(doc) = documentation(&analysis.sources, found) {
        signature.push((
            "documentation",
            Json::object(vec![
                ("kind", Json::string("markdown")),
                ("value", Json::String(doc)),
            ]),
        ));
    }
    Some(Json::object(vec![
        ("signatures", Json::Array(vec![Json::object(signature)])),
        ("activeSignature", Json::number(0)),
        ("activeParameter", Json::number(active as f64)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_are_written_after_colons_arrows_and_type_arguments() {
        assert!(type_position("function f(x: "));
        assert!(type_position("function f(x) -> "));
        assert!(type_position("items: Array<"));
        assert!(type_position("table: Dict<string, "));
        assert!(!type_position("if count < "));
        assert!(!type_position("x = "));
        assert!(!type_position("print(a, "));
    }

    #[test]
    fn imports_go_after_the_last_import_or_the_first_comments() {
        assert_eq!(import_line("# A program.\n\nx = 1\n"), 2);
        assert_eq!(import_line("a = import(\"a.sayon\")\nb = 2\n"), 1);
        assert_eq!(import_line("print(1)\n"), 0);
    }
}
