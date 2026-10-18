//! `sayon lint`: warnings about code that compiles but is probably not what
//! was meant, or does not follow the naming conventions.
//!
//! The lints run on a checked program and the modules it was built from; the
//! standard library is not linted. A warning is suppressed by a comment on
//! its line or the line above naming it, such as `# allow(unused_variable)`.

use crate::ModuleSource;
use crate::hir::{self, Block, Expr, ExprKind, LocalId, LocalKind, Pattern, StmtKind};
use sayon_syntax::ast::{self, DeclKind, PatternKind, TypeExprKind, Visibility};
use sayon_syntax::{Diagnostic, Diagnostics, FileId, Severity, SourceMap, Span};
use std::collections::{HashMap, HashSet};

/// The warnings `sayon lint` adds to those of the compiler.
pub const LINTS: &[&str] = &[
    "W_UNUSED_VARIABLE",
    "W_UNUSED_PARAMETER",
    "W_UNUSED_IMPORT",
    "W_UNUSED_PRIVATE",
    "W_NAMING",
    "W_SHADOWED_VARIABLE",
    "W_SUSPICIOUS_DECLARATION",
    "W_UNKNOWN_LINT",
];

/// Warnings the compiler reports on its own, which can be suppressed too.
const COMPILER_WARNINGS: &[&str] = &[
    "W_UNUSED_OUTCOME",
    "W_UNREACHABLE_CASE",
    "W_SHADOWED_BUILTIN",
];

/// The name used in `# allow(...)` for a warning code.
fn lint_name(code: &str) -> String {
    code.trim_start_matches("W_").to_ascii_lowercase()
}

/// Lints the modules of a program that are not part of the standard library.
pub fn lint(program: &hir::Program, modules: &[ModuleSource]) -> Vec<Diagnostic> {
    let files: HashSet<FileId> = modules
        .iter()
        .filter(|m| !m.standard && !m.dependency)
        .map(|m| m.ast.file)
        .collect();
    let mut out = Vec::new();
    for module in modules.iter().filter(|m| !m.standard && !m.dependency) {
        lint_module(module.ast, &mut out);
    }
    for function in &program.functions {
        if files.contains(&function.span.file) && !function.closure {
            FunctionLint::new(program, &mut out).run(function, &mut Vec::new());
        }
    }
    out
}

// ----- names in the source -------------------------------------------------------

fn is_snake(name: &str) -> bool {
    let name = name.trim_start_matches('_');
    name.is_empty()
        || (!name.starts_with(|c: char| c.is_ascii_digit())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
}

fn is_pascal(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase()) && !name.contains('_')
}

fn is_upper(name: &str) -> bool {
    name.chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The words of a name in any case style.
fn words(name: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (index, &c) in chars.iter().enumerate() {
        let boundary = c == '_'
            || (c.is_ascii_uppercase()
                && index > 0
                && (chars[index - 1].is_ascii_lowercase()
                    || chars.get(index + 1).is_some_and(|n| n.is_ascii_lowercase())));
        if boundary && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        if c != '_' {
            current.push(c.to_ascii_lowercase());
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn to_snake(name: &str) -> String {
    words(name).join("_")
}

fn to_pascal(name: &str) -> String {
    words(name)
        .iter()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn to_upper(name: &str) -> String {
    to_snake(name).to_ascii_uppercase()
}

fn naming(
    what: &str,
    name: &ast::Ident,
    style: &str,
    suggestion: String,
    out: &mut Vec<Diagnostic>,
) {
    out.push(
        Diagnostic::warning(
            "W_NAMING",
            format!("{what} `{}` should be written in {style}", name.name),
        )
        .primary(name.span, "")
        .help(format!("rename it `{suggestion}`")),
    );
}

fn check_snake(what: &str, name: &ast::Ident, out: &mut Vec<Diagnostic>) {
    if !is_snake(&name.name) {
        naming(what, name, "snake_case", to_snake(&name.name), out);
    }
}

fn check_pascal(what: &str, name: &ast::Ident, out: &mut Vec<Diagnostic>) {
    if !is_pascal(&name.name) {
        naming(what, name, "PascalCase", to_pascal(&name.name), out);
    }
}

/// Lints that need only the module's source: names of declarations, and
/// imports and private items the module never uses.
fn lint_module(module: &ast::Module, out: &mut Vec<Diagnostic>) {
    let mut used = HashSet::new();
    for stmt in &module.stmts {
        stmt_names(stmt, &mut used);
    }
    for stmt in &module.stmts {
        match &stmt.kind {
            ast::StmtKind::Assign { target, value } => {
                if let (ast::ExprKind::Name(alias), ast::ExprKind::Import { .. }) =
                    (&target.kind, &value.kind)
                    && !used.contains(&alias.name)
                    && !alias.name.starts_with('_')
                {
                    out.push(
                        Diagnostic::warning(
                            "W_UNUSED_IMPORT",
                            format!("the module `{}` is imported but never used", alias.name),
                        )
                        .primary(target.span, "")
                        .help("remove the import"),
                    );
                }
            }
            ast::StmtKind::Declare(declaration) => {
                if declaration.kind == DeclKind::Constant && !is_upper(&declaration.name.name) {
                    let suggestion = to_upper(&declaration.name.name);
                    naming(
                        "the constant",
                        &declaration.name,
                        "UPPER_SNAKE_CASE",
                        suggestion,
                        out,
                    );
                }
                if declaration.visibility == Visibility::Private {
                    unused_private(&declaration.name, "constant", &used, out);
                }
            }
            ast::StmtKind::Function(function) => {
                if let Some(name) = &function.name
                    && function.visibility == Visibility::Private
                {
                    unused_private(name, "function", &used, out);
                }
            }
            ast::StmtKind::TypeDecl {
                visibility: Visibility::Private,
                name,
                ..
            }
            | ast::StmtKind::EnumDecl {
                visibility: Visibility::Private,
                name,
                ..
            } => unused_private(name, "type", &used, out),
            _ => {}
        }
        declaration_names(stmt, out);
    }
}

fn unused_private(
    name: &ast::Ident,
    what: &str,
    used: &HashSet<String>,
    out: &mut Vec<Diagnostic>,
) {
    if !used.contains(&name.name) && !name.name.starts_with('_') {
        out.push(
            Diagnostic::warning(
                "W_UNUSED_PRIVATE",
                format!("the private {what} `{}` is never used", name.name),
            )
            .primary(name.span, "")
            .help("remove it, or make it public if other modules should use it"),
        );
    }
}

/// Naming conventions for functions, types, variants, and fields.
fn declaration_names(stmt: &ast::Stmt, out: &mut Vec<Diagnostic>) {
    match &stmt.kind {
        ast::StmtKind::Function(function) => {
            if let Some(name) = &function.name {
                check_snake("the function", name, out);
            }
            block_declarations(&function.body, out);
        }
        ast::StmtKind::TypeDecl { name, fields, .. } => {
            check_pascal("the type", name, out);
            for field in fields {
                check_snake("the field", &field.name, out);
            }
        }
        ast::StmtKind::EnumDecl { name, variants, .. } => {
            check_pascal("the enum", name, out);
            for variant in variants {
                check_snake("the variant", &variant.name, out);
                for field in &variant.fields {
                    check_snake("the field", &field.name, out);
                }
            }
        }
        ast::StmtKind::If {
            branches,
            else_body,
        } => {
            for branch in branches {
                block_declarations(&branch.body, out);
            }
            if let Some(body) = else_body {
                block_declarations(body, out);
            }
        }
        ast::StmtKind::While { body, .. }
        | ast::StmtKind::For { body, .. }
        | ast::StmtKind::Test { body, .. } => block_declarations(body, out),
        ast::StmtKind::Try {
            body,
            catch,
            finally,
        } => {
            block_declarations(body, out);
            if let Some(catch) = catch {
                block_declarations(&catch.body, out);
            }
            if let Some(finally) = finally {
                block_declarations(finally, out);
            }
        }
        ast::StmtKind::Match { arms, .. } => {
            for arm in arms {
                block_declarations(&arm.body, out);
            }
        }
        _ => {}
    }
}

fn block_declarations(block: &ast::Block, out: &mut Vec<Diagnostic>) {
    for stmt in &block.stmts {
        declaration_names(stmt, out);
    }
}

// Every name a module refers to, in expressions, types, and patterns.

fn stmt_names(stmt: &ast::Stmt, used: &mut HashSet<String>) {
    match &stmt.kind {
        ast::StmtKind::Expr(expr) | ast::StmtKind::Return(Some(expr)) => expr_names(expr, used),
        ast::StmtKind::Assign { target, value }
        | ast::StmtKind::CompoundAssign { target, value, .. } => {
            if !matches!(target.kind, ast::ExprKind::Name(_)) {
                expr_names(target, used);
            }
            expr_names(value, used);
        }
        ast::StmtKind::Declare(declaration) => {
            if let Some(ty) = &declaration.ty {
                type_names(ty, used);
            }
            expr_names(&declaration.value, used);
        }
        ast::StmtKind::If {
            branches,
            else_body,
        } => {
            for branch in branches {
                expr_names(&branch.condition, used);
                block_names(&branch.body, used);
            }
            if let Some(body) = else_body {
                block_names(body, used);
            }
        }
        ast::StmtKind::While { condition, body } => {
            expr_names(condition, used);
            block_names(body, used);
        }
        ast::StmtKind::For { iterable, body, .. } => {
            expr_names(iterable, used);
            block_names(body, used);
        }
        ast::StmtKind::Function(function) => function_names(function, used),
        ast::StmtKind::Try {
            body,
            catch,
            finally,
        } => {
            block_names(body, used);
            if let Some(catch) = catch {
                block_names(&catch.body, used);
            }
            if let Some(finally) = finally {
                block_names(finally, used);
            }
        }
        ast::StmtKind::TypeDecl { fields, .. } => {
            for field in fields {
                type_names(&field.ty, used);
            }
        }
        ast::StmtKind::EnumDecl { variants, .. } => {
            for field in variants.iter().flat_map(|v| &v.fields) {
                type_names(&field.ty, used);
            }
        }
        ast::StmtKind::Match { scrutinee, arms } => {
            expr_names(scrutinee, used);
            for arm in arms {
                pattern_names(&arm.pattern, used);
                if let Some(guard) = &arm.guard {
                    expr_names(guard, used);
                }
                block_names(&arm.body, used);
            }
        }
        ast::StmtKind::Test { body, .. } => block_names(body, used),
        ast::StmtKind::Return(None)
        | ast::StmtKind::Break
        | ast::StmtKind::Continue
        | ast::StmtKind::Unavailable { .. } => {}
    }
}

fn block_names(block: &ast::Block, used: &mut HashSet<String>) {
    for stmt in &block.stmts {
        stmt_names(stmt, used);
    }
}

fn function_names(function: &ast::Function, used: &mut HashSet<String>) {
    for param in &function.params {
        if let Some(ty) = &param.ty {
            type_names(ty, used);
        }
        if let Some(default) = &param.default {
            expr_names(default, used);
        }
    }
    if let Some(ret) = &function.ret {
        type_names(ret, used);
    }
    block_names(&function.body, used);
}

fn expr_names(expr: &ast::Expr, used: &mut HashSet<String>) {
    use ast::ExprKind as E;
    match &expr.kind {
        E::Name(name) => {
            used.insert(name.name.clone());
        }
        E::String(segments) => {
            for segment in segments {
                if let ast::StringSegment::Interpolation(value) = segment {
                    expr_names(value, used);
                }
            }
        }
        E::Array(items) => items.iter().for_each(|item| expr_names(item, used)),
        E::Dict(entries) => entries.iter().for_each(|(key, value)| {
            expr_names(key, used);
            expr_names(value, used);
        }),
        E::Record(fields) => {
            // Names in a field list can also be dictionary keys.
            for (name, value) in fields {
                used.insert(name.name.clone());
                expr_names(value, used);
            }
        }
        E::Call {
            callee,
            generic_args,
            args,
        } => {
            expr_names(callee, used);
            generic_args.iter().for_each(|ty| type_names(ty, used));
            args.iter().for_each(|arg| expr_names(arg, used));
        }
        E::Construct {
            callee,
            generic_args,
            fields,
        } => {
            expr_names(callee, used);
            generic_args.iter().for_each(|ty| type_names(ty, used));
            fields.iter().for_each(|(_, value)| expr_names(value, used));
        }
        E::Index { target, index } => {
            expr_names(target, used);
            expr_names(index, used);
        }
        E::Member { target, .. } => expr_names(target, used),
        E::Unary { operand, .. } => expr_names(operand, used),
        E::Binary { lhs, rhs, .. } => {
            expr_names(lhs, used);
            expr_names(rhs, used);
        }
        E::IsError(inner) | E::TryExpr(inner) => expr_names(inner, used),
        E::Function(function) => function_names(function, used),
        E::Integer { .. }
        | E::Float(_)
        | E::Boolean(_)
        | E::Nothing
        | E::Import { .. }
        | E::Error => {}
    }
}

fn type_names(ty: &ast::TypeExpr, used: &mut HashSet<String>) {
    match &ty.kind {
        TypeExprKind::Named { module, name, args } => {
            if let Some(module) = module {
                used.insert(module.name.clone());
            }
            used.insert(name.name.clone());
            args.iter().for_each(|arg| type_names(arg, used));
        }
        TypeExprKind::Union(members) => members.iter().for_each(|m| type_names(m, used)),
        TypeExprKind::Function { params, ret } => {
            params.iter().for_each(|(_, ty)| type_names(ty, used));
            if let Some(ret) = ret {
                type_names(ret, used);
            }
        }
        TypeExprKind::Error => {}
    }
}

fn pattern_names(pattern: &ast::Pattern, used: &mut HashSet<String>) {
    match &pattern.kind {
        PatternKind::Name(name) => {
            used.insert(name.name.clone());
        }
        PatternKind::Variant {
            qualifier,
            name,
            args,
        } => {
            used.extend(qualifier.iter().map(|q| q.name.clone()));
            used.insert(name.name.clone());
            args.iter().for_each(|arg| pattern_names(arg, used));
        }
        PatternKind::Fields {
            qualifier,
            name,
            fields,
        } => {
            used.extend(qualifier.iter().map(|q| q.name.clone()));
            used.insert(name.name.clone());
            fields.iter().for_each(|(_, p)| pattern_names(p, used));
        }
        PatternKind::Or(alternatives) => alternatives.iter().for_each(|a| pattern_names(a, used)),
        PatternKind::Literal(expr) => expr_names(expr, used),
        PatternKind::Wildcard => {}
    }
}

// ----- locals --------------------------------------------------------------------

/// Whether two names differ by one inserted, removed, replaced, or swapped
/// character.
fn one_edit_apart(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a == b || a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let (rest_a, rest_b) = (&a[prefix..], &b[prefix..]);
    match (rest_a.len(), rest_b.len()) {
        (x, y) if x == y => {
            rest_a[1..] == rest_b[1..]
                || (x >= 2
                    && rest_a[0] == rest_b[1]
                    && rest_a[1] == rest_b[0]
                    && rest_a[2..] == rest_b[2..])
        }
        (x, y) if x > y => rest_a[1..] == *rest_b,
        _ => *rest_a == rest_b[1..],
    }
}

/// The locals in scope: name, where declared, and block depth.
type Visible = Vec<(String, Span, usize)>;

/// Whether two names differ only in their digits, like `total` and `total2`.
fn only_digits_differ(a: &str, b: &str) -> bool {
    let strip = |s: &str| {
        s.chars()
            .filter(|c| !c.is_ascii_digit())
            .collect::<String>()
    };
    strip(a) == strip(b)
}

struct FunctionLint<'p, 'o> {
    program: &'p hir::Program,
    out: &'o mut Vec<Diagnostic>,
    /// How many blocks deep the walk is.
    depth: usize,
}

impl<'p, 'o> FunctionLint<'p, 'o> {
    fn new(program: &'p hir::Program, out: &'o mut Vec<Diagnostic>) -> Self {
        FunctionLint {
            program,
            out,
            depth: 0,
        }
    }

    /// Lints a function and the closures inside it. `visible` holds the
    /// names of the enclosing functions' locals, for a closure.
    fn run(&mut self, function: &'p hir::Function, visible: &mut Visible) {
        let mut reads: HashSet<LocalId> = HashSet::new();
        for param in &function.params {
            if let Some(default) = &param.default {
                each_expr(default, &mut |e| note_read(e, &mut reads));
            }
        }
        let mut changed = HashSet::new();
        each_block_expr(
            &function.body,
            &mut |e| note_read(e, &mut reads),
            &mut |local| {
                changed.insert(local);
            },
        );
        reads.extend(changed);

        for (index, local) in function.locals.iter().enumerate() {
            let id = LocalId(index as u32);
            self.naming(local);
            if local.name.starts_with('_') || reads.contains(&id) {
                continue;
            }
            let (code, message) = match local.kind {
                LocalKind::Param => (
                    "W_UNUSED_PARAMETER",
                    format!("the parameter `{}` is never used", local.name),
                ),
                LocalKind::Variable | LocalKind::Constant => (
                    "W_UNUSED_VARIABLE",
                    format!("`{}` is given a value that is never read", local.name),
                ),
                LocalKind::LoopVariable | LocalKind::CaseBinding | LocalKind::CaughtError => (
                    "W_UNUSED_VARIABLE",
                    format!("`{}` is never used", local.name),
                ),
                LocalKind::Captured | LocalKind::Hidden => continue,
            };
            self.out.push(
                Diagnostic::warning(code, message)
                    .primary(local.span, "")
                    .help(format!(
                        "remove it, or name it `_{}` if it is meant to be unused",
                        local.name
                    )),
            );
        }

        let mark = visible.len();
        for param in &function.params {
            let local = &function.locals[param.local.0 as usize];
            self.declare(&local.name, local.span, false, visible);
        }
        self.block(function, &function.body, visible);
        visible.truncate(mark);
    }

    fn naming(&mut self, local: &hir::Local) {
        if matches!(local.kind, LocalKind::Captured | LocalKind::Hidden) || is_snake(&local.name) {
            return;
        }
        // A local constant may be written as a module constant would be.
        if local.kind == LocalKind::Constant && is_upper(&local.name) {
            return;
        }
        let what = match local.kind {
            LocalKind::Param => "the parameter",
            _ => "the variable",
        };
        let ident = ast::Ident {
            name: local.name.clone(),
            span: local.span,
        };
        naming(what, &ident, "snake_case", to_snake(&local.name), self.out);
    }

    /// Checks a new name against the names already visible, then adds it.
    fn declare(&mut self, name: &str, span: Span, implicit: bool, visible: &mut Visible) {
        if !name.starts_with('_') {
            if let Some((_, outer, _)) = visible.iter().rev().find(|(n, _, _)| n == name) {
                self.out.push(
                    Diagnostic::warning(
                        "W_SHADOWED_VARIABLE",
                        format!("`{name}` hides a variable of the same name"),
                    )
                    .primary(span, "")
                    .secondary(*outer, "the hidden variable")
                    .help("choose another name, so that each name means one thing"),
                );
            } else if implicit
                && name.chars().count() >= 3
                && let Some((close, outer, _)) = visible.iter().rev().find(|(n, _, depth)| {
                    // A typo usually updates a variable of an enclosing block.
                    *depth < self.depth && one_edit_apart(n, name) && !only_digits_differ(n, name)
                })
            {
                self.out.push(
                    Diagnostic::warning(
                        "W_SUSPICIOUS_DECLARATION",
                        format!("this makes a new variable `{name}`, almost the same as `{close}`"),
                    )
                    .primary(span, "")
                    .secondary(*outer, format!("`{close}` is declared here"))
                    .help(format!(
                        "to change `{close}`, spell its name exactly; to make a new variable, write `variable {name} = ...`"
                    )),
                );
            }
        }
        visible.push((name.to_string(), span, self.depth));
    }

    fn block(&mut self, function: &'p hir::Function, block: &'p Block, visible: &mut Visible) {
        let mark = visible.len();
        self.depth += 1;
        for stmt in &block.stmts {
            self.stmt(function, stmt, visible);
        }
        self.depth -= 1;
        visible.truncate(mark);
    }

    fn stmt(&mut self, function: &'p hir::Function, stmt: &'p hir::Stmt, visible: &mut Visible) {
        let local = |id: &LocalId| &function.locals[id.0 as usize];
        match &stmt.kind {
            StmtKind::Let { local: id, value } => {
                self.closures(value, visible);
                let info = local(id);
                self.declare(&info.name, info.span, info.implicit, visible);
            }
            StmtKind::Assign { value, .. }
            | StmtKind::CompoundAssign { value, .. }
            | StmtKind::Expr(value)
            | StmtKind::Return(Some(value)) => self.closures(value, visible),
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.closures(condition, visible);
                self.block(function, then_block, visible);
                if let Some(block) = else_block {
                    self.block(function, block, visible);
                }
            }
            StmtKind::While { condition, body } => {
                self.closures(condition, visible);
                self.block(function, body, visible);
            }
            StmtKind::For {
                bindings,
                iterable,
                body,
                ..
            } => {
                self.closures(iterable, visible);
                let mark = visible.len();
                for id in bindings {
                    let info = local(id);
                    self.declare(&info.name, info.span, false, visible);
                }
                self.block(function, body, visible);
                visible.truncate(mark);
            }
            StmtKind::Try {
                body,
                catch,
                finally,
            } => {
                self.block(function, body, visible);
                if let Some((id, block)) = catch {
                    let mark = visible.len();
                    let info = local(id);
                    self.declare(&info.name, info.span, false, visible);
                    self.block(function, block, visible);
                    visible.truncate(mark);
                }
                if let Some(block) = finally {
                    self.block(function, block, visible);
                }
            }
            StmtKind::Block(block) => self.block(function, block, visible),
            StmtKind::Match {
                scrutinee, arms, ..
            } => {
                self.closures(scrutinee, visible);
                for arm in arms {
                    let mark = visible.len();
                    let mut bindings = Vec::new();
                    arm.pattern.bindings(&mut bindings);
                    for id in &bindings {
                        let info = local(id);
                        self.declare(&info.name, info.span, false, visible);
                    }
                    if let Some(guard) = &arm.guard {
                        self.closures(guard, visible);
                    }
                    self.block(function, &arm.body, visible);
                    visible.truncate(mark);
                }
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }

    /// Lints the closures made in an expression, with the names visible there.
    fn closures(&mut self, expr: &'p Expr, visible: &mut Visible) {
        let mut found = Vec::new();
        each_expr(expr, &mut |e| {
            if let ExprKind::Closure { function, .. } = &e.kind {
                found.push(*function);
            }
        });
        for id in found {
            let closure = self.program.function(id);
            self.run(closure, visible);
        }
    }
}

/// Records a read of a local, including the captures of a closure.
fn note_read(expr: &Expr, reads: &mut HashSet<LocalId>) {
    if let ExprKind::Local { local, .. } = &expr.kind {
        reads.insert(*local);
    }
}

/// Calls `f` on an expression and everything inside it, but not on the
/// bodies of closures, which belong to their own functions.
fn each_expr<'e>(expr: &'e Expr, f: &mut dyn FnMut(&'e Expr)) {
    f(expr);
    for child in expr.children() {
        each_expr(child, f);
    }
}

/// `changed` receives the variables whose parts are changed.
fn each_block_expr<'e>(
    block: &'e Block,
    f: &mut dyn FnMut(&'e Expr),
    changed: &mut dyn FnMut(LocalId),
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            StmtKind::Let { value, .. } | StmtKind::Expr(value) | StmtKind::Return(Some(value)) => {
                each_expr(value, f)
            }
            StmtKind::Assign { place, value } | StmtKind::CompoundAssign { place, value, .. } => {
                each_place_expr(place, f, changed);
                each_expr(value, f);
            }
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                each_expr(condition, f);
                each_block_expr(then_block, f, changed);
                if let Some(block) = else_block {
                    each_block_expr(block, f, changed);
                }
            }
            StmtKind::While { condition, body } => {
                each_expr(condition, f);
                each_block_expr(body, f, changed);
            }
            StmtKind::For { iterable, body, .. } => {
                each_expr(iterable, f);
                each_block_expr(body, f, changed);
            }
            StmtKind::Try {
                body,
                catch,
                finally,
            } => {
                each_block_expr(body, f, changed);
                if let Some((_, block)) = catch {
                    each_block_expr(block, f, changed);
                }
                if let Some(block) = finally {
                    each_block_expr(block, f, changed);
                }
            }
            StmtKind::Block(block) => each_block_expr(block, f, changed),
            StmtKind::Match {
                scrutinee, arms, ..
            } => {
                each_expr(scrutinee, f);
                for arm in arms {
                    each_pattern_expr(&arm.pattern, f);
                    if let Some(guard) = &arm.guard {
                        each_expr(guard, f);
                    }
                    each_block_expr(&arm.body, f, changed);
                }
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }
}

/// The expressions inside an assignment target; the target's own local is
/// written, not read, unless an element or field of it is assigned.
fn each_place_expr<'e>(
    place: &'e hir::Place,
    f: &mut dyn FnMut(&'e Expr),
    changed: &mut dyn FnMut(LocalId),
) {
    match place {
        hir::Place::Local(_) => {}
        hir::Place::Index { target, index, .. } => {
            mark_place_root(target, f, changed);
            each_expr(index, f);
        }
        hir::Place::Field { target, .. } | hir::Place::Narrow { target, .. } => {
            mark_place_root(target, f, changed);
        }
    }
}

/// Changing part of a variable counts as using it.
fn mark_place_root<'e>(
    place: &'e hir::Place,
    f: &mut dyn FnMut(&'e Expr),
    changed: &mut dyn FnMut(LocalId),
) {
    match place {
        hir::Place::Local(local) => changed(*local),
        hir::Place::Index { target, index, .. } => {
            mark_place_root(target, f, changed);
            each_expr(index, f);
        }
        hir::Place::Field { target, .. } | hir::Place::Narrow { target, .. } => {
            mark_place_root(target, f, changed)
        }
    }
}

fn each_pattern_expr<'e>(pattern: &'e Pattern, f: &mut dyn FnMut(&'e Expr)) {
    match pattern {
        Pattern::Literal(value) => each_expr(value, f),
        Pattern::Variant { fields, .. } | Pattern::Record { fields, .. } => {
            fields.iter().for_each(|p| each_pattern_expr(p, f))
        }
        Pattern::Member { inner, .. } => each_pattern_expr(inner, f),
        Pattern::Or(alternatives) => alternatives.iter().for_each(|p| each_pattern_expr(p, f)),
        Pattern::Wildcard | Pattern::Binding(_) => {}
    }
}

// ----- suppressions ------------------------------------------------------------

/// Drops warnings that a comment on their line, or the line above, allows:
/// `# allow(unused_variable, naming)`. With `report_unknown`, a name that is
/// not a warning is itself reported.
pub fn suppress(
    diagnostics: Diagnostics,
    sources: &SourceMap,
    files: &[FileId],
    report_unknown: bool,
) -> Diagnostics {
    let known: HashSet<String> = LINTS
        .iter()
        .chain(COMPILER_WARNINGS)
        .map(|code| lint_name(code))
        .collect();
    // For each file, the allowed names on each line.
    let mut allowed: HashMap<FileId, HashMap<usize, Vec<String>>> = HashMap::new();
    let mut unknown = Vec::new();
    for &file in files {
        let source = sources.get(file);
        let mut lines = HashMap::new();
        let mut offset = 0usize;
        for (number, line) in source.text.split_inclusive('\n').enumerate() {
            if let Some(start) = line.find("# allow(")
                && let Some(length) = line[start..].find(')')
            {
                let inside = &line[start + "# allow(".len()..start + length];
                let mut names = Vec::new();
                for name in inside.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    if !known.contains(name) {
                        let at = offset + start + line[start..].find(name).unwrap_or(0);
                        let span = Span::new(file, at, at + name.len());
                        unknown.push((name.to_string(), span));
                    }
                    names.push(name.to_string());
                }
                lines.insert(number, names);
            }
            offset += line.len();
        }
        allowed.insert(file, lines);
    }
    let mut kept = Diagnostics::new();
    for diagnostic in diagnostics.into_vec() {
        let suppressed = diagnostic.severity == Severity::Warning
            && diagnostic.primary_span().is_some_and(|span| {
                let Some(lines) = allowed.get(&span.file) else {
                    return false;
                };
                let (line, _) = sources.get(span.file).line_col(span.start);
                let name = lint_name(diagnostic.code);
                // `line_col` counts lines from 1.
                [line.checked_sub(1), line.checked_sub(2)]
                    .into_iter()
                    .flatten()
                    .any(|l| lines.get(&l).is_some_and(|names| names.contains(&name)))
            });
        if !suppressed {
            kept.push(diagnostic);
        }
    }
    if report_unknown {
        for (name, span) in unknown {
            kept.push(
                Diagnostic::warning("W_UNKNOWN_LINT", format!("no warning is called `{name}`"))
                    .primary(span, "")
                    .help("`sayon explain` lists the warnings; an allowed name is the code without `W_`, in lowercase"),
            );
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_styles() {
        assert!(is_snake("total_count") && is_snake("_unused") && is_snake("x2"));
        assert!(!is_snake("totalCount") && !is_snake("Total"));
        assert!(is_pascal("Point") && is_pascal("HttpServer") && !is_pascal("point_type"));
        assert!(is_upper("MAX_SIZE") && !is_upper("MaxSize"));
        assert_eq!(to_snake("totalCount"), "total_count");
        assert_eq!(to_snake("HTTPServer"), "http_server");
        assert_eq!(to_pascal("point_type"), "PointType");
        assert_eq!(to_upper("maxSize"), "MAX_SIZE");
    }

    #[test]
    fn near_names() {
        assert!(one_edit_apart("total", "totl"));
        assert!(one_edit_apart("total", "totals"));
        assert!(one_edit_apart("total", "totak"));
        assert!(one_edit_apart("total", "toatl"));
        assert!(!one_edit_apart("total", "total"));
        assert!(!one_edit_apart("total", "count"));
    }
}
