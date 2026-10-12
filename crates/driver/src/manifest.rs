//! Project manifests (`sayon.toml`).
//!
//! The manifest is read with a strict reader for the subset of TOML it
//! needs: `[table]` headers, `key = value` lines, and string, integer,
//! boolean, and single-line string array values. Unknown tables and keys are
//! errors, so a misspelling is reported instead of silently ignored.
//!
//! ```toml
//! [package]
//! name = "hello"
//! version = "0.1.0"
//! entry = "src/main.sayon"      # optional; this is the default
//! authors = ["Ada"]             # optional
//! description = "A greeting"    # optional
//! language = "1"                # optional
//! ```

use sayon_syntax::{Diagnostic, FileId, Span};

pub const FILE_NAME: &str = "sayon.toml";

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub entry: String,
    pub authors: Vec<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    String(String),
    Integer(i64),
    Boolean(bool),
    Strings(Vec<String>),
}

impl Value {
    fn kind(&self) -> &'static str {
        match self {
            Value::String(_) => "a string",
            Value::Integer(_) => "an integer",
            Value::Boolean(_) => "a boolean",
            Value::Strings(_) => "an array of strings",
        }
    }
}

struct Entry {
    table: String,
    key: String,
    key_span: Span,
    value: Value,
    value_span: Span,
}

const PACKAGE_KEYS: [&str; 6] = [
    "name",
    "version",
    "entry",
    "authors",
    "description",
    "language",
];
const LATER_TABLES: [(&str, &str); 3] = [
    ("dependencies", "M2"),
    ("dev_dependencies", "M2"),
    ("compiler", "M4"),
];

/// Reads a manifest, reporting every problem found.
pub fn parse(file: FileId, text: &str) -> Result<Manifest, Vec<Diagnostic>> {
    let mut errors = Vec::new();
    let entries = read_entries(file, text, &mut errors);
    let mut name = None;
    let mut version = None;
    let mut entry = None;
    let mut authors = Vec::new();
    let mut description = None;
    let mut seen: Vec<(String, String, Span)> = Vec::new();

    for item in &entries {
        if let Some((_, _, first)) = seen
            .iter()
            .find(|(t, k, _)| *t == item.table && *k == item.key)
        {
            errors.push(
                Diagnostic::error("E_MANIFEST", format!("`{}` is set twice", item.key))
                    .primary(item.key_span, "set again here")
                    .secondary(*first, "first set here"),
            );
            continue;
        }
        seen.push((item.table.clone(), item.key.clone(), item.key_span));
        if item.table != "package" {
            continue;
        }
        let expect_string = |errors: &mut Vec<Diagnostic>| -> Option<String> {
            match &item.value {
                Value::String(text) => Some(text.clone()),
                other => {
                    errors.push(
                        Diagnostic::error(
                            "E_MANIFEST",
                            format!("`{}` must be a string, not {}", item.key, other.kind()),
                        )
                        .primary(item.value_span, ""),
                    );
                    None
                }
            }
        };
        match item.key.as_str() {
            "name" => {
                if let Some(text) = expect_string(&mut errors) {
                    let valid = !text.is_empty()
                        && text.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                        && text
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                    if !valid {
                        errors.push(
                            Diagnostic::error("E_MANIFEST", format!("\"{text}\" is not a valid package name"))
                                .primary(item.value_span, "")
                                .help("use lowercase letters, digits, and underscores, starting with a letter"),
                        );
                    }
                    name = Some(text);
                }
            }
            "version" => {
                if let Some(text) = expect_string(&mut errors) {
                    let parts: Vec<&str> = text.split('.').collect();
                    let valid = parts.len() == 3
                        && parts
                            .iter()
                            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
                    if !valid {
                        errors.push(
                            Diagnostic::error("E_MANIFEST", format!("\"{text}\" is not a version"))
                                .primary(item.value_span, "")
                                .help("versions have three numbers: major.minor.patch, such as \"0.1.0\""),
                        );
                    }
                    version = Some(text);
                }
            }
            "entry" => {
                if let Some(text) = expect_string(&mut errors) {
                    entry = Some(text);
                }
            }
            "description" => description = expect_string(&mut errors),
            "language" => {
                if let Some(text) = expect_string(&mut errors) {
                    if text != "1" {
                        errors.push(
                            Diagnostic::error(
                                "E_MANIFEST",
                                format!("language edition \"{text}\" is not supported"),
                            )
                            .primary(item.value_span, "")
                            .help("this release supports edition \"1\""),
                        );
                    }
                }
            }
            "authors" => match &item.value {
                Value::Strings(list) => authors = list.clone(),
                other => errors.push(
                    Diagnostic::error(
                        "E_MANIFEST",
                        format!(
                            "`authors` must be an array of strings, not {}",
                            other.kind()
                        ),
                    )
                    .primary(item.value_span, ""),
                ),
            },
            other => {
                let mut diagnostic =
                    Diagnostic::error("E_MANIFEST", format!("unknown key `{other}` in [package]"))
                        .primary(item.key_span, "");
                if let Some(suggestion) = PACKAGE_KEYS.iter().find(|k| close(other, k)) {
                    diagnostic = diagnostic.help(format!("did you mean `{suggestion}`?"));
                } else {
                    diagnostic =
                        diagnostic.help(format!("[package] accepts {}", PACKAGE_KEYS.join(", ")));
                }
                errors.push(diagnostic);
            }
        }
    }

    let end = Span::new(file, text.len(), text.len());
    if name.is_none() && errors.is_empty() {
        errors.push(
            Diagnostic::error("E_MANIFEST", "the manifest needs a package name")
                .primary(end, "")
                .help("add `name = \"my_project\"` under [package]"),
        );
    }
    if version.is_none() && errors.is_empty() {
        errors.push(
            Diagnostic::error("E_MANIFEST", "the manifest needs a package version")
                .primary(end, "")
                .help("add `version = \"0.1.0\"` under [package]"),
        );
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Manifest {
        name: name.unwrap_or_default(),
        version: version.unwrap_or_default(),
        entry: entry.unwrap_or_else(|| "src/main.sayon".to_string()),
        authors,
        description,
    })
}

fn close(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut current = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            current[j] = (previous[j] + 1)
                .min(current[j - 1] + 1)
                .min(previous[j - 1] + cost);
        }
        previous = current;
    }
    previous[b.len()] <= 2
}

fn read_entries(file: FileId, text: &str, errors: &mut Vec<Diagnostic>) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut table: Option<String> = None;
    let mut offset = 0;
    for raw in text.split('\n') {
        let line_start = offset;
        offset += raw.len() + 1;
        let line = raw.trim_end_matches('\r');
        let content = strip_comment(line);
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = content.len() - content.trim_start().len();
        let span_of = |start: usize, len: usize| {
            Span::new(file, line_start + start, line_start + start + len)
        };
        if let Some(header) = trimmed.strip_prefix('[') {
            let Some(name) = header.strip_suffix(']') else {
                errors.push(
                    Diagnostic::error("E_MANIFEST", "a table header ends with `]`")
                        .primary(span_of(indent, trimmed.len()), ""),
                );
                table = Some(String::new());
                continue;
            };
            let name = name.trim().to_string();
            let span = span_of(indent, trimmed.len());
            if name == "package" {
                table = Some(name);
            } else if let Some((_, milestone)) = LATER_TABLES.iter().find(|(t, _)| *t == name) {
                errors.push(
                    Diagnostic::error("E_UNAVAILABLE", format!("[{name}] is not available yet"))
                        .primary(span, format!("planned for milestone {milestone}")),
                );
                table = Some(String::new());
            } else {
                errors.push(
                    Diagnostic::error("E_MANIFEST", format!("unknown table [{name}]"))
                        .primary(span, "")
                        .help("the manifest has a [package] table"),
                );
                table = Some(String::new());
            }
            continue;
        }
        let Some(equals) = content.find('=') else {
            errors.push(
                Diagnostic::error("E_MANIFEST", "expected `key = value`")
                    .primary(span_of(indent, trimmed.len()), ""),
            );
            continue;
        };
        let key = content[..equals].trim();
        let key_span = span_of(indent, key.len());
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            errors.push(Diagnostic::error("E_MANIFEST", "invalid key").primary(key_span, ""));
            continue;
        }
        let value_text = content[equals + 1..].trim();
        let value_start =
            equals + 1 + (content[equals + 1..].len() - content[equals + 1..].trim_start().len());
        let value_span = span_of(value_start, value_text.len().max(1));
        let Some(current) = table.clone() else {
            errors.push(
                Diagnostic::error("E_MANIFEST", format!("`{key}` must be inside a table"))
                    .primary(key_span, "")
                    .help("start the manifest with a [package] line"),
            );
            continue;
        };
        if current.is_empty() {
            continue;
        }
        match parse_value(value_text) {
            Ok(value) => entries.push(Entry {
                table: current,
                key: key.to_string(),
                key_span,
                value,
                value_span,
            }),
            Err(message) => {
                errors.push(Diagnostic::error("E_MANIFEST", message).primary(value_span, ""))
            }
        }
    }
    entries
}

/// Removes a `#` comment that is not inside a string.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (index, c) in line.char_indices() {
        match c {
            '\\' if in_string && !escaped => {
                escaped = true;
                continue;
            }
            '"' if !escaped => in_string = !in_string,
            '#' if !in_string => return &line[..index],
            _ => {}
        }
        escaped = false;
    }
    line
}

fn parse_string(text: &str) -> Result<(String, &str), String> {
    let mut chars = text.char_indices();
    if chars.next().map(|(_, c)| c) != Some('"') {
        return Err("expected a string in double quotes".to_string());
    }
    let mut out = String::new();
    while let Some((index, c)) = chars.next() {
        match c {
            '"' => return Ok((out, &text[index + 1..])),
            '\\' => match chars.next().map(|(_, c)| c) {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => return Err(format!("unknown escape `\\{other}`")),
                None => return Err("unterminated string".to_string()),
            },
            c => out.push(c),
        }
    }
    Err("unterminated string".to_string())
}

fn parse_value(text: &str) -> Result<Value, String> {
    if text.starts_with('"') {
        let (value, rest) = parse_string(text)?;
        if !rest.trim().is_empty() {
            return Err("unexpected text after the string".to_string());
        }
        return Ok(Value::String(value));
    }
    if let Some(inner) = text.strip_prefix('[') {
        let Some(inner) = inner.trim_end().strip_suffix(']') else {
            return Err("arrays must close on the same line with `]`".to_string());
        };
        let mut items = Vec::new();
        let mut rest = inner.trim();
        while !rest.is_empty() {
            let (item, after) = parse_string(rest)
                .map_err(|_| "arrays hold strings in double quotes".to_string())?;
            items.push(item);
            rest = after.trim_start();
            if let Some(after_comma) = rest.strip_prefix(',') {
                rest = after_comma.trim_start();
            } else if !rest.is_empty() {
                return Err("separate array items with commas".to_string());
            }
        }
        return Ok(Value::Strings(items));
    }
    match text {
        "true" => return Ok(Value::Boolean(true)),
        "false" => return Ok(Value::Boolean(false)),
        _ => {}
    }
    if let Ok(number) = text.replace('_', "").parse::<i64>() {
        return Ok(Value::Integer(number));
    }
    Err(format!(
        "`{text}` is not a value; strings need double quotes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(text: &str) -> Vec<String> {
        match parse(FileId(0), text) {
            Ok(_) => Vec::new(),
            Err(errors) => errors.iter().map(|e| e.message.clone()).collect(),
        }
    }

    #[test]
    fn reads_a_package() {
        let manifest = parse(
            FileId(0),
            "# A project\n[package]\nname = \"hello_world\"  # the name\nversion = \"0.1.0\"\nauthors = [\"Ada\", \"Grace\"]\ndescription = \"Says #1 hello\"\n",
        )
        .ok()
        .unwrap();
        assert_eq!(manifest.name, "hello_world");
        assert_eq!(manifest.entry, "src/main.sayon");
        assert_eq!(manifest.authors, vec!["Ada", "Grace"]);
        assert_eq!(manifest.description.as_deref(), Some("Says #1 hello"));
    }

    #[test]
    fn reports_problems() {
        assert_eq!(
            codes("[package]\nname = \"x\"\nverison = \"0.1.0\"\n"),
            vec!["unknown key `verison` in [package]"]
        );
        assert_eq!(
            codes("[package]\nname = \"Hello\"\nversion = \"1\"\n").len(),
            2
        );
        assert_eq!(
            codes("[package]\nversion = \"0.1.0\"\n"),
            vec!["the manifest needs a package name"]
        );
        assert_eq!(
            codes("[package]\nname = hello\nversion = \"0.1.0\"\n"),
            vec!["`hello` is not a value; strings need double quotes"]
        );
        assert_eq!(
            codes("[package]\nname = \"a\"\nversion = \"0.1.0\"\n[dependencies]\nx = \"1.0.0\"\n"),
            vec!["[dependencies] is not available yet"]
        );
        assert_eq!(
            codes("name = \"a\"\n"),
            vec!["`name` must be inside a table"]
        );
    }
}
