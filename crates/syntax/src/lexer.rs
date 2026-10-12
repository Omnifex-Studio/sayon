//! Converts UTF-8 source text into tokens.
//!
//! The lexer does not produce indentation tokens. Instead every token records
//! whether it starts a physical line and that line's indentation column, and the
//! parser applies the layout rules. This keeps blocks inside delimiters (such as
//! a function literal passed as an argument) straightforward to parse.

use crate::diagnostic::Diagnostic;
use crate::source::{FileId, Span};
use crate::token::{Comment, Keyword, StringPart, Token, TokenKind};

pub const TAB_COLUMNS: u32 = 4;

pub struct LexOutput {
    pub tokens: Vec<Token>,
    pub comments: Vec<Comment>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn lex(file: FileId, source: &str) -> LexOutput {
    let mut lexer = Lexer {
        file,
        src: source,
        pos: 0,
        tokens: Vec::new(),
        comments: Vec::new(),
        diagnostics: Vec::new(),
        indent_style: None,
        unterminated_line_end: None,
    };
    lexer.run();
    LexOutput {
        tokens: lexer.tokens,
        comments: lexer.comments,
        diagnostics: lexer.diagnostics,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IndentChar {
    Space,
    Tab,
}

enum Scan {
    Token(TokenKind, usize),
    EndOfLine,
    CloseBrace(usize),
}

struct Lexer<'a> {
    file: FileId,
    src: &'a str,
    pos: usize,
    tokens: Vec<Token>,
    comments: Vec<Comment>,
    diagnostics: Vec<Diagnostic>,
    indent_style: Option<(IndentChar, Span)>,
    /// Line end of the last unterminated-string report, to avoid repeating it
    /// for a string nested inside an interpolation.
    unterminated_line_end: Option<usize>,
}

impl<'a> Lexer<'a> {
    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, start, end)
    }

    fn error(&mut self, code: &'static str, message: impl Into<String>, start: usize, end: usize) {
        let span = self.span(start, end.max(start));
        self.diagnostics
            .push(Diagnostic::error(code, message).primary(span, ""));
    }

    fn peek_byte(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn byte_at(&self, offset: usize) -> Option<u8> {
        self.src.as_bytes().get(self.pos + offset).copied()
    }

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    /// Byte offset of the line terminator at or after `from`, or the end of input.
    fn line_end(&self, from: usize) -> usize {
        self.src[from..]
            .find(['\n', '\r'])
            .map(|i| from + i)
            .unwrap_or(self.src.len())
    }

    fn run(&mut self) {
        if self.src.starts_with('\u{FEFF}') {
            self.pos = '\u{FEFF}'.len_utf8();
        }
        while self.pos < self.src.len() {
            self.lex_line();
        }
        let end = self.src.len();
        self.tokens.push(Token {
            kind: TokenKind::EndOfFile,
            span: self.span(end, end),
            line_start: true,
            indent: 0,
        });
    }

    fn consume_newline(&mut self) {
        match (self.peek_byte(), self.byte_at(1)) {
            (Some(b'\r'), Some(b'\n')) => self.pos += 2,
            (Some(b'\n'), _) => self.pos += 1,
            (Some(b'\r'), _) => {
                self.error(
                    "E_LONE_CARRIAGE_RETURN",
                    "a carriage return must be followed by a line feed",
                    self.pos,
                    self.pos + 1,
                );
                self.pos += 1;
            }
            _ => {}
        }
    }

    fn lex_line(&mut self) {
        let line_begin = self.pos;
        let mut spaces = 0u32;
        let mut tabs = 0u32;
        while let Some(byte) = self.peek_byte() {
            match byte {
                b' ' => spaces += 1,
                b'\t' => tabs += 1,
                _ => break,
            }
            self.pos += 1;
        }
        let content_start = self.pos;
        let line_end = self.line_end(content_start);
        let content = &self.src[content_start..line_end];

        if content.trim_end_matches([' ', '\t']).is_empty() {
            self.pos = line_end;
            self.consume_newline();
            return;
        }
        if content.trim_end_matches([' ', '\t']) == "##" {
            self.block_comment(line_begin, content_start, line_end);
            return;
        }
        if content.starts_with('#') {
            self.comments.push(Comment {
                span: self.span(content_start, line_end),
                text: content.trim_end().to_string(),
                block: false,
                own_line: true,
            });
            self.pos = line_end;
            self.consume_newline();
            return;
        }

        self.check_indentation(spaces, tabs, line_begin, content_start);
        let indent = spaces + tabs * TAB_COLUMNS;
        let mut first = true;
        loop {
            match self.scan(false) {
                Scan::Token(kind, start) => {
                    let span = self.span(start, self.pos);
                    self.tokens.push(Token {
                        kind,
                        span,
                        line_start: first,
                        indent,
                    });
                    first = false;
                }
                Scan::EndOfLine => break,
                Scan::CloseBrace(start) => {
                    self.error(
                        "E_UNEXPECTED_CHARACTER",
                        "unexpected `}`; Sayon uses indentation for blocks",
                        start,
                        start + 1,
                    );
                }
            }
        }
        self.pos = self.line_end(self.pos);
        self.consume_newline();
    }

    fn check_indentation(
        &mut self,
        spaces: u32,
        tabs: u32,
        line_begin: usize,
        content_start: usize,
    ) {
        if spaces == 0 && tabs == 0 {
            return;
        }
        let span = self.span(line_begin, content_start);
        if spaces > 0 && tabs > 0 {
            self.diagnostics.push(
                Diagnostic::error(
                    "E_MIXED_INDENTATION",
                    "this line's indentation mixes tabs and spaces",
                )
                .primary(span, "")
                .help("indent with four spaces per level; the formatter can convert tabs"),
            );
            return;
        }
        let style = if tabs > 0 {
            IndentChar::Tab
        } else {
            IndentChar::Space
        };
        match self.indent_style {
            None => self.indent_style = Some((style, span)),
            Some((established, first_span)) if established != style => {
                let (used, other) = match established {
                    IndentChar::Space => ("spaces", "tabs"),
                    IndentChar::Tab => ("tabs", "spaces"),
                };
                self.diagnostics.push(
                    Diagnostic::error(
                        "E_MIXED_INDENTATION",
                        format!("this file indents with {used}, but this line uses {other}"),
                    )
                    .primary(span, format!("indented with {other}"))
                    .secondary(
                        first_span,
                        format!("the file's first indentation uses {used}"),
                    )
                    .help("use one indentation character throughout a file"),
                );
            }
            Some(_) => {}
        }
    }

    fn block_comment(&mut self, line_begin: usize, content_start: usize, line_end: usize) {
        let opening = self.span(content_start, content_start + 2);
        self.pos = line_end;
        self.consume_newline();
        let body_start = self.pos;
        while self.pos < self.src.len() {
            let start = self.pos;
            let end = self.line_end(start);
            if self.src[start..end].trim_matches([' ', '\t']) == "##" {
                self.comments.push(Comment {
                    span: self.span(content_start, end),
                    text: self.src[body_start..start]
                        .trim_end_matches(['\n', '\r'])
                        .to_string(),
                    block: true,
                    own_line: true,
                });
                self.pos = end;
                self.consume_newline();
                return;
            }
            self.pos = end;
            self.consume_newline();
        }
        let _ = line_begin;
        self.diagnostics.push(
            Diagnostic::error(
                "E_UNTERMINATED_COMMENT",
                "this block comment is never closed",
            )
            .primary(opening, "block comment opened here")
            .help("close it with a line containing only `##`"),
        );
    }

    fn scan(&mut self, nested: bool) -> Scan {
        loop {
            while matches!(self.peek_byte(), Some(b' ' | b'\t')) {
                self.pos += 1;
            }
            let start = self.pos;
            let Some(c) = self.peek_char() else {
                return Scan::EndOfLine;
            };
            let single = |kind: TokenKind, lexer: &mut Lexer<'a>| {
                lexer.pos += 1;
                Scan::Token(kind, start)
            };
            return match c {
                '\n' | '\r' => Scan::EndOfLine,
                '#' => {
                    let end = self.line_end(start);
                    if nested {
                        self.error(
                            "E_UNTERMINATED_STRING",
                            "a comment cannot appear inside string interpolation",
                            start,
                            end,
                        );
                    } else {
                        self.comments.push(Comment {
                            span: self.span(start, end),
                            text: self.src[start..end].trim_end().to_string(),
                            block: false,
                            own_line: false,
                        });
                    }
                    self.pos = end;
                    Scan::EndOfLine
                }
                '}' => {
                    self.pos += 1;
                    Scan::CloseBrace(start)
                }
                '"' => self.string(start),
                '0'..='9' => self.number(start),
                'a'..='z' | 'A'..='Z' | '_' => self.identifier(start),
                '(' => single(TokenKind::LeftParen, self),
                ')' => single(TokenKind::RightParen, self),
                '[' => single(TokenKind::LeftBracket, self),
                ']' => single(TokenKind::RightBracket, self),
                ',' => single(TokenKind::Comma, self),
                ':' => single(TokenKind::Colon, self),
                '.' => {
                    if self.byte_at(1) == Some(b'.') && self.byte_at(2) == Some(b'.') {
                        self.pos += 3;
                        Scan::Token(TokenKind::Ellipsis, start)
                    } else {
                        single(TokenKind::Dot, self)
                    }
                }
                '+' => self.with_assign(start, TokenKind::Plus, TokenKind::PlusAssign),
                '*' => self.with_assign(start, TokenKind::Star, TokenKind::StarAssign),
                '/' => self.with_assign(start, TokenKind::Slash, TokenKind::SlashAssign),
                '%' => self.with_assign(start, TokenKind::Percent, TokenKind::PercentAssign),
                '-' => match self.byte_at(1) {
                    Some(b'>') => {
                        self.pos += 2;
                        Scan::Token(TokenKind::Arrow, start)
                    }
                    _ => self.with_assign(start, TokenKind::Minus, TokenKind::MinusAssign),
                },
                '=' => self.with_assign(start, TokenKind::Assign, TokenKind::Equal),
                '<' => self.with_assign(start, TokenKind::Less, TokenKind::LessEqual),
                '>' => self.with_assign(start, TokenKind::Greater, TokenKind::GreaterEqual),
                '!' => {
                    if self.byte_at(1) == Some(b'=') {
                        self.pos += 2;
                        Scan::Token(TokenKind::NotEqual, start)
                    } else {
                        // Reported, then read as `not` so parsing continues.
                        self.replaced_operator(start, 1, "!", "not");
                        Scan::Token(TokenKind::Keyword(Keyword::Not), start)
                    }
                }
                '&' | '|' => {
                    let doubled = self.byte_at(1) == Some(c as u8);
                    let (symbol, word, keyword) = match (c, doubled) {
                        ('&', true) => ("&&", "and", Keyword::And),
                        ('|', true) => ("||", "or", Keyword::Or),
                        _ => {
                            let help = if c == '&' {
                                "use `and` to combine conditions"
                            } else {
                                "use `or` to combine conditions"
                            };
                            self.unexpected(start, c, help);
                            continue;
                        }
                    };
                    self.replaced_operator(start, 2, symbol, word);
                    Scan::Token(TokenKind::Keyword(keyword), start)
                }
                ';' => {
                    self.unexpected(start, c, "statements end at line breaks; remove the `;`");
                    continue;
                }
                '{' => {
                    self.unexpected(start, c, "Sayon uses indentation for blocks");
                    continue;
                }
                '\'' => {
                    self.unexpected(start, c, "strings use double quotes");
                    continue;
                }
                _ => {
                    self.unexpected(start, c, "");
                    continue;
                }
            };
        }
    }

    fn with_assign(&mut self, start: usize, plain: TokenKind, with_equal: TokenKind) -> Scan {
        if self.byte_at(1) == Some(b'=') {
            self.pos += 2;
            Scan::Token(with_equal, start)
        } else {
            self.pos += 1;
            Scan::Token(plain, start)
        }
    }

    fn replaced_operator(&mut self, start: usize, length: usize, symbol: &str, word: &str) {
        self.pos = start + length;
        self.diagnostics.push(
            Diagnostic::error(
                "E_UNEXPECTED_CHARACTER",
                format!("`{symbol}` is not a Sayon operator"),
            )
            .primary(self.span(start, self.pos), format!("write `{word}`"))
            .help("logic in Sayon uses the words `and`, `or`, and `not`"),
        );
    }

    fn unexpected(&mut self, start: usize, c: char, help: &str) {
        self.pos += c.len_utf8();
        let shown = if c.is_control() {
            format!("{:?}", c)
        } else {
            format!("`{c}`")
        };
        let mut diagnostic = Diagnostic::error(
            "E_UNEXPECTED_CHARACTER",
            format!("unexpected character {shown}"),
        )
        .primary(self.span(start, self.pos), "");
        if !help.is_empty() {
            diagnostic = diagnostic.help(help);
        } else if !c.is_ascii() {
            diagnostic = diagnostic.help(
                "identifiers use ASCII letters, digits and underscores; Unicode belongs in strings",
            );
        }
        self.diagnostics.push(diagnostic);
    }

    fn identifier(&mut self, start: usize) -> Scan {
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        let kind = match Keyword::lookup(text) {
            Some(keyword) => TokenKind::Keyword(keyword),
            None => TokenKind::Identifier(text.to_string()),
        };
        Scan::Token(kind, start)
    }

    fn take_digits(&mut self, is_digit: impl Fn(u8) -> bool) -> (usize, usize) {
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            if is_digit(b) || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        (start, self.pos)
    }

    /// Checks underscore placement and returns the digits with separators removed.
    fn clean_digits(&mut self, start: usize, end: usize) -> Option<String> {
        let text = &self.src[start..end];
        if text.is_empty() {
            return Some(String::new());
        }
        if text.starts_with('_') || text.ends_with('_') || text.contains("__") {
            self.error(
                "E_INVALID_NUMBER",
                "underscores in numbers must separate digits",
                start,
                end,
            );
            return None;
        }
        Some(text.replace('_', ""))
    }

    fn number(&mut self, start: usize) -> Scan {
        let radix = match (self.peek_byte(), self.byte_at(1)) {
            (Some(b'0'), Some(b'x')) => 16,
            (Some(b'0'), Some(b'b')) => 2,
            (Some(b'0'), Some(b'o')) => 8,
            _ => 10,
        };
        let kind = if radix != 10 {
            self.pos += 2;
            let (digits_start, digits_end) = if radix == 16 {
                self.take_digits(|b| b.is_ascii_hexdigit())
            } else {
                self.take_digits(|b| b.is_ascii_digit())
            };
            let cleaned = self.clean_digits(digits_start, digits_end);
            let unsigned = self.unsigned_suffix(start);
            match cleaned {
                Some(digits) if digits.is_empty() => {
                    self.error(
                        "E_INVALID_NUMBER",
                        "expected digits after the number prefix",
                        start,
                        self.pos,
                    );
                    TokenKind::Integer { value: 0, unsigned }
                }
                Some(digits) => match u128::from_str_radix(&digits, radix) {
                    Ok(value) => TokenKind::Integer {
                        value: self.check_range(value, start),
                        unsigned,
                    },
                    Err(_) => {
                        let name = match radix {
                            16 => "hexadecimal",
                            8 => "octal",
                            _ => "binary",
                        };
                        self.error(
                            "E_INVALID_NUMBER",
                            format!("invalid digit in {name} literal"),
                            digits_start,
                            digits_end,
                        );
                        TokenKind::Integer { value: 0, unsigned }
                    }
                },
                None => TokenKind::Integer { value: 0, unsigned },
            }
        } else {
            let (int_start, int_end) = self.take_digits(|b| b.is_ascii_digit());
            let mut is_float = false;
            let mut fraction = None;
            if self.peek_byte() == Some(b'.') && self.byte_at(1).is_some_and(|b| b.is_ascii_digit())
            {
                self.pos += 1;
                fraction = Some(self.take_digits(|b| b.is_ascii_digit()));
                is_float = true;
            }
            let mut exponent = None;
            if matches!(self.peek_byte(), Some(b'e' | b'E')) {
                let sign = matches!(self.byte_at(1), Some(b'+' | b'-')) as usize;
                if self.byte_at(1 + sign).is_some_and(|b| b.is_ascii_digit()) {
                    let exp_start = self.pos;
                    self.pos += 1 + sign;
                    let (digits_start, digits_end) = self.take_digits(|b| b.is_ascii_digit());
                    exponent = Some((exp_start, digits_start, digits_end));
                    is_float = true;
                }
            }
            let integer = self.clean_digits(int_start, int_end);
            let fraction = fraction.map(|(s, e)| self.clean_digits(s, e));
            let exponent_digits = exponent.map(|(_, s, e)| self.clean_digits(s, e));
            if is_float {
                if self.peek_byte() == Some(b'u') {
                    let at = self.pos;
                    self.pos += 1;
                    self.error(
                        "E_INVALID_NUMBER",
                        "the `u` suffix applies only to integer literals",
                        at,
                        self.pos,
                    );
                }
                self.reject_suffix(start);
                match (
                    integer,
                    fraction.unwrap_or(Some(String::new())),
                    exponent_digits.unwrap_or(Some(String::new())),
                ) {
                    (Some(int), Some(frac), Some(_)) => {
                        let mut text = int;
                        if !frac.is_empty() {
                            text.push('.');
                            text.push_str(&frac);
                        }
                        if let Some((exp_start, digits_start, digits_end)) = exponent {
                            let sign = &self.src[exp_start + 1..digits_start];
                            text.push('e');
                            text.push_str(sign);
                            text.push_str(&self.src[digits_start..digits_end].replace('_', ""));
                        }
                        let value: f64 = text.parse().unwrap_or(f64::INFINITY);
                        if !value.is_finite() {
                            self.error(
                                "E_LITERAL_RANGE",
                                "float literal is too large to represent",
                                start,
                                self.pos,
                            );
                        }
                        TokenKind::Float(value)
                    }
                    _ => TokenKind::Float(0.0),
                }
            } else {
                let unsigned = self.unsigned_suffix(start);
                match integer {
                    Some(digits) => match digits.parse::<u128>() {
                        Ok(value) => TokenKind::Integer {
                            value: self.check_range(value, start),
                            unsigned,
                        },
                        Err(_) => {
                            self.error(
                                "E_LITERAL_RANGE",
                                "integer literal is too large",
                                start,
                                self.pos,
                            );
                            TokenKind::Integer { value: 0, unsigned }
                        }
                    },
                    None => TokenKind::Integer { value: 0, unsigned },
                }
            }
        };
        Scan::Token(kind, start)
    }

    fn check_range(&mut self, value: u128, start: usize) -> u64 {
        if value > u64::MAX as u128 {
            self.error(
                "E_LITERAL_RANGE",
                "integer literal does not fit in 64 bits",
                start,
                self.pos,
            );
            0
        } else {
            value as u64
        }
    }

    fn unsigned_suffix(&mut self, start: usize) -> bool {
        let unsigned = self.peek_byte() == Some(b'u')
            && !self
                .byte_at(1)
                .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
        if unsigned {
            self.pos += 1;
        }
        self.reject_suffix(start);
        unsigned
    }

    fn reject_suffix(&mut self, start: usize) {
        let suffix_start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos > suffix_start {
            let suffix = &self.src[suffix_start..self.pos];
            let help =
                if suffix.starts_with(['X', 'B', 'O']) && &self.src[start..suffix_start] == "0" {
                    "number prefixes are lowercase: 0x, 0b, 0o"
                } else {
                    "only the `u` suffix is accepted, for unsigned integers"
                };
            self.diagnostics.push(
                Diagnostic::error(
                    "E_INVALID_NUMBER",
                    format!("invalid number suffix `{suffix}`"),
                )
                .primary(self.span(suffix_start, self.pos), "")
                .help(help),
            );
        }
    }

    fn string(&mut self, start: usize) -> Scan {
        self.pos += 1;
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut text_start = self.pos;
        loop {
            let Some(c) = self.peek_char() else {
                self.unterminated_string(start);
                break;
            };
            match c {
                '"' => {
                    self.flush_text(&mut parts, &mut text, text_start, self.pos);
                    self.pos += 1;
                    return Scan::Token(TokenKind::String(parts), start);
                }
                '\n' | '\r' => {
                    self.unterminated_string(start);
                    break;
                }
                '\\' => self.escape(&mut text),
                '{' if self.byte_at(1) == Some(b'{') => {
                    text.push('{');
                    self.pos += 2;
                }
                '{' => {
                    self.flush_text(&mut parts, &mut text, text_start, self.pos);
                    let open = self.pos;
                    self.pos += 1;
                    let mut tokens = Vec::new();
                    let closed = loop {
                        match self.scan(true) {
                            Scan::Token(kind, token_start) => tokens.push(Token {
                                kind,
                                span: self.span(token_start, self.pos),
                                line_start: false,
                                indent: 0,
                            }),
                            Scan::CloseBrace(_) => break true,
                            Scan::EndOfLine => break false,
                        }
                    };
                    if !closed {
                        self.unterminated_string(start);
                        break;
                    }
                    let span = self.span(open, self.pos);
                    if tokens.is_empty() {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "E_EMPTY_INTERPOLATION",
                                "empty interpolation in string",
                            )
                            .primary(span, "")
                            .help("write `{{` and `}}` for literal braces"),
                        );
                    } else {
                        parts.push(StringPart::Interpolation { tokens, span });
                    }
                    text_start = self.pos;
                }
                '}' if self.byte_at(1) == Some(b'}') => {
                    text.push('}');
                    self.pos += 2;
                }
                '}' => {
                    self.diagnostics.push(
                        Diagnostic::error("E_SINGLE_BRACE", "unmatched `}` in string")
                            .primary(self.span(self.pos, self.pos + 1), "")
                            .help("write `}}` for a literal closing brace"),
                    );
                    self.pos += 1;
                }
                _ => {
                    text.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
        self.flush_text(&mut parts, &mut text, text_start, self.pos);
        Scan::Token(TokenKind::String(parts), start)
    }

    fn flush_text(&self, parts: &mut Vec<StringPart>, text: &mut String, start: usize, end: usize) {
        if !text.is_empty() {
            parts.push(StringPart::Text {
                text: std::mem::take(text),
                span: self.span(start, end),
            });
        }
    }

    fn unterminated_string(&mut self, start: usize) {
        let end = self.line_end(start);
        self.pos = end;
        if self.unterminated_line_end == Some(end) {
            return;
        }
        self.unterminated_line_end = Some(end);
        self.diagnostics.push(
            Diagnostic::error("E_UNTERMINATED_STRING", "this string is never closed")
                .primary(self.span(start, end), "")
                .help("strings end with `\"` on the same line"),
        );
    }

    fn escape(&mut self, text: &mut String) {
        let start = self.pos;
        self.pos += 1;
        let Some(c) = self.peek_char() else { return };
        let simple = match c {
            '"' => Some('"'),
            '\\' => Some('\\'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            '0' => Some('\0'),
            _ => None,
        };
        if let Some(value) = simple {
            text.push(value);
            self.pos += 1;
            return;
        }
        if c == 'u' && self.byte_at(1) == Some(b'{') {
            self.pos += 2;
            let digits_start = self.pos;
            while self.peek_byte().is_some_and(|b| b.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            let digits = &self.src[digits_start..self.pos];
            let closed = self.peek_byte() == Some(b'}');
            if closed {
                self.pos += 1;
            }
            if !closed || digits.is_empty() || digits.len() > 6 {
                self.error(
                    "E_INVALID_ESCAPE",
                    "a Unicode escape is written `\\u{HEX}` with one to six hex digits",
                    start,
                    self.pos,
                );
                return;
            }
            let value = u32::from_str_radix(digits, 16).unwrap_or(u32::MAX);
            match char::from_u32(value) {
                Some(scalar) => text.push(scalar),
                None => self.error(
                    "E_INVALID_ESCAPE",
                    format!("U+{value:X} is not a Unicode scalar value"),
                    start,
                    self.pos,
                ),
            }
            return;
        }
        if c == '\n' || c == '\r' {
            return;
        }
        self.pos += c.len_utf8();
        self.diagnostics.push(
            Diagnostic::error(
                "E_INVALID_ESCAPE",
                format!("unknown escape sequence `\\{c}`"),
            )
            .primary(self.span(start, self.pos), "")
            .help("valid escapes are \\\" \\\\ \\n \\r \\t \\0 and \\u{HEX}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<TokenKind> {
        let output = lex(FileId(0), source);
        assert!(
            output.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            output.diagnostics
        );
        output.tokens.into_iter().map(|t| t.kind).collect()
    }

    fn errors(source: &str) -> Vec<&'static str> {
        lex(FileId(0), source)
            .diagnostics
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    fn ident(name: &str) -> TokenKind {
        TokenKind::Identifier(name.to_string())
    }

    fn int(value: u64) -> TokenKind {
        TokenKind::Integer {
            value,
            unsigned: false,
        }
    }

    #[test]
    fn numbers_in_every_base() {
        let tokens = kinds("42 0xFF 0b1010 0o755 1_000_000 3.25 255u 1e3 2.5e-2");
        assert_eq!(
            tokens,
            vec![
                int(42),
                int(255),
                int(10),
                int(493),
                int(1_000_000),
                TokenKind::Float(3.25),
                TokenKind::Integer {
                    value: 255,
                    unsigned: true
                },
                TokenKind::Float(1000.0),
                TokenKind::Float(0.025),
                TokenKind::EndOfFile,
            ]
        );
    }

    #[test]
    fn invalid_numbers() {
        assert_eq!(errors("1__0"), vec!["E_INVALID_NUMBER"]);
        assert_eq!(errors("100_"), vec!["E_INVALID_NUMBER"]);
        assert_eq!(errors("0x"), vec!["E_INVALID_NUMBER"]);
        assert_eq!(errors("0b102"), vec!["E_INVALID_NUMBER"]);
        assert_eq!(errors("12abc"), vec!["E_INVALID_NUMBER"]);
        assert_eq!(errors("0XFF"), vec!["E_INVALID_NUMBER"]);
        assert_eq!(errors("1.5u"), vec!["E_INVALID_NUMBER"]);
        assert_eq!(errors("18446744073709551616"), vec!["E_LITERAL_RANGE"]);
        assert!(errors("18446744073709551615").is_empty());
    }

    #[test]
    fn member_access_on_integer_is_not_a_float() {
        assert_eq!(
            kinds("1.x"),
            vec![int(1), TokenKind::Dot, ident("x"), TokenKind::EndOfFile]
        );
    }

    #[test]
    fn operators_and_keywords() {
        let tokens = kinds("a += b -> c == d != e <= f >= g ... not and or");
        assert_eq!(
            tokens,
            vec![
                ident("a"),
                TokenKind::PlusAssign,
                ident("b"),
                TokenKind::Arrow,
                ident("c"),
                TokenKind::Equal,
                ident("d"),
                TokenKind::NotEqual,
                ident("e"),
                TokenKind::LessEqual,
                ident("f"),
                TokenKind::GreaterEqual,
                ident("g"),
                TokenKind::Ellipsis,
                TokenKind::Keyword(Keyword::Not),
                TokenKind::Keyword(Keyword::And),
                TokenKind::Keyword(Keyword::Or),
                TokenKind::EndOfFile,
            ]
        );
    }

    #[test]
    fn rejected_symbols_explain_alternatives() {
        assert_eq!(errors("a && b"), vec!["E_UNEXPECTED_CHARACTER"]);
        assert_eq!(errors("a & b"), vec!["E_UNEXPECTED_CHARACTER"]);
        assert_eq!(errors("x = 1;"), vec!["E_UNEXPECTED_CHARACTER"]);
        assert_eq!(errors("!x"), vec!["E_UNEXPECTED_CHARACTER"]);
        assert_eq!(errors("café = 1"), vec!["E_UNEXPECTED_CHARACTER"]);
    }

    #[test]
    fn line_starts_and_indentation() {
        let output = lex(FileId(0), "if x\n    y\n\n    # comment\n\tz\n");
        let starts: Vec<(bool, u32)> = output
            .tokens
            .iter()
            .map(|t| (t.line_start, t.indent))
            .collect();
        assert_eq!(
            starts,
            vec![(true, 0), (false, 0), (true, 4), (true, 4), (true, 0)]
        );
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(output.diagnostics[0].code, "E_MIXED_INDENTATION");
    }

    #[test]
    fn tabs_count_as_four_columns() {
        let output = lex(FileId(0), "a\n\tb\n\t\tc\n");
        let indents: Vec<u32> = output.tokens.iter().map(|t| t.indent).collect();
        assert_eq!(indents, vec![0, 4, 8, 0]);
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn mixed_indentation_within_a_line() {
        assert_eq!(errors("a\n \tb\n"), vec!["E_MIXED_INDENTATION"]);
    }

    #[test]
    fn comments_are_collected() {
        let output = lex(
            FileId(0),
            "# line\nx = 1  # trailing\n##\nblock\n  text\n##\ny",
        );
        assert!(output.diagnostics.is_empty());
        let texts: Vec<&str> = output.comments.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["# line", "# trailing", "block\n  text"]);
        let names: Vec<TokenKind> = output.tokens.iter().map(|t| t.kind.clone()).collect();
        assert_eq!(
            names,
            vec![
                ident("x"),
                TokenKind::Assign,
                int(1),
                ident("y"),
                TokenKind::EndOfFile
            ]
        );
    }

    #[test]
    fn unterminated_block_comment() {
        assert_eq!(errors("##\nnever closed\n"), vec!["E_UNTERMINATED_COMMENT"]);
    }

    #[test]
    fn crlf_and_bom_are_accepted() {
        let output = lex(FileId(0), "\u{FEFF}a = 1\r\nb = 2\r\n");
        assert!(output.diagnostics.is_empty());
        assert_eq!(output.tokens.len(), 7);
        assert_eq!(errors("a\rb"), vec!["E_LONE_CARRIAGE_RETURN"]);
    }

    fn string_parts(source: &str) -> Vec<StringPart> {
        match kinds(source).into_iter().next() {
            Some(TokenKind::String(parts)) => parts,
            other => panic!("expected string, got {other:?}"),
        }
    }

    fn text_of(part: &StringPart) -> &str {
        match part {
            StringPart::Text { text, .. } => text,
            StringPart::Interpolation { .. } => panic!("expected text"),
        }
    }

    #[test]
    fn escapes() {
        let parts = string_parts(r#""a\"b\\c\nd\te\0\u{1F600}\u{e9}""#);
        assert_eq!(text_of(&parts[0]), "a\"b\\c\nd\te\0\u{1F600}é");
    }

    #[test]
    fn invalid_escapes() {
        assert_eq!(errors(r#""\q""#), vec!["E_INVALID_ESCAPE"]);
        assert_eq!(errors(r#""\u{D800}""#), vec!["E_INVALID_ESCAPE"]);
        assert_eq!(errors(r#""\u{110000}""#), vec!["E_INVALID_ESCAPE"]);
        assert_eq!(errors(r#""\u{}""#), vec!["E_INVALID_ESCAPE"]);
        assert_eq!(errors(r#""\u41""#), vec!["E_INVALID_ESCAPE"]);
    }

    #[test]
    fn interpolation_and_doubled_braces() {
        let parts = string_parts(r#""Braces: {{{name}}}""#);
        assert_eq!(parts.len(), 3);
        assert_eq!(text_of(&parts[0]), "Braces: {");
        match &parts[1] {
            StringPart::Interpolation { tokens, .. } => {
                assert_eq!(tokens.len(), 1);
                assert_eq!(tokens[0].kind, ident("name"));
            }
            _ => panic!("expected interpolation"),
        }
        assert_eq!(text_of(&parts[2]), "}");
    }

    #[test]
    fn nested_strings_inside_interpolation() {
        let parts = string_parts(r#""a {greet("b {c}")} d""#);
        assert_eq!(parts.len(), 3);
        let StringPart::Interpolation { tokens, .. } = &parts[1] else {
            panic!()
        };
        assert_eq!(tokens.len(), 4);
        let TokenKind::String(inner) = &tokens[2].kind else {
            panic!("expected nested string")
        };
        assert_eq!(inner.len(), 2);
    }

    #[test]
    fn string_errors() {
        assert_eq!(errors("\"open\n"), vec!["E_UNTERMINATED_STRING"]);
        assert_eq!(errors("\"a { b\""), vec!["E_UNTERMINATED_STRING"]);
        assert_eq!(errors("\"{}\""), vec!["E_EMPTY_INTERPOLATION"]);
        assert_eq!(errors("\"a } b\""), vec!["E_SINGLE_BRACE"]);
    }

    #[test]
    fn unicode_text_in_strings() {
        let parts = string_parts("\"héllo wörld ✓\"");
        assert_eq!(text_of(&parts[0]), "héllo wörld ✓");
    }
}
