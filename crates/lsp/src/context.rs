//! What the text before a place says about it, for completion and signature
//! help: whether it is in a comment or a string, and which calls' arguments
//! hold it. The text is read from its start, as the lexer reads it, so that
//! brackets in strings and comments do not count.

/// A call whose arguments hold a place.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Call {
    /// What is called, as written before `(`: a name, or names joined by
    /// dots.
    pub callee: String,
    /// Where that ends.
    pub callee_end: usize,
    /// Which argument holds the place, counting from 0.
    pub argument: usize,
    /// The fields the other arguments name, as in `Point(x = 1, ...)`.
    pub named: Vec<String>,
    /// The field the place's own argument names, once `name =` is written.
    pub naming: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Context {
    pub comment: bool,
    /// In a string's text, not in an interpolation.
    pub string: bool,
    /// In the path of an `import("...")`: where the path starts.
    pub import_path: Option<usize>,
    /// The calls whose arguments hold the place, innermost last.
    pub calls: Vec<Call>,
}

enum Frame {
    Paren {
        open: usize,
        argument: usize,
        start: usize,
        named: Vec<String>,
    },
    Bracket,
    Brace,
    String {
        start: usize,
    },
    Interpolation,
}

/// The name an argument gives itself, as `x` in `x = 1`.
fn argument_name(argument: &str) -> Option<String> {
    let argument = argument.trim_start();
    let end = argument
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(argument.len());
    let (name, rest) = argument.split_at(end);
    let rest = rest.trim_start();
    let starts = name
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_');
    (starts && rest.starts_with('=') && !rest.starts_with("==")).then(|| name.to_string())
}

/// The names joined by dots that end at `end`, as `graphics.canvas`.
fn callee_before(text: &str, end: usize) -> (String, usize) {
    let before = &text[..end];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '.'))
        .map_or(0, |(index, c)| index + c.len_utf8());
    (before[start..].trim_matches('.').to_string(), end)
}

/// Whether each line is inside a block comment, its `##` lines among them.
fn block_lines(text: &str) -> Vec<bool> {
    let mut inside = false;
    text.split('\n')
        .map(|line| {
            if line.trim() == "##" {
                inside = !inside;
                return true;
            }
            inside
        })
        .collect()
}

pub fn context(text: &str, offset: usize) -> Context {
    let offset = offset.min(text.len());
    let blocks = block_lines(text);
    // A place in a block comment, its `##` lines among it.
    let line = text[..offset].bytes().filter(|b| *b == b'\n').count();
    if blocks.get(line).copied().unwrap_or(false) {
        return Context {
            comment: true,
            ..Context::default()
        };
    }
    let bytes = text.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    let mut at = 0;
    let mut line = 0;
    let mut line_start = true;
    while at < offset {
        if line_start {
            line_start = false;
            if blocks.get(line).copied().unwrap_or(false) {
                let end = text[at..].find('\n').map_or(text.len(), |n| at + n);
                if offset <= end {
                    return Context {
                        comment: true,
                        ..Context::default()
                    };
                }
                at = end;
                continue;
            }
        }
        let byte = bytes[at];
        if byte == b'\n' {
            line += 1;
            line_start = true;
            // A string ends with its line.
            if matches!(stack.last(), Some(Frame::String { .. })) {
                stack.pop();
            }
            at += 1;
            continue;
        }
        if let Some(Frame::String { .. }) = stack.last() {
            match byte {
                b'\\' => at += 1,
                b'"' => {
                    stack.pop();
                }
                b'{' if bytes.get(at + 1) == Some(&b'{') => at += 1,
                b'{' => stack.push(Frame::Interpolation),
                _ => {}
            }
            at += 1;
            continue;
        }
        match byte {
            b'#' => {
                let end = text[at..].find('\n').map_or(text.len(), |n| at + n);
                if offset <= end {
                    return Context {
                        comment: true,
                        ..Context::default()
                    };
                }
                at = end;
                continue;
            }
            b'"' => stack.push(Frame::String { start: at }),
            b'(' => stack.push(Frame::Paren {
                open: at,
                argument: 0,
                start: at + 1,
                named: Vec::new(),
            }),
            b')' => {
                if let Some(index) = stack.iter().rposition(|f| matches!(f, Frame::Paren { .. })) {
                    stack.truncate(index);
                }
            }
            b'[' => stack.push(Frame::Bracket),
            b']' => {
                if let Some(index) = stack.iter().rposition(|f| matches!(f, Frame::Bracket)) {
                    stack.truncate(index);
                }
            }
            b'{' => stack.push(Frame::Brace),
            b'}' => {
                if matches!(stack.last(), Some(Frame::Interpolation)) {
                    stack.pop();
                } else if let Some(index) = stack.iter().rposition(|f| matches!(f, Frame::Brace)) {
                    stack.truncate(index);
                }
            }
            b',' => {
                if let Some(Frame::Paren {
                    argument,
                    start,
                    named,
                    ..
                }) = stack.last_mut()
                {
                    if let Some(name) = argument_name(&text[*start..at]) {
                        named.push(name);
                    }
                    *argument += 1;
                    *start = at + 1;
                }
            }
            _ => {}
        }
        at += 1;
    }
    let mut context = Context::default();
    if let Some(Frame::String { start }) = stack.last() {
        context.string = true;
        if text[..*start].trim_end().ends_with("import(") {
            context.import_path = Some(start + 1);
        }
    }
    for frame in &stack {
        if let Frame::Paren {
            open,
            argument,
            start,
            named,
        } = frame
        {
            let (callee, callee_end) = callee_before(text, *open);
            if callee.is_empty() {
                continue;
            }
            context.calls.push(Call {
                callee,
                callee_end,
                argument: *argument,
                named: named.clone(),
                naming: argument_name(&text[*start..offset]),
            });
        }
    }
    context
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_end(text: &str) -> Context {
        context(text, text.len())
    }

    #[test]
    fn calls_and_their_arguments() {
        let found = at_end("x = graphics.rgb(10, f(\"a, b\"), ");
        assert_eq!(found.calls.len(), 1);
        assert_eq!(found.calls[0].callee, "graphics.rgb");
        assert_eq!(found.calls[0].argument, 2);
        let nested = at_end("print(length(items), canvas.line(1, ");
        let callees: Vec<&str> = nested.calls.iter().map(|c| c.callee.as_str()).collect();
        assert_eq!(callees, vec!["print", "canvas.line"]);
        assert_eq!(nested.calls[1].argument, 1);
        let named = at_end("p = Point(x = 1, y = ");
        assert_eq!(named.calls[0].named, vec!["x".to_string()]);
        assert_eq!(named.calls[0].naming.as_deref(), Some("y"));
        assert!(at_end("(1 + 2) * ").calls.is_empty());
    }

    #[test]
    fn comments_and_strings() {
        assert!(at_end("x = 1  # a comment (").comment);
        assert!(at_end("##\nnotes (\n").comment);
        assert!(!at_end("##\nnotes\n##\nprint(").comment);
        assert_eq!(at_end("##\nnotes\n##\nprint(").calls.len(), 1);
        let string = at_end("print(\"text (");
        assert!(string.string && string.import_path.is_none());
        let path = at_end("strings = import(\"standard/st");
        assert_eq!(path.import_path, Some(18));
        let interpolation = at_end("print(\"total: {count(");
        assert!(!interpolation.string);
        assert_eq!(interpolation.calls.last().unwrap().callee, "count");
    }
}
