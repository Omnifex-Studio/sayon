//! Token definitions produced by the lexer.

use crate::source::Span;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Keyword {
    And,
    Async,
    Await,
    Break,
    Case,
    Catch,
    Constant,
    Continue,
    Else,
    Enum,
    False,
    Finally,
    For,
    Foreign,
    Function,
    If,
    Import,
    In,
    Is,
    Match,
    Not,
    Nothing,
    Or,
    Own,
    Private,
    Public,
    Return,
    True,
    Try,
    Type,
    Unsafe,
    Update,
    Variable,
    While,
}

impl Keyword {
    pub fn lookup(text: &str) -> Option<Keyword> {
        use Keyword::*;
        Some(match text {
            "and" => And,
            "async" => Async,
            "await" => Await,
            "break" => Break,
            "case" => Case,
            "catch" => Catch,
            "constant" => Constant,
            "continue" => Continue,
            "else" => Else,
            "enum" => Enum,
            "false" => False,
            "finally" => Finally,
            "for" => For,
            "foreign" => Foreign,
            "function" => Function,
            "if" => If,
            "import" => Import,
            "in" => In,
            "is" => Is,
            "match" => Match,
            "not" => Not,
            "nothing" => Nothing,
            "or" => Or,
            "own" => Own,
            "private" => Private,
            "public" => Public,
            "return" => Return,
            "true" => True,
            "try" => Try,
            "type" => Type,
            "unsafe" => Unsafe,
            "update" => Update,
            "variable" => Variable,
            "while" => While,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        use Keyword::*;
        match self {
            And => "and",
            Async => "async",
            Await => "await",
            Break => "break",
            Case => "case",
            Catch => "catch",
            Constant => "constant",
            Continue => "continue",
            Else => "else",
            Enum => "enum",
            False => "false",
            Finally => "finally",
            For => "for",
            Foreign => "foreign",
            Function => "function",
            If => "if",
            Import => "import",
            In => "in",
            Is => "is",
            Match => "match",
            Not => "not",
            Nothing => "nothing",
            Or => "or",
            Own => "own",
            Private => "private",
            Public => "public",
            Return => "return",
            True => "true",
            Try => "try",
            Type => "type",
            Unsafe => "unsafe",
            Update => "update",
            Variable => "variable",
            While => "while",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum StringPart {
    Text {
        text: String,
        span: Span,
    },
    /// Tokens of an interpolated expression, without the surrounding braces.
    Interpolation {
        tokens: Vec<Token>,
        span: Span,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    Identifier(String),
    Keyword(Keyword),
    Integer { value: u64, unsigned: bool },
    Float(f64),
    String(Vec<StringPart>),

    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    Comma,
    Colon,
    Dot,
    Ellipsis,
    Arrow,

    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Assign,
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    PercentAssign,
    Equal,
    NotEqual,
    Less,
    Greater,
    LessEqual,
    GreaterEqual,

    EndOfFile,
}

impl TokenKind {
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Identifier(name) => format!("identifier `{name}`"),
            TokenKind::Keyword(keyword) => format!("keyword `{}`", keyword.as_str()),
            TokenKind::Integer { .. } => "integer literal".to_string(),
            TokenKind::Float(_) => "float literal".to_string(),
            TokenKind::String(_) => "string literal".to_string(),
            TokenKind::EndOfFile => "end of file".to_string(),
            other => format!("`{}`", other.symbol()),
        }
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            TokenKind::LeftParen => "(",
            TokenKind::RightParen => ")",
            TokenKind::LeftBracket => "[",
            TokenKind::RightBracket => "]",
            TokenKind::Comma => ",",
            TokenKind::Colon => ":",
            TokenKind::Dot => ".",
            TokenKind::Ellipsis => "...",
            TokenKind::Arrow => "->",
            TokenKind::Plus => "+",
            TokenKind::Minus => "-",
            TokenKind::Star => "*",
            TokenKind::Slash => "/",
            TokenKind::Percent => "%",
            TokenKind::Assign => "=",
            TokenKind::PlusAssign => "+=",
            TokenKind::MinusAssign => "-=",
            TokenKind::StarAssign => "*=",
            TokenKind::SlashAssign => "/=",
            TokenKind::PercentAssign => "%=",
            TokenKind::Equal => "==",
            TokenKind::NotEqual => "!=",
            TokenKind::Less => "<",
            TokenKind::Greater => ">",
            TokenKind::LessEqual => "<=",
            TokenKind::GreaterEqual => ">=",
            _ => "",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// True for the first token on its physical line.
    pub line_start: bool,
    /// Indentation column of the line, meaningful when `line_start` is set.
    pub indent: u32,
}

impl Token {
    pub fn is_keyword(&self, keyword: Keyword) -> bool {
        self.kind == TokenKind::Keyword(keyword)
    }
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// A comment preserved for the formatter.
#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub span: Span,
    pub text: String,
    pub block: bool,
    /// True when nothing but whitespace precedes the comment on its line.
    pub own_line: bool,
}
