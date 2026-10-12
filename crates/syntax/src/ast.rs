//! Abstract syntax tree.
//!
//! Every expression carries an [`ExprId`] that later phases use as a key for
//! side tables (types, ownership decisions, resolved names).

use crate::source::{FileId, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(pub u32);

#[derive(Clone, Debug, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Module {
    pub file: FileId,
    pub stmts: Vec<Stmt>,
    /// One past the largest expression id allocated in this module.
    pub expr_count: u32,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// No modifier: public for module-level items.
    Default,
    Public,
    Private,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclKind {
    /// `variable name = value`
    Variable,
    /// `constant NAME = value`
    Constant,
    /// `name: Type = value`
    Annotated,
}

#[derive(Clone, Debug)]
pub struct Declaration {
    pub kind: DeclKind,
    pub visibility: Visibility,
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamMode {
    Read,
    Update,
    Own,
}

#[derive(Clone, Debug)]
pub struct Param {
    /// Explicit `update` or `own`; `None` means the default for the parameter.
    pub mode: Option<(ParamMode, Span)>,
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub visibility: Visibility,
    /// `None` for function literals.
    pub name: Option<Ident>,
    pub generics: Vec<Ident>,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Block,
    /// Span of the header, from `function` to the end of the signature.
    pub header_span: Span,
}

#[derive(Clone, Debug)]
pub struct IfBranch {
    pub condition: Expr,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct CatchClause {
    pub binding: Ident,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: Ident,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug)]
pub struct Variant {
    pub name: Ident,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum PatternKind {
    Wildcard,
    Literal(Expr),
    /// A binding or a payload-free variant, decided during checking.
    Name(Ident),
    Variant { name: Ident, args: Vec<Pattern> },
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    Expr(Expr),
    Assign { target: Expr, value: Expr },
    CompoundAssign { target: Expr, op: BinaryOp, op_span: Span, value: Expr },
    Declare(Declaration),
    If { branches: Vec<IfBranch>, else_body: Option<Block> },
    While { condition: Expr, body: Block },
    For { bindings: Vec<Ident>, iterable: Expr, body: Block },
    Break,
    Continue,
    Return(Option<Expr>),
    Function(Box<Function>),
    Try { body: Block, catch: Option<CatchClause>, finally: Option<Block> },
    TypeDecl { visibility: Visibility, name: Ident, generics: Vec<Ident>, fields: Vec<Field> },
    EnumDecl { visibility: Visibility, name: Ident, generics: Vec<Ident>, variants: Vec<Variant> },
    Match { scrutinee: Expr, arms: Vec<MatchArm> },
    /// Syntax recognised so it can be reported precisely, but not parsed further
    /// (for example `foreign` blocks before the systems milestone).
    Unavailable { feature: &'static str, milestone: &'static str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Equal,
    NotEqual,
    Less,
    Greater,
    LessEqual,
    GreaterEqual,
    And,
    Or,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Subtract => "-",
            BinaryOp::Multiply => "*",
            BinaryOp::Divide => "/",
            BinaryOp::Remainder => "%",
            BinaryOp::Equal => "==",
            BinaryOp::NotEqual => "!=",
            BinaryOp::Less => "<",
            BinaryOp::Greater => ">",
            BinaryOp::LessEqual => "<=",
            BinaryOp::GreaterEqual => ">=",
            BinaryOp::And => "and",
            BinaryOp::Or => "or",
        }
    }

    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinaryOp::Equal
                | BinaryOp::NotEqual
                | BinaryOp::Less
                | BinaryOp::Greater
                | BinaryOp::LessEqual
                | BinaryOp::GreaterEqual
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Negate,
    Not,
}

#[derive(Clone, Debug)]
pub enum StringSegment {
    Text(String),
    Interpolation(Expr),
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub id: ExprId,
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Integer { value: u64, unsigned: bool },
    Float(f64),
    String(Vec<StringSegment>),
    Boolean(bool),
    Nothing,
    Name(Ident),
    /// `[a, b]`, including the empty literal `[]`.
    Array(Vec<Expr>),
    /// `["key" = value, ...]`
    Dict(Vec<(Expr, Expr)>),
    /// `[field = value, ...]`
    Record(Vec<(Ident, Expr)>),
    Call { callee: Box<Expr>, generic_args: Vec<TypeExpr>, args: Vec<Expr> },
    Index { target: Box<Expr>, index: Box<Expr> },
    Member { target: Box<Expr>, name: Ident },
    Unary { op: UnaryOp, operand: Box<Expr> },
    Binary { op: BinaryOp, lhs: Box<Expr>, rhs: Box<Expr> },
    /// `value is error`
    IsError(Box<Expr>),
    /// `try expression`
    TryExpr(Box<Expr>),
    Function(Box<Function>),
    Import { path: String, path_span: Span },
    /// Placeholder produced after a syntax error.
    Error,
}

#[derive(Clone, Debug)]
pub struct TypeExpr {
    pub kind: TypeExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TypeExprKind {
    Named { name: Ident, args: Vec<TypeExpr> },
    Union(Vec<TypeExpr>),
    Error,
}
