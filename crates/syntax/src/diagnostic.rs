//! Diagnostics with stable codes and a plain-text renderer.

use crate::source::{SourceMap, Span};
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Label {
    pub span: Span,
    pub message: String,
    pub primary: bool,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub labels: Vec<Label>,
    pub notes: Vec<String>,
    pub help: Vec<String>,
}

impl Diagnostic {
    pub fn new(severity: Severity, code: &'static str, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity,
            code,
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
            help: Vec::new(),
        }
    }

    pub fn error(code: &'static str, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(Severity::Error, code, message)
    }

    pub fn warning(code: &'static str, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(Severity::Warning, code, message)
    }

    pub fn primary(mut self, span: Span, message: impl Into<String>) -> Diagnostic {
        self.labels.push(Label {
            span,
            message: message.into(),
            primary: true,
        });
        self
    }

    pub fn secondary(mut self, span: Span, message: impl Into<String>) -> Diagnostic {
        self.labels.push(Label {
            span,
            message: message.into(),
            primary: false,
        });
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Diagnostic {
        self.notes.push(note.into());
        self
    }

    pub fn help(mut self, help: impl Into<String>) -> Diagnostic {
        self.help.push(help.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    pub fn primary_span(&self) -> Option<Span> {
        self.labels
            .iter()
            .find(|label| label.primary)
            .or(self.labels.first())
            .map(|l| l.span)
    }
}

/// Collects diagnostics while a phase runs.
#[derive(Default, Debug)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new() -> Diagnostics {
        Diagnostics::default()
    }

    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    pub fn extend(&mut self, other: impl IntoIterator<Item = Diagnostic>) {
        self.items.extend(other);
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(Diagnostic::is_error)
    }

    pub fn error_count(&self) -> usize {
        self.items.iter().filter(|d| d.is_error()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter()
    }

    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.items
    }

    /// Orders diagnostics by file and position so output is deterministic.
    pub fn sort(&mut self) {
        self.items.sort_by_key(|d| {
            let span = d.primary_span();
            (span.map(|s| (s.file, s.start)), d.severity)
        });
    }
}

impl IntoIterator for Diagnostics {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;
    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

#[derive(Clone, Copy)]
struct Palette {
    enabled: bool,
}

impl Palette {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    fn severity(&self, severity: Severity, text: &str) -> String {
        match severity {
            Severity::Error => self.paint("1;31", text),
            Severity::Warning => self.paint("1;33", text),
            Severity::Note => self.paint("1;36", text),
        }
    }
    fn gutter(&self, text: &str) -> String {
        self.paint("1;34", text)
    }
    fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }
}

const TAB_WIDTH: usize = 4;

/// Renders a diagnostic in the familiar `error[CODE]: message` layout.
pub fn render(diagnostic: &Diagnostic, sources: &SourceMap, color: bool) -> String {
    let palette = Palette { enabled: color };
    let mut out = String::new();
    let head = format!("{}[{}]", diagnostic.severity.label(), diagnostic.code);
    let _ = writeln!(
        out,
        "{}{}",
        palette.severity(diagnostic.severity, &head),
        palette.bold(&format!(": {}", diagnostic.message))
    );

    // Group labels by file, primary file first.
    let mut labels: Vec<&Label> = diagnostic.labels.iter().collect();
    labels.sort_by_key(|label| (!label.primary, label.span.file, label.span.start));
    let mut files = Vec::new();
    for label in &labels {
        if !files.contains(&label.span.file) {
            files.push(label.span.file);
        }
    }

    let max_line = labels
        .iter()
        .map(|label| sources.get(label.span.file).line_col(label.span.start).0)
        .max()
        .unwrap_or(1);
    let width = max_line.to_string().len();
    let pad = " ".repeat(width + 1);

    for file_id in files {
        let file = sources.get(file_id);
        let mut file_labels: Vec<&Label> = labels
            .iter()
            .copied()
            .filter(|label| label.span.file == file_id)
            .collect();
        let anchor = file_labels
            .iter()
            .find(|label| label.primary)
            .copied()
            .unwrap_or(file_labels[0]);
        let (line, column) = file.line_col(anchor.span.start);
        let _ = writeln!(
            out,
            "{}{} {}:{}:{}",
            pad,
            palette.gutter("-->"),
            file.name,
            line,
            column
        );
        let _ = writeln!(out, "{} {}", pad, palette.gutter("|"));

        file_labels.sort_by_key(|label| (label.span.start, !label.primary));
        let mut previous_line: Option<usize> = None;
        for label in file_labels {
            let line_index = file.line_index(label.span.start);
            if let Some(previous) = previous_line
                && line_index > previous + 1
            {
                let _ = writeln!(out, "{}", palette.gutter("..."));
            }
            if previous_line != Some(line_index) {
                let text = expand_tabs(file.line_text(line_index));
                let number = format!(" {:>width$}", line_index + 1);
                let _ = writeln!(
                    out,
                    "{} {} {}",
                    palette.gutter(&number),
                    palette.gutter("|"),
                    text
                );
            }
            previous_line = Some(line_index);

            let line_start = file.line_start(line_index) as usize;
            let line_text = file.line_text(line_index);
            let start_in_line = (label.span.start as usize - line_start).min(line_text.len());
            let end_in_line = (label.span.end as usize)
                .saturating_sub(line_start)
                .clamp(start_in_line, line_text.len());
            let prefix_width = display_width(&line_text[..start_in_line]);
            let marked_width = display_width(&line_text[start_in_line..end_in_line]).max(1);
            let marker = if label.primary { "^" } else { "-" };
            let underline = marker.repeat(marked_width);
            let painted = if label.primary {
                palette.severity(diagnostic.severity, &underline)
            } else {
                palette.gutter(&underline)
            };
            let message = if label.message.is_empty() {
                String::new()
            } else if label.primary {
                format!(" {}", palette.severity(diagnostic.severity, &label.message))
            } else {
                format!(" {}", palette.gutter(&label.message))
            };
            let _ = writeln!(
                out,
                "{} {} {}{}{}",
                pad,
                palette.gutter("|"),
                " ".repeat(prefix_width),
                painted,
                message
            );
        }
    }

    if !diagnostic.labels.is_empty()
        && (!diagnostic.notes.is_empty() || !diagnostic.help.is_empty())
    {
        let _ = writeln!(out, "{} {}", pad, palette.gutter("|"));
    }
    for (kind, items) in [("note", &diagnostic.notes), ("help", &diagnostic.help)] {
        for item in items {
            let mut lines = item.lines();
            let first = lines.next().unwrap_or("");
            let _ = writeln!(
                out,
                "{} {} {}: {}",
                pad,
                palette.gutter("="),
                palette.bold(kind),
                first
            );
            let continuation = " ".repeat(width + 1 + 3 + kind.len() + 2);
            for line in lines {
                let _ = writeln!(out, "{continuation}{line}");
            }
        }
    }
    out
}

fn expand_tabs(text: &str) -> String {
    text.replace('\t', &" ".repeat(TAB_WIDTH))
}

fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| if c == '\t' { TAB_WIDTH } else { 1 })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceMap;

    #[test]
    fn renders_primary_and_secondary_labels() {
        let mut sources = SourceMap::new();
        let text = "numbers = [1, 2]\nother = numbers\nprint(numbers)\n".to_string();
        let file = sources.add("src/main.sayon", None, text);
        let moved = Span::new(file, 25, 32);
        let used = Span::new(file, 39, 46);
        let diagnostic = Diagnostic::error("E_MOVED_VALUE", "numbers was transferred to other")
            .secondary(moved, "ownership transferred here")
            .primary(used, "numbers no longer owns the array")
            .help("use other here, or write other = clone(numbers)\nwhen you need two independent arrays");
        let rendered = render(&diagnostic, &sources, false);
        let expected = "\
error[E_MOVED_VALUE]: numbers was transferred to other
  --> src/main.sayon:3:7
   |
 2 | other = numbers
   |         ------- ownership transferred here
 3 | print(numbers)
   |       ^^^^^^^ numbers no longer owns the array
   |
   = help: use other here, or write other = clone(numbers)
           when you need two independent arrays
";
        assert_eq!(rendered, expected);
    }
}
