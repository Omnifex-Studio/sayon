//! The checked program: names resolved, types inferred, ownership decided.
//!
//! Generic functions keep `Type::Param` in their bodies; code generation
//! instantiates them per concrete argument list.

use crate::types::{Type, TypeDef};
use sayon_syntax::Span;
use sayon_syntax::ast::{BinaryOp, ParamMode, UnaryOp};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FuncId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConstId(pub u32);

#[derive(Debug)]
pub struct Program {
    pub functions: Vec<Function>,
    pub constants: Vec<Constant>,
    /// Constants in initialization order.
    pub init_order: Vec<ConstId>,
    pub entry: FuncId,
    /// Test blocks of the entry module, run by `sayon test`.
    pub tests: Vec<(String, FuncId)>,
    /// Declared structures and enums, indexed by `TypeId`.
    pub types: Vec<TypeDef>,
}

impl Program {
    pub fn function(&self, id: FuncId) -> &Function {
        &self.functions[id.0 as usize]
    }

    pub fn constant(&self, id: ConstId) -> &Constant {
        &self.constants[id.0 as usize]
    }
}

#[derive(Debug)]
pub struct Constant {
    pub name: String,
    pub span: Span,
    pub ty: Type,
    pub value: Expr,
    /// Locals used while evaluating the initializer.
    pub locals: Vec<Local>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    Param,
    Variable,
    Constant,
    LoopVariable,
    CaughtError,
    /// A name bound by a `case` pattern.
    CaseBinding,
    /// A function literal's copy of a variable of the function around it,
    /// kept in the closure's environment between calls.
    Captured,
}

#[derive(Debug, Clone)]
pub struct Local {
    pub name: String,
    pub ty: Type,
    pub span: Span,
    pub kind: LocalKind,
    /// Loop variables over collections, and case bindings of a matched value
    /// the match does not own, borrow what they name instead of owning it.
    pub borrowed: bool,
    /// Introduced by assigning to a new name, without `variable`.
    pub implicit: bool,
}

#[derive(Debug)]
pub struct Param {
    pub local: LocalId,
    pub mode: ParamMode,
    /// Whether the mode was written in the source or inferred from the body.
    pub explicit_mode: bool,
    pub ty: Type,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Function {
    pub name: String,
    pub span: Span,
    /// Number of generic parameters (`Type::Param(0..generic_count)`).
    pub generic_count: u32,
    /// The generic parameters only numbers can fill, so that their values
    /// are Copy.
    pub numeric_generics: Vec<bool>,
    pub params: Vec<Param>,
    pub ret: Type,
    pub locals: Vec<Local>,
    pub body: Block,
    pub is_entry: bool,
    /// The body of a function literal. Its compiled form takes the closure's
    /// environment before its parameters.
    pub closure: bool,
    /// For a closure, the locals of the body that hold captured values, in
    /// the order of the environment.
    pub captures: Vec<LocalId>,
}

impl Function {
    pub fn local(&self, id: LocalId) -> &Local {
        &self.locals[id.0 as usize]
    }
}

#[derive(Debug, Default)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// Locals introduced directly in this block, in declaration order.
    pub locals: Vec<LocalId>,
}

#[derive(Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum StmtKind {
    /// Introduces and initializes a local.
    Let {
        local: LocalId,
        value: Expr,
    },
    /// Replaces the value stored in a place, destroying the old value.
    Assign {
        place: Place,
        value: Expr,
    },
    /// `place op= value`, evaluating the place once.
    CompoundAssign {
        place: Place,
        op: BinaryOp,
        value: Expr,
    },
    Expr(Expr),
    If {
        condition: Expr,
        then_block: Block,
        else_block: Option<Block>,
    },
    While {
        condition: Expr,
        body: Block,
    },
    /// Iterates a range, an array, or a dictionary (keys, or keys and values),
    /// according to the iterable's type.
    For {
        bindings: Vec<LocalId>,
        iterable: Expr,
        body: Block,
    },
    Break,
    Continue,
    Return(Option<Expr>),
    Try {
        body: Block,
        catch: Option<(LocalId, Block)>,
        finally: Option<Block>,
    },
    /// A nested lexical scope.
    Block(Block),
    /// Runs the first arm whose pattern matches. `owned` means the match
    /// owns the value (a temporary, or one transferred with `move`), so the
    /// unguarded arms' bindings take ownership of its parts.
    Match {
        scrutinee: Expr,
        owned: bool,
        arms: Vec<MatchArm>,
    },
}

#[derive(Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Block,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Wildcard,
    /// Binds the value to a new local. At the top of a pattern the local's
    /// type can be a narrower union (or one member) than the value's, when
    /// earlier arms handled the other members.
    Binding(LocalId),
    /// Equal to a literal.
    Literal(Expr),
    /// One variant of the enum `ty`, with a pattern for each field.
    Variant {
        ty: Type,
        variant: u32,
        fields: Vec<Pattern>,
    },
    /// A union holding `member`, with a pattern for that value.
    Member {
        member: Type,
        inner: Box<Pattern>,
    },
    /// A structure of type `ty`, with a pattern for each field.
    Record {
        ty: Type,
        fields: Vec<Pattern>,
    },
    /// Any of the alternatives, which all bind the same locals.
    Or(Vec<Pattern>),
}

impl Pattern {
    /// The locals the pattern binds, in order.
    pub fn bindings(&self, out: &mut Vec<LocalId>) {
        match self {
            Pattern::Binding(local) => out.push(*local),
            Pattern::Variant { fields, .. } | Pattern::Record { fields, .. } => {
                fields.iter().for_each(|f| f.bindings(out))
            }
            Pattern::Member { inner, .. } => inner.bindings(out),
            Pattern::Or(alternatives) => {
                if let Some(first) = alternatives.first() {
                    first.bindings(out);
                }
            }
            Pattern::Wildcard | Pattern::Literal(_) => {}
        }
    }
}

#[derive(Debug, Clone)]
pub enum Place {
    Local(LocalId),
    /// `target[index]` on an array or dictionary.
    Index {
        target: Box<Place>,
        target_ty: Type,
        index: Box<Expr>,
    },
    /// `target.name` on a structure.
    Field {
        target: Box<Place>,
        target_ty: Type,
        name: String,
    },
    /// A local known by narrowing to hold `ty`: a union member, or (with a
    /// variant) the one field of that variant of an enum.
    Narrow {
        target: Box<Place>,
        ty: Type,
        variant: Option<u32>,
    },
}

impl Place {
    pub fn root(&self) -> LocalId {
        match self {
            Place::Local(local) => *local,
            Place::Index { target, .. }
            | Place::Field { target, .. }
            | Place::Narrow { target, .. } => target.root(),
        }
    }
}

/// How an expression reading a local uses its value; decided by the
/// ownership checker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usage {
    /// Not yet decided.
    Unset,
    /// Duplicate a Copy value.
    Copy,
    /// Transfer ownership out of the local.
    Move,
    /// Temporary read-only access.
    Borrow,
    /// Temporary exclusive access.
    BorrowUpdate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Print,
    Length,
    ByteLength,
    TypeOf,
    Assert,
    Range,
    ToNumber,
    ToString,
    Push,
    Pop,
    HasKey,
    ReadFile,
    WriteFile,
    Clone,
    Error,
    Raise,
    /// Checked numeric conversion to the expression's type.
    Convert,
    /// A host primitive, callable only from standard library modules.
    Host(sayon_runtime::host::Host),
}

impl Builtin {
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Print => "print",
            Builtin::Length => "length",
            Builtin::ByteLength => "bytelength",
            Builtin::TypeOf => "typeof",
            Builtin::Assert => "assert",
            Builtin::Range => "range",
            Builtin::ToNumber => "tonumber",
            Builtin::ToString => "tostring",
            Builtin::Push => "push",
            Builtin::Pop => "pop",
            Builtin::HasKey => "haskey",
            Builtin::ReadFile => "readfile",
            Builtin::WriteFile => "writefile",
            Builtin::Clone => "clone",
            Builtin::Error => "error",
            Builtin::Raise => "raise",
            Builtin::Convert => "conversion",
            Builtin::Host(host) => host.name(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Segment {
    Text(String),
    Value(Expr),
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Integer(i128),
    Float(f64),
    Boolean(bool),
    String(String),
    Nothing,
    Local {
        local: LocalId,
        usage: Usage,
    },
    Constant(ConstId),
    Call {
        function: FuncId,
        type_args: Vec<Type>,
        args: Vec<Arg>,
    },
    Builtin {
        builtin: Builtin,
        args: Vec<Arg>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Array(Vec<Expr>),
    Dict(Vec<(Expr, Expr)>),
    /// A structure value of type `ty`: each field's index and value, in the
    /// order the source wrote them (which is the evaluation order).
    Record(Vec<(u32, Expr)>),
    /// A value of one variant of the enum in `ty`.
    Variant {
        variant: u32,
        args: Vec<Expr>,
    },
    /// Reads the field `name` of a structure.
    Field {
        value: Box<Expr>,
        name: String,
    },
    /// A closure: the function literal's body, and the values it captures
    /// from the enclosing function, in the order of its `captures`.
    Closure {
        function: FuncId,
        captures: Vec<Capture>,
    },
    /// A named function used as a value.
    FunctionRef {
        function: FuncId,
        type_args: Vec<Type>,
    },
    /// Calls a function value.
    CallValue {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    /// `try value`: the `Outcome` in `ty`, `success(value)`, or `failure(error)`
    /// when evaluating the value raises an error.
    TryExpr(Box<Expr>),
    /// Tests whether an enum value is the given variant.
    IsVariant {
        value: Box<Expr>,
        variant: u32,
    },
    /// Reads the one field of an enum value known (by narrowing) to be the
    /// given variant, such as the Error of an `Outcome` that `is error`.
    VariantPayload {
        value: Box<Expr>,
        variant: u32,
    },
    /// Reads `target[index]`.
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    Interpolate(Vec<Segment>),
    /// Wraps a value as a member of the union type in `ty`.
    Inject(Box<Expr>),
    /// Reads the payload of a union known (by narrowing) to hold `ty`.
    Narrow(Box<Expr>),
    /// Tests whether a union currently holds `member`.
    IsMember {
        value: Box<Expr>,
        member: Type,
    },
    /// `typeof` of a union value, decided at run time.
    UnionTypeName(Box<Expr>),
    /// A field of an Error value, such as `problem.message`.
    ErrorField {
        value: Box<Expr>,
        field: ErrorField,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorField {
    Message,
    Code,
}

/// A value a closure takes from the enclosing function when it is made.
#[derive(Debug, Clone)]
pub struct Capture {
    /// The enclosing function's variable, as it is read there.
    pub value: Expr,
    /// The closure changes it, so it takes the variable over even when the
    /// value is Copy.
    pub mutated: bool,
}

#[derive(Debug, Clone)]
pub struct Arg {
    pub expr: Expr,
    pub mode: ParamMode,
}

impl Expr {
    /// The expressions this one is made of, in evaluation order.
    pub fn children(&self) -> Vec<&Expr> {
        match &self.kind {
            ExprKind::Call { args, .. } | ExprKind::Builtin { args, .. } => {
                args.iter().map(|arg| &arg.expr).collect()
            }
            ExprKind::CallValue { callee, args } => std::iter::once(&**callee)
                .chain(args.iter().map(|arg| &arg.expr))
                .collect(),
            ExprKind::Closure { captures, .. } => {
                captures.iter().map(|capture| &capture.value).collect()
            }
            ExprKind::Binary { lhs, rhs, .. } => vec![lhs, rhs],
            ExprKind::Array(items) | ExprKind::Variant { args: items, .. } => {
                items.iter().collect()
            }
            ExprKind::Record(fields) => fields.iter().map(|(_, value)| value).collect(),
            ExprKind::Dict(entries) => entries.iter().flat_map(|(k, v)| [k, v]).collect(),
            ExprKind::Index { target, index } => vec![target, index],
            ExprKind::Interpolate(segments) => segments
                .iter()
                .filter_map(|segment| match segment {
                    Segment::Value(value) => Some(value),
                    Segment::Text(_) => None,
                })
                .collect(),
            ExprKind::Unary { operand: inner, .. }
            | ExprKind::Field { value: inner, .. }
            | ExprKind::Inject(inner)
            | ExprKind::Narrow(inner)
            | ExprKind::UnionTypeName(inner)
            | ExprKind::IsMember { value: inner, .. }
            | ExprKind::ErrorField { value: inner, .. }
            | ExprKind::TryExpr(inner)
            | ExprKind::IsVariant { value: inner, .. }
            | ExprKind::VariantPayload { value: inner, .. } => vec![inner],
            ExprKind::Integer(_)
            | ExprKind::Float(_)
            | ExprKind::Boolean(_)
            | ExprKind::String(_)
            | ExprKind::Nothing
            | ExprKind::Local { .. }
            | ExprKind::Constant(_)
            | ExprKind::FunctionRef { .. } => Vec::new(),
        }
    }
}

/// Calls `f` on every expression in the block, parents before children.
pub fn visit_block_mut(block: &mut Block, f: &mut dyn FnMut(&mut Expr)) {
    for stmt in &mut block.stmts {
        visit_stmt_mut(stmt, f);
    }
}

fn visit_place_mut(place: &mut Place, f: &mut dyn FnMut(&mut Expr)) {
    match place {
        Place::Local(_) => {}
        Place::Index { target, index, .. } => {
            visit_place_mut(target, f);
            visit_expr_mut(index, f);
        }
        Place::Field { target, .. } | Place::Narrow { target, .. } => visit_place_mut(target, f),
    }
}

pub fn visit_stmt_mut(stmt: &mut Stmt, f: &mut dyn FnMut(&mut Expr)) {
    match &mut stmt.kind {
        StmtKind::Let { value, .. } | StmtKind::Expr(value) => visit_expr_mut(value, f),
        StmtKind::Assign { place, value } | StmtKind::CompoundAssign { place, value, .. } => {
            visit_place_mut(place, f);
            visit_expr_mut(value, f);
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            visit_expr_mut(condition, f);
            visit_block_mut(then_block, f);
            if let Some(block) = else_block {
                visit_block_mut(block, f);
            }
        }
        StmtKind::While { condition, body } => {
            visit_expr_mut(condition, f);
            visit_block_mut(body, f);
        }
        StmtKind::For { iterable, body, .. } => {
            visit_expr_mut(iterable, f);
            visit_block_mut(body, f);
        }
        StmtKind::Return(value) => {
            if let Some(value) = value {
                visit_expr_mut(value, f);
            }
        }
        StmtKind::Try {
            body,
            catch,
            finally,
        } => {
            visit_block_mut(body, f);
            if let Some((_, block)) = catch {
                visit_block_mut(block, f);
            }
            if let Some(block) = finally {
                visit_block_mut(block, f);
            }
        }
        StmtKind::Block(block) => visit_block_mut(block, f),
        StmtKind::Match {
            scrutinee, arms, ..
        } => {
            visit_expr_mut(scrutinee, f);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    visit_expr_mut(guard, f);
                }
                visit_block_mut(&mut arm.body, f);
            }
        }
        StmtKind::Break | StmtKind::Continue => {}
    }
}

pub fn visit_expr_mut(expr: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
    f(expr);
    match &mut expr.kind {
        ExprKind::Call { args, .. } | ExprKind::Builtin { args, .. } => {
            for arg in args {
                visit_expr_mut(&mut arg.expr, f);
            }
        }
        ExprKind::CallValue { callee, args } => {
            visit_expr_mut(callee, f);
            for arg in args {
                visit_expr_mut(&mut arg.expr, f);
            }
        }
        ExprKind::Closure { captures, .. } => {
            for capture in captures {
                visit_expr_mut(&mut capture.value, f);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            visit_expr_mut(lhs, f);
            visit_expr_mut(rhs, f);
        }
        ExprKind::Unary { operand, .. } => visit_expr_mut(operand, f),
        ExprKind::Array(items) | ExprKind::Variant { args: items, .. } => {
            items.iter_mut().for_each(|item| visit_expr_mut(item, f))
        }
        ExprKind::Record(fields) => fields
            .iter_mut()
            .for_each(|(_, value)| visit_expr_mut(value, f)),
        ExprKind::Field { value, .. } => visit_expr_mut(value, f),
        ExprKind::Dict(entries) => entries.iter_mut().for_each(|(key, value)| {
            visit_expr_mut(key, f);
            visit_expr_mut(value, f);
        }),
        ExprKind::Index { target, index } => {
            visit_expr_mut(target, f);
            visit_expr_mut(index, f);
        }
        ExprKind::Interpolate(segments) => {
            for segment in segments {
                if let Segment::Value(value) = segment {
                    visit_expr_mut(value, f);
                }
            }
        }
        ExprKind::Inject(inner)
        | ExprKind::Narrow(inner)
        | ExprKind::UnionTypeName(inner)
        | ExprKind::IsMember { value: inner, .. }
        | ExprKind::ErrorField { value: inner, .. }
        | ExprKind::TryExpr(inner)
        | ExprKind::IsVariant { value: inner, .. }
        | ExprKind::VariantPayload { value: inner, .. } => visit_expr_mut(inner, f),
        ExprKind::Integer(_)
        | ExprKind::Float(_)
        | ExprKind::Boolean(_)
        | ExprKind::String(_)
        | ExprKind::Nothing
        | ExprKind::Local { .. }
        | ExprKind::Constant(_)
        | ExprKind::FunctionRef { .. } => {}
    }
}
