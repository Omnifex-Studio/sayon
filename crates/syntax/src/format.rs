//! The source formatter.
//!
//! Single-line statements are printed from the syntax tree with canonical
//! spacing; blocks are indented with four spaces. Literals keep their
//! original spelling. Comments and blank lines between statements are kept:
//! the gaps between statements contain only whitespace and comments, so they
//! are carried over line by line, with runs of blank lines reduced to one.
//! A statement that spans several lines or contains a comment keeps its own
//! layout and is only re-indented.
//!
//! The result is parsed again and must have the same structure as the input;
//! otherwise formatting fails and the file is left as it was.

use crate::ast::*;
use crate::diagnostic::Diagnostic;
use crate::lexer::TAB_COLUMNS;
use crate::source::FileId;
use std::fmt::Write as _;

pub enum FormatError {
    /// The source has syntax errors; it is left unchanged.
    Invalid(Vec<Diagnostic>),
    /// The formatted text would not mean the same thing (a formatter bug).
    Changed,
}

/// Formats a source text.
pub fn format_source(file: FileId, source: &str) -> Result<String, FormatError> {
    let source = source.strip_prefix('\u{FEFF}').unwrap_or(source);
    let normalized = source.replace("\r\n", "\n");
    let lexed = crate::lexer::lex(file, &normalized);
    let mut parsed = crate::parser::parse(file, &lexed.tokens);
    let mut diagnostics = lexed.diagnostics;
    diagnostics.append(&mut parsed.diagnostics);
    if diagnostics.iter().any(Diagnostic::is_error) {
        return Err(FormatError::Invalid(diagnostics));
    }
    let comments: Vec<(u32, u32)> = lexed
        .comments
        .iter()
        .map(|c| (c.span.start, c.span.end))
        .collect();
    let mut printer = Printer {
        source: &normalized,
        out: String::new(),
        comments,
    };
    printer.statements(&parsed.module.stmts, 0, 0, normalized.len());
    let mut out = printer.out.trim_end_matches('\n').to_string();
    while out.starts_with('\n') {
        out.remove(0);
    }
    if !out.is_empty() {
        out.push('\n');
    }

    let reparsed = crate::parser::parse_source(file, &out);
    if reparsed.diagnostics.iter().any(Diagnostic::is_error)
        || fingerprint(&parsed.module, &normalized) != fingerprint(&reparsed.module, &out)
    {
        return Err(FormatError::Changed);
    }
    Ok(out)
}

struct Printer<'s> {
    source: &'s str,
    out: String,
    /// Positions of every comment in the source.
    comments: Vec<(u32, u32)>,
}

/// Where a gap between statements sits.
#[derive(Clone, Copy)]
struct Gap<'a> {
    /// Depth of the statements around the gap.
    depth: usize,
    /// Column of the line after the gap.
    next_column: usize,
    /// For the gap that ends a block: the column of the block's statements.
    closing: Option<usize>,
    /// The statement before the gap, if any.
    previous: Option<&'a Stmt>,
}

/// The block a statement ends with, where trailing comments may belong.
fn last_block(stmt: &Stmt) -> Option<&Block> {
    match &stmt.kind {
        StmtKind::If {
            branches,
            else_body,
        } => else_body
            .as_ref()
            .or_else(|| branches.last().map(|b| &b.body)),
        StmtKind::While { body, .. } | StmtKind::For { body, .. } | StmtKind::Test { body, .. } => {
            Some(body)
        }
        StmtKind::Function(function) => Some(&function.body),
        StmtKind::Try {
            body,
            catch,
            finally,
        } => finally
            .as_ref()
            .or(catch.as_ref().map(|c| &c.body))
            .or(Some(body)),
        StmtKind::Match { arms, .. } => arms.last().map(|arm| &arm.body),
        _ => None,
    }
}

fn indent(depth: usize) -> String {
    " ".repeat(depth * 4)
}

/// Leading whitespace width, counting tabs as four columns.
fn columns(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { TAB_COLUMNS as usize } else { 1 })
        .sum()
}

fn line_start(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

impl<'s> Printer<'s> {
    fn has_comment(&self, start: u32, end: u32) -> bool {
        self.comments
            .iter()
            .any(|(from, _)| *from >= start && *from < end)
    }

    fn text(&self, start: u32, end: u32) -> &'s str {
        &self.source[start as usize..end as usize]
    }

    /// Emits a run of statements, carrying comments and blank lines from the
    /// gaps between them. `from` is where the gap before the first statement
    /// begins and `to` where the gap after the last one ends.
    fn statements(&mut self, stmts: &[Stmt], depth: usize, from: usize, to: usize) {
        let mut position = from;
        let column_of = |stmt: &Stmt| {
            columns(&self.source[line_start(self.source, stmt.span.start as usize)..])
        };
        let own_column = stmts.first().map(column_of).unwrap_or(0);
        let mut previous = None;
        for stmt in stmts {
            let next_column = column_of(stmt);
            let gap = Gap {
                depth,
                next_column,
                closing: None,
                previous,
            };
            self.gap(position, stmt.span.start as usize, gap);
            self.statement(stmt, depth);
            position = stmt.span.end as usize;
            previous = Some(stmt);
        }
        let next_column = self.source[to..]
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(columns)
            .unwrap_or(0);
        let gap = Gap {
            depth,
            next_column,
            closing: Some(own_column),
            previous,
        };
        self.gap(position, to, gap);
    }

    /// The depth of a comment indented deeper than the statements at
    /// `depth`: inside the last blocks of the statement before it, as far
    /// down as its column reaches.
    fn trailing_depth(&self, previous: Option<&Stmt>, depth: usize, column: usize) -> usize {
        let mut depth = depth;
        let mut current = previous;
        while let Some(block) = current.and_then(last_block) {
            let Some(first) = block.stmts.first() else {
                break;
            };
            let block_column =
                columns(&self.source[line_start(self.source, first.span.start as usize)..]);
            if column < block_column {
                break;
            }
            depth += 1;
            current = block.stmts.last();
        }
        depth
    }

    /// Carries the comments and blank lines in `source[from..to]`. The first
    /// partial line's comment trails the previous statement.
    fn gap(&mut self, from: usize, to: usize, gap: Gap) {
        let Gap {
            depth,
            next_column,
            closing,
            previous,
        } = gap;
        let gap = &self.source[from..to];
        let mut lines = gap.split('\n');
        // The gap's first line is the rest of the previous statement's line,
        // except when the gap starts at the beginning of a line.
        let partial = from > 0 && !self.source[..from].ends_with('\n');
        let first_line = if partial { lines.next() } else { None };
        if let Some(first) = first_line {
            let trailing = first.trim();
            if trailing.starts_with('#') {
                while self.out.ends_with(' ') {
                    self.out.pop();
                }
                if self.out.ends_with('\n') {
                    self.out.pop();
                }
                let _ = writeln!(self.out, "  {trailing}");
            }
        }
        let rest: Vec<&str> = lines.collect();
        // The last element is the partial line before the next statement.
        let full = if rest.is_empty() {
            &rest[..]
        } else {
            &rest[..rest.len() - 1]
        };
        let mut blank_pending = false;
        let mut in_block_comment = false;
        let mut block_depth = depth;
        for line in full {
            let trimmed = line.trim();
            if in_block_comment {
                if trimmed == "##" {
                    in_block_comment = false;
                    let _ = writeln!(self.out, "{}##", indent(block_depth));
                } else {
                    let _ = writeln!(self.out, "{}", line.trim_end());
                }
                continue;
            }
            if trimmed.is_empty() {
                blank_pending = true;
                continue;
            }
            if blank_pending && !self.out.is_empty() && !self.out.ends_with("\n\n") {
                self.out.push('\n');
            }
            blank_pending = false;
            // A comment indented deeper than the following code belongs to
            // the block that just ended. At the end of a block, a comment
            // at the block's own column stays in it, and one no deeper than
            // the next line (such as an `else`) belongs outside.
            let column = columns(line);
            let comment_depth = match closing {
                Some(own) if column > own => self.trailing_depth(previous, depth, column),
                Some(_) if column > next_column || depth == 0 => depth,
                Some(_) => depth - 1,
                None if column > next_column => self.trailing_depth(previous, depth, column),
                None => depth,
            };
            if trimmed == "##" {
                in_block_comment = true;
                block_depth = comment_depth;
            }
            let _ = writeln!(self.out, "{}{}", indent(comment_depth), trimmed);
        }
        if blank_pending && closing.is_none() && !self.out.is_empty() && !self.out.ends_with("\n\n")
        {
            self.out.push('\n');
        }
    }

    /// Copies a statement's source, re-indented so its first line sits at
    /// `depth` and later lines keep their relative indentation.
    fn verbatim(&mut self, stmt: &Stmt, depth: usize) {
        let start = line_start(self.source, stmt.span.start as usize);
        let base = columns(&self.source[start..]);
        let text = self.text(stmt.span.start, stmt.span.end);
        for (index, line) in text.split('\n').enumerate() {
            let content = line.trim();
            if content.is_empty() {
                self.out.push('\n');
                continue;
            }
            let column = if index == 0 {
                depth * 4
            } else {
                (depth * 4 + columns(line)).saturating_sub(base)
            };
            let _ = writeln!(self.out, "{}{}", " ".repeat(column), content.trim_end());
        }
    }

    fn statement(&mut self, stmt: &Stmt, depth: usize) {
        let text = self.text(stmt.span.start, stmt.span.end);
        let simple = matches!(
            stmt.kind,
            StmtKind::Expr(_)
                | StmtKind::Assign { .. }
                | StmtKind::CompoundAssign { .. }
                | StmtKind::Declare(_)
                | StmtKind::Return(_)
        );
        let keeps_layout = matches!(
            stmt.kind,
            StmtKind::Unavailable { .. } | StmtKind::TypeDecl { .. }
        ) || (simple
            && (text.contains('\n') || self.has_comment(stmt.span.start, stmt.span.end)))
            || contains_function_literal(stmt);
        if keeps_layout {
            self.verbatim(stmt, depth);
            return;
        }
        let pad = indent(depth);
        match &stmt.kind {
            StmtKind::Expr(expr) => {
                let line = self.expr(expr, 0);
                let _ = writeln!(self.out, "{pad}{line}");
            }
            StmtKind::Assign { target, value } => {
                let line = format!("{} = {}", self.expr(target, 0), self.expr(value, 0));
                let _ = writeln!(self.out, "{pad}{line}");
            }
            StmtKind::CompoundAssign {
                target, op, value, ..
            } => {
                let line = format!(
                    "{} {}= {}",
                    self.expr(target, 0),
                    op.symbol(),
                    self.expr(value, 0)
                );
                let _ = writeln!(self.out, "{pad}{line}");
            }
            StmtKind::Declare(decl) => {
                let mut line = visibility(decl.visibility).to_string();
                match decl.kind {
                    DeclKind::Variable => line.push_str("variable "),
                    DeclKind::Constant => line.push_str("constant "),
                    DeclKind::Annotated => {}
                }
                line.push_str(&decl.name.name);
                if let Some(ty) = &decl.ty {
                    let _ = write!(line, ": {}", type_text(ty));
                }
                let _ = write!(line, " = {}", self.expr(&decl.value, 0));
                let _ = writeln!(self.out, "{pad}{line}");
            }
            StmtKind::Return(value) => match value {
                Some(value) => {
                    let line = self.expr(value, 0);
                    let _ = writeln!(self.out, "{pad}return {line}");
                }
                None => {
                    let _ = writeln!(self.out, "{pad}return");
                }
            },
            StmtKind::Break => {
                let _ = writeln!(self.out, "{pad}break");
            }
            StmtKind::Continue => {
                let _ = writeln!(self.out, "{pad}continue");
            }
            StmtKind::If {
                branches,
                else_body,
            } => {
                for (index, branch) in branches.iter().enumerate() {
                    let keyword = if index == 0 { "if" } else { "elseif" };
                    let condition = self.expr(&branch.condition, 0);
                    let _ = writeln!(self.out, "{pad}{keyword} {condition}");
                    let next = match (branches.get(index + 1), else_body) {
                        (Some(next), _) => self.keyword_line_start(next.condition.span.start),
                        (None, Some(block)) => self.keyword_line(block.span.start, "else").0,
                        (None, None) => stmt.span.end as usize,
                    };
                    self.block(
                        &branch.body,
                        branch.condition.span.end as usize,
                        next,
                        depth,
                    );
                }
                if let Some(block) = else_body {
                    let _ = writeln!(self.out, "{pad}else");
                    let header_end = self.keyword_line(block.span.start, "else").1;
                    self.block(block, header_end, stmt.span.end as usize, depth);
                }
            }
            StmtKind::While { condition, body } => {
                let condition_text = self.expr(condition, 0);
                let _ = writeln!(self.out, "{pad}while {condition_text}");
                self.block(
                    body,
                    condition.span.end as usize,
                    stmt.span.end as usize,
                    depth,
                );
            }
            StmtKind::For {
                bindings,
                iterable,
                body,
            } => {
                let names: Vec<&str> = bindings.iter().map(|b| b.name.as_str()).collect();
                let iterable_text = self.expr(iterable, 0);
                let _ = writeln!(self.out, "{pad}for {} in {iterable_text}", names.join(", "));
                self.block(
                    body,
                    iterable.span.end as usize,
                    stmt.span.end as usize,
                    depth,
                );
            }
            StmtKind::Function(function) => {
                let header = self.function_header(function);
                let _ = writeln!(self.out, "{pad}{header}");
                self.block(
                    &function.body,
                    function.header_span.end as usize,
                    stmt.span.end as usize,
                    depth,
                );
            }
            StmtKind::Try {
                body,
                catch,
                finally,
            } => {
                let _ = writeln!(self.out, "{pad}try");
                let try_end = stmt.span.start as usize + 3;
                let body_next = match (catch, finally) {
                    (Some(clause), _) => self.keyword_line_start(clause.binding.span.start),
                    (None, Some(block)) => self.keyword_line(block.span.start, "finally").0,
                    (None, None) => stmt.span.end as usize,
                };
                self.block(body, try_end, body_next, depth);
                if let Some(clause) = catch {
                    let _ = writeln!(self.out, "{pad}catch {}", clause.binding.name);
                    let next = match finally {
                        Some(block) => self.keyword_line(block.span.start, "finally").0,
                        None => stmt.span.end as usize,
                    };
                    self.block(&clause.body, clause.binding.span.end as usize, next, depth);
                }
                if let Some(block) = finally {
                    let _ = writeln!(self.out, "{pad}finally");
                    let header_end = self.keyword_line(block.span.start, "finally").1;
                    self.block(block, header_end, stmt.span.end as usize, depth);
                }
            }
            StmtKind::EnumDecl {
                visibility: vis,
                name,
                generics,
                variants,
            } => {
                let _ = writeln!(
                    self.out,
                    "{pad}{}enum {}{}",
                    visibility(*vis),
                    name.name,
                    generic_list(generics)
                );
                for variant in variants {
                    let fields = if variant.fields.is_empty() {
                        String::new()
                    } else {
                        let list: Vec<String> = variant
                            .fields
                            .iter()
                            .map(|f| format!("{}: {}", f.name.name, type_text(&f.ty)))
                            .collect();
                        format!("({})", list.join(", "))
                    };
                    let _ = writeln!(
                        self.out,
                        "{}{}{fields}",
                        indent(depth + 1),
                        variant.name.name
                    );
                }
            }
            StmtKind::Match { scrutinee, arms } => {
                let scrutinee_text = self.expr(scrutinee, 0);
                let _ = writeln!(self.out, "{pad}match {scrutinee_text}");
                for (index, arm) in arms.iter().enumerate() {
                    let guard = match &arm.guard {
                        Some(guard) => format!(" if {}", self.expr(guard, 0)),
                        None => String::new(),
                    };
                    let pattern = self.pattern(&arm.pattern);
                    let _ = writeln!(self.out, "{}case {pattern}{guard}", indent(depth + 1));
                    let header_end =
                        arm.guard
                            .as_ref()
                            .map(|g| g.span.end)
                            .unwrap_or(arm.pattern.span.end) as usize;
                    let next = match arms.get(index + 1) {
                        Some(next) => self.keyword_line_start(next.pattern.span.start),
                        None => stmt.span.end as usize,
                    };
                    self.block(&arm.body, header_end, next, depth + 1);
                }
            }
            StmtKind::Test {
                name_span, body, ..
            } => {
                let name = self.text(name_span.start, name_span.end);
                let _ = writeln!(self.out, "{pad}test {name}");
                self.block(body, name_span.end as usize, stmt.span.end as usize, depth);
            }
            StmtKind::TypeDecl { .. } | StmtKind::Unavailable { .. } => {
                unreachable!("kept verbatim")
            }
        }
    }

    fn block(&mut self, block: &Block, header_end: usize, next: usize, depth: usize) {
        self.statements(&block.stmts, depth + 1, header_end, next.max(header_end));
    }

    /// The start of the line holding the clause keyword before `offset`.
    fn keyword_line_start(&self, offset: u32) -> usize {
        line_start(self.source, offset as usize)
    }

    /// The nearest line above `before` that starts with `keyword`: its start
    /// and the end of the keyword. Only blank and comment lines can separate
    /// a clause keyword from its block.
    fn keyword_line(&self, before: u32, keyword: &str) -> (usize, usize) {
        let mut end = line_start(self.source, before as usize);
        while end > 0 {
            let start = line_start(self.source, end - 1);
            let line = &self.source[start..end - 1];
            let content = line.trim_start();
            if content.starts_with(keyword) {
                let column = line.len() - content.len();
                return (start, start + column + keyword.len());
            }
            end = start;
        }
        (0, 0)
    }

    fn function_header(&self, function: &Function) -> String {
        let mut header = visibility(function.visibility).to_string();
        header.push_str("function");
        if let Some(name) = &function.name {
            let _ = write!(header, " {}", name.name);
        }
        header.push_str(&generic_list(&function.generics));
        let params: Vec<String> = function
            .params
            .iter()
            .map(|param| {
                let mut text = match param.mode {
                    Some((ParamMode::Own, _)) => "own ".to_string(),
                    Some((ParamMode::Update, _)) => "update ".to_string(),
                    _ => String::new(),
                };
                text.push_str(&param.name.name);
                if let Some(ty) = &param.ty {
                    let _ = write!(text, ": {}", type_text(ty));
                }
                if let Some(default) = &param.default {
                    let _ = write!(text, " = {}", self.expr(default, 0));
                }
                text
            })
            .collect();
        let _ = write!(header, "({})", params.join(", "));
        if let Some(ret) = &function.ret {
            let _ = write!(header, " -> {}", type_text(ret));
        }
        header
    }

    fn pattern(&self, pattern: &Pattern) -> String {
        match &pattern.kind {
            PatternKind::Wildcard => "_".to_string(),
            PatternKind::Literal(expr) => self.expr(expr, 0),
            PatternKind::Name(name) => name.name.clone(),
            PatternKind::Variant {
                qualifier,
                name,
                args,
            } => {
                let path = qualified(qualifier, name);
                if args.is_empty() && !qualifier.is_empty() {
                    return path;
                }
                let args: Vec<String> = args.iter().map(|a| self.pattern(a)).collect();
                format!("{path}({})", args.join(", "))
            }
            PatternKind::Fields {
                qualifier,
                name,
                fields,
            } => {
                let fields: Vec<String> = fields
                    .iter()
                    .map(|(field, pattern)| format!("{} = {}", field.name, self.pattern(pattern)))
                    .collect();
                format!("{}({})", qualified(qualifier, name), fields.join(", "))
            }
            PatternKind::Or(alternatives) => {
                let alternatives: Vec<String> =
                    alternatives.iter().map(|a| self.pattern(a)).collect();
                alternatives.join(" or ")
            }
        }
    }

    /// Prints an expression, parenthesized when its precedence is below `min`.
    fn expr(&self, expr: &Expr, min: u8) -> String {
        let (text, precedence) = self.expr_inner(expr);
        if precedence < min {
            format!("({text})")
        } else {
            text
        }
    }

    fn expr_inner(&self, expr: &Expr) -> (String, u8) {
        const PRIMARY: u8 = 9;
        const POSTFIX: u8 = 8;
        const UNARY: u8 = 7;
        let list = |items: &[Expr]| -> String {
            let parts: Vec<String> = items.iter().map(|item| self.expr(item, 0)).collect();
            parts.join(", ")
        };
        match &expr.kind {
            ExprKind::Integer { .. } | ExprKind::Float(_) | ExprKind::String(_) => (
                self.text(expr.span.start, expr.span.end).to_string(),
                PRIMARY,
            ),
            ExprKind::Boolean(value) => (value.to_string(), PRIMARY),
            ExprKind::Nothing => ("nothing".to_string(), PRIMARY),
            ExprKind::Name(name) => (name.name.clone(), PRIMARY),
            ExprKind::Array(items) => (format!("[{}]", list(items)), PRIMARY),
            ExprKind::Dict(entries) => {
                let parts: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{} = {}", self.expr(k, 0), self.expr(v, 0)))
                    .collect();
                (format!("[{}]", parts.join(", ")), PRIMARY)
            }
            ExprKind::Record(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{} = {}", k.name, self.expr(v, 0)))
                    .collect();
                (format!("[{}]", parts.join(", ")), PRIMARY)
            }
            ExprKind::Call {
                callee,
                generic_args,
                args,
            } => {
                let generics = if generic_args.is_empty() {
                    String::new()
                } else {
                    let types: Vec<String> = generic_args.iter().map(type_text).collect();
                    format!("<{}>", types.join(", "))
                };
                (
                    format!("{}{generics}({})", self.expr(callee, POSTFIX), list(args)),
                    POSTFIX,
                )
            }
            ExprKind::Construct {
                callee,
                generic_args,
                fields,
            } => {
                let generics = if generic_args.is_empty() {
                    String::new()
                } else {
                    let types: Vec<String> = generic_args.iter().map(type_text).collect();
                    format!("<{}>", types.join(", "))
                };
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(name, value)| format!("{} = {}", name.name, self.expr(value, 0)))
                    .collect();
                (
                    format!(
                        "{}{generics}({})",
                        self.expr(callee, POSTFIX),
                        parts.join(", ")
                    ),
                    POSTFIX,
                )
            }
            ExprKind::Index { target, index } => (
                format!("{}[{}]", self.expr(target, POSTFIX), self.expr(index, 0)),
                POSTFIX,
            ),
            ExprKind::Member { target, name } => (
                format!("{}.{}", self.expr(target, POSTFIX), name.name),
                POSTFIX,
            ),
            ExprKind::Unary { op, operand } => {
                let operand_text = self.expr(operand, UNARY);
                match op {
                    UnaryOp::Not => (format!("not {operand_text}"), UNARY),
                    UnaryOp::Negate => {
                        let nested = matches!(
                            operand.kind,
                            ExprKind::Unary {
                                op: UnaryOp::Negate,
                                ..
                            }
                        ) || operand_text.starts_with('-');
                        if nested {
                            (format!("-({operand_text})"), UNARY)
                        } else {
                            (format!("-{operand_text}"), UNARY)
                        }
                    }
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let precedence = binary_precedence(*op);
                let comparison = op.is_comparison();
                // Operands of `==` and `!=` that are themselves comparisons
                // keep parentheses, for readability.
                let minimum = match op {
                    BinaryOp::Equal | BinaryOp::NotEqual => precedence + 2,
                    _ if comparison => precedence + 1,
                    _ => precedence,
                };
                let left = self.expr(lhs, minimum);
                let right = self.expr(rhs, minimum.max(precedence + 1));
                (format!("{left} {} {right}", op.symbol()), precedence)
            }
            ExprKind::IsError(inner) => (format!("{} is error", self.expr(inner, 4)), 3),
            ExprKind::TryExpr(inner) => (format!("try {}", self.expr(inner, UNARY)), UNARY),
            ExprKind::Import { path_span, .. } => (
                format!("import({})", self.text(path_span.start, path_span.end)),
                POSTFIX,
            ),
            ExprKind::Function(_) | ExprKind::Error => (
                self.text(expr.span.start, expr.span.end).to_string(),
                PRIMARY,
            ),
        }
    }
}

fn binary_precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => 1,
        BinaryOp::And => 2,
        BinaryOp::Equal | BinaryOp::NotEqual => 3,
        BinaryOp::Less | BinaryOp::Greater | BinaryOp::LessEqual | BinaryOp::GreaterEqual => 4,
        BinaryOp::Add | BinaryOp::Subtract => 5,
        BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Remainder => 6,
    }
}

fn visibility(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Default => "",
        Visibility::Public => "public ",
        Visibility::Private => "private ",
    }
}

fn generic_list(generics: &[Ident]) -> String {
    if generics.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = generics.iter().map(|g| g.name.as_str()).collect();
        format!("<{}>", names.join(", "))
    }
}

pub fn type_text(ty: &TypeExpr) -> String {
    match &ty.kind {
        TypeExprKind::Named { module, name, args } => {
            let qualified = match module {
                Some(module) => format!("{}.{}", module.name, name.name),
                None => name.name.clone(),
            };
            if args.is_empty() {
                qualified
            } else {
                let args: Vec<String> = args.iter().map(type_text).collect();
                format!("{qualified}<{}>", args.join(", "))
            }
        }
        TypeExprKind::Union(members) => {
            let members: Vec<String> = members
                .iter()
                .map(|member| match member.kind {
                    TypeExprKind::Function { .. } => format!("({})", type_text(member)),
                    _ => type_text(member),
                })
                .collect();
            members.join(" or ")
        }
        TypeExprKind::Function { params, ret } => {
            let params: Vec<String> = params
                .iter()
                .map(|(mode, ty)| {
                    let mode = match mode {
                        Some(ParamMode::Own) => "own ",
                        Some(ParamMode::Update) => "update ",
                        _ => "",
                    };
                    format!("{mode}{}", type_text(ty))
                })
                .collect();
            match ret {
                Some(ret) => format!("function({}) -> {}", params.join(", "), type_text(ret)),
                None => format!("function({})", params.join(", ")),
            }
        }
        TypeExprKind::Error => "?".to_string(),
    }
}

fn contains_function_literal(stmt: &Stmt) -> bool {
    fn expr_has(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Function(_) => true,
            ExprKind::Array(items) => items.iter().any(expr_has),
            ExprKind::Dict(entries) => entries.iter().any(|(k, v)| expr_has(k) || expr_has(v)),
            ExprKind::Record(fields) => fields.iter().any(|(_, v)| expr_has(v)),
            ExprKind::Call { callee, args, .. } => expr_has(callee) || args.iter().any(expr_has),
            ExprKind::Construct { callee, fields, .. } => {
                expr_has(callee) || fields.iter().any(|(_, v)| expr_has(v))
            }
            ExprKind::Index { target, index } => expr_has(target) || expr_has(index),
            ExprKind::Member { target, .. } => expr_has(target),
            ExprKind::Unary { operand, .. } => expr_has(operand),
            ExprKind::Binary { lhs, rhs, .. } => expr_has(lhs) || expr_has(rhs),
            ExprKind::IsError(inner) | ExprKind::TryExpr(inner) => expr_has(inner),
            _ => false,
        }
    }
    match &stmt.kind {
        StmtKind::Expr(expr) | StmtKind::Return(Some(expr)) => expr_has(expr),
        StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
            expr_has(target) || expr_has(value)
        }
        StmtKind::Declare(decl) => expr_has(&decl.value),
        _ => false,
    }
}

// ----- structural comparison ------------------------------------------------------

/// A description of the program's structure without positions or layout,
/// used to confirm that formatting did not change meaning.
pub fn fingerprint(module: &Module, source: &str) -> String {
    let mut out = String::new();
    for stmt in &module.stmts {
        stmt_print(stmt, source, &mut out);
    }
    out
}

fn stmt_print(stmt: &Stmt, source: &str, out: &mut String) {
    let e = |expr: &Expr, out: &mut String| expr_print(expr, source, out);
    match &stmt.kind {
        StmtKind::Expr(expr) => {
            out.push_str("expr ");
            e(expr, out);
        }
        StmtKind::Assign { target, value } => {
            out.push_str("assign ");
            e(target, out);
            out.push(' ');
            e(value, out);
        }
        StmtKind::CompoundAssign {
            target, op, value, ..
        } => {
            let _ = write!(out, "compound {} ", op.symbol());
            e(target, out);
            out.push(' ');
            e(value, out);
        }
        StmtKind::Declare(decl) => {
            let _ = write!(
                out,
                "declare {:?} {:?} {} ",
                decl.kind, decl.visibility, decl.name.name
            );
            if let Some(ty) = &decl.ty {
                out.push_str(&type_text(ty));
            }
            e(&decl.value, out);
        }
        StmtKind::If {
            branches,
            else_body,
        } => {
            for branch in branches {
                out.push_str("if ");
                e(&branch.condition, out);
                block_print(&branch.body, source, out);
            }
            if let Some(block) = else_body {
                out.push_str("else");
                block_print(block, source, out);
            }
        }
        StmtKind::While { condition, body } => {
            out.push_str("while ");
            e(condition, out);
            block_print(body, source, out);
        }
        StmtKind::For {
            bindings,
            iterable,
            body,
        } => {
            let names: Vec<&str> = bindings.iter().map(|b| b.name.as_str()).collect();
            let _ = write!(out, "for {} ", names.join(","));
            e(iterable, out);
            block_print(body, source, out);
        }
        StmtKind::Break => out.push_str("break"),
        StmtKind::Continue => out.push_str("continue"),
        StmtKind::Return(value) => {
            out.push_str("return ");
            if let Some(value) = value {
                e(value, out);
            }
        }
        StmtKind::Function(function) => function_print(function, source, out),
        StmtKind::Try {
            body,
            catch,
            finally,
        } => {
            out.push_str("try");
            block_print(body, source, out);
            if let Some(clause) = catch {
                let _ = write!(out, "catch {}", clause.binding.name);
                block_print(&clause.body, source, out);
            }
            if let Some(block) = finally {
                out.push_str("finally");
                block_print(block, source, out);
            }
        }
        StmtKind::TypeDecl {
            visibility,
            name,
            generics,
            fields,
        } => {
            let _ = write!(
                out,
                "type {visibility:?} {} {} ",
                name.name,
                generic_list(generics)
            );
            for field in fields {
                let _ = write!(out, "{}:{} ", field.name.name, type_text(&field.ty));
            }
        }
        StmtKind::EnumDecl {
            visibility,
            name,
            generics,
            variants,
        } => {
            let _ = write!(
                out,
                "enum {visibility:?} {} {} ",
                name.name,
                generic_list(generics)
            );
            for variant in variants {
                let _ = write!(out, "{}(", variant.name.name);
                for field in &variant.fields {
                    let _ = write!(out, "{}:{} ", field.name.name, type_text(&field.ty));
                }
                out.push(')');
            }
        }
        StmtKind::Match { scrutinee, arms } => {
            out.push_str("match ");
            e(scrutinee, out);
            for arm in arms {
                let _ = write!(out, "case {:?} ", pattern_shape(&arm.pattern, source));
                if let Some(guard) = &arm.guard {
                    e(guard, out);
                }
                block_print(&arm.body, source, out);
            }
        }
        StmtKind::Test { name, body, .. } => {
            let _ = write!(out, "test {name:?}");
            block_print(body, source, out);
        }
        StmtKind::Unavailable { feature, .. } => {
            let _ = write!(out, "unavailable {feature}");
        }
    }
    out.push(';');
}

/// `a.b.name` for a qualified name in a pattern.
fn qualified(qualifier: &[Ident], name: &Ident) -> String {
    qualifier
        .iter()
        .chain(std::iter::once(name))
        .map(|part| part.name.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

fn pattern_shape(pattern: &Pattern, source: &str) -> String {
    match &pattern.kind {
        PatternKind::Wildcard => "_".into(),
        PatternKind::Literal(expr) => {
            let mut out = String::new();
            expr_print(expr, source, &mut out);
            out
        }
        PatternKind::Name(name) => name.name.clone(),
        PatternKind::Variant {
            qualifier,
            name,
            args,
        } => {
            let args: Vec<String> = args.iter().map(|a| pattern_shape(a, source)).collect();
            format!("{}({})", qualified(qualifier, name), args.join(","))
        }
        PatternKind::Fields {
            qualifier,
            name,
            fields,
        } => {
            let fields: Vec<String> = fields
                .iter()
                .map(|(field, pattern)| {
                    format!("{}={}", field.name, pattern_shape(pattern, source))
                })
                .collect();
            format!("{}({})", qualified(qualifier, name), fields.join(","))
        }
        PatternKind::Or(alternatives) => {
            let alternatives: Vec<String> = alternatives
                .iter()
                .map(|a| pattern_shape(a, source))
                .collect();
            format!("({})", alternatives.join("|"))
        }
    }
}

fn block_print(block: &Block, source: &str, out: &mut String) {
    out.push('{');
    for stmt in &block.stmts {
        stmt_print(stmt, source, out);
    }
    out.push('}');
}

fn function_print(function: &Function, source: &str, out: &mut String) {
    let name = function
        .name
        .as_ref()
        .map(|n| n.name.as_str())
        .unwrap_or("");
    let _ = write!(
        out,
        "function {:?} {name} {} (",
        function.visibility,
        generic_list(&function.generics)
    );
    for param in &function.params {
        let _ = write!(out, "{:?} {} ", param.mode.map(|m| m.0), param.name.name);
        if let Some(ty) = &param.ty {
            out.push_str(&type_text(ty));
        }
        if let Some(default) = &param.default {
            out.push('=');
            expr_print(default, source, out);
        }
        out.push(',');
    }
    out.push(')');
    if let Some(ret) = &function.ret {
        out.push_str(&type_text(ret));
    }
    block_print(&function.body, source, out);
}

fn expr_print(expr: &Expr, source: &str, out: &mut String) {
    let e = |expr: &Expr, out: &mut String| expr_print(expr, source, out);
    match &expr.kind {
        ExprKind::Integer { value, unsigned } => {
            let _ = write!(out, "int({value},{unsigned})");
        }
        ExprKind::Float(value) => {
            let _ = write!(out, "float({})", value.to_bits());
        }
        ExprKind::String(segments) => {
            out.push_str("str(");
            for segment in segments {
                match segment {
                    StringSegment::Text(text) => {
                        let _ = write!(out, "{text:?}");
                    }
                    StringSegment::Interpolation(inner) => {
                        out.push('{');
                        e(inner, out);
                        out.push('}');
                    }
                }
            }
            out.push(')');
        }
        ExprKind::Boolean(value) => {
            let _ = write!(out, "{value}");
        }
        ExprKind::Nothing => out.push_str("nothing"),
        ExprKind::Name(name) => out.push_str(&name.name),
        ExprKind::Array(items) => {
            out.push('[');
            for item in items {
                e(item, out);
                out.push(',');
            }
            out.push(']');
        }
        ExprKind::Dict(entries) => {
            out.push_str("dict[");
            for (key, value) in entries {
                e(key, out);
                out.push('=');
                e(value, out);
                out.push(',');
            }
            out.push(']');
        }
        ExprKind::Record(fields) => {
            out.push_str("record[");
            for (name, value) in fields {
                let _ = write!(out, "{}=", name.name);
                e(value, out);
                out.push(',');
            }
            out.push(']');
        }
        ExprKind::Call {
            callee,
            generic_args,
            args,
        } => {
            out.push_str("call(");
            e(callee, out);
            for ty in generic_args {
                let _ = write!(out, "<{}>", type_text(ty));
            }
            for arg in args {
                out.push(',');
                e(arg, out);
            }
            out.push(')');
        }
        ExprKind::Construct {
            callee,
            generic_args,
            fields,
        } => {
            out.push_str("construct(");
            e(callee, out);
            for ty in generic_args {
                let _ = write!(out, "<{}>", type_text(ty));
            }
            for (name, value) in fields {
                let _ = write!(out, ",{}=", name.name);
                e(value, out);
            }
            out.push(')');
        }
        ExprKind::Index { target, index } => {
            out.push_str("index(");
            e(target, out);
            out.push(',');
            e(index, out);
            out.push(')');
        }
        ExprKind::Member { target, name } => {
            out.push_str("member(");
            e(target, out);
            let _ = write!(out, ",{})", name.name);
        }
        ExprKind::Unary { op, operand } => {
            let _ = write!(out, "unary({op:?},");
            e(operand, out);
            out.push(')');
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let _ = write!(out, "binary({},", op.symbol());
            e(lhs, out);
            out.push(',');
            e(rhs, out);
            out.push(')');
        }
        ExprKind::IsError(inner) => {
            out.push_str("is_error(");
            e(inner, out);
            out.push(')');
        }
        ExprKind::TryExpr(inner) => {
            out.push_str("try(");
            e(inner, out);
            out.push(')');
        }
        ExprKind::Function(function) => function_print(function, source, out),
        ExprKind::Import { path, .. } => {
            let _ = write!(out, "import({path:?})");
        }
        ExprKind::Error => out.push_str("error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(source: &str) -> String {
        match format_source(FileId(0), source) {
            Ok(text) => text,
            Err(FormatError::Invalid(diagnostics)) => panic!("invalid: {diagnostics:?}"),
            Err(FormatError::Changed) => panic!("formatting changed the program"),
        }
    }

    #[track_caller]
    fn check(input: &str, expected: &str) {
        let once = format(input);
        assert_eq!(once, expected);
        assert_eq!(format(&once), once, "formatting is not idempotent");
    }

    #[test]
    fn spacing_and_indentation() {
        check(
            "x=1+2*3\nif x>3 and not false\n\tprint( x ,\"big\" )\nelse\n\t\tprint(x)\n",
            "x = 1 + 2 * 3\nif x > 3 and not false\n    print(x, \"big\")\nelse\n    print(x)\n",
        );
    }

    #[test]
    fn parentheses_follow_precedence() {
        check(
            "y = (2 + 3) * 4\nz = 2 + (3 * 4)\nw = a - (b - c)\nv = -(-1)\n",
            "y = (2 + 3) * 4\nz = 2 + 3 * 4\nw = a - (b - c)\nv = -(-1)\n",
        );
        check(
            "ok = not (a and b)\nk = (a < b) == (c < d)\n",
            "ok = not (a and b)\nk = (a < b) == (c < d)\n",
        );
    }

    #[test]
    fn literals_keep_their_spelling() {
        check(
            "mask = 0xFF\nbig = 1_000_000\ns = \"caf\\u{e9} {name}\"\n",
            "mask = 0xFF\nbig = 1_000_000\ns = \"caf\\u{e9} {name}\"\n",
        );
    }

    #[test]
    fn comments_and_blank_lines_are_kept() {
        check(
            "# heading\n\n\n\nx = 1  # trailing\n# about y\ny = 2\n\n##\nblock\n  comment\n##\nfunction f()\n    # inside\n    return 1\n    # after the return\n\nprint(f())\n",
            "# heading\n\nx = 1  # trailing\n# about y\ny = 2\n\n##\nblock\n  comment\n##\nfunction f()\n    # inside\n    return 1\n    # after the return\n\nprint(f())\n",
        );
    }

    #[test]
    fn functions_and_control_flow() {
        check(
            "function  add( a,b = 2 )->integer\n  return a+b\nfor  i  in range(3)\n  try\n    print(i)\n  catch problem\n    print(problem)\n  finally\n    print(\"done\")\n",
            "function add(a, b = 2) -> integer\n    return a + b\nfor i in range(3)\n    try\n        print(i)\n    catch problem\n        print(problem)\n    finally\n        print(\"done\")\n",
        );
    }

    #[test]
    fn multi_line_statements_keep_their_layout() {
        check(
            "values = [\n    1,\n    2,  # two\n]\n",
            "values = [\n    1,\n    2,  # two\n]\n",
        );
        check(
            "if true\n\tnames = [\n\t\t\"a\",\n\t]\n",
            "if true\n    names = [\n        \"a\",\n    ]\n",
        );
    }

    #[test]
    fn tests_and_types() {
        check(
            "test   \"adds\"\n  assert(1+1==2)\n",
            "test \"adds\"\n    assert(1 + 1 == 2)\n",
        );
    }

    #[test]
    fn elseif_chains() {
        check(
            "if a\n  x()\nelseif b\n  y()\n# before else\nelse\n  z()\n",
            "if a\n    x()\nelseif b\n    y()\n# before else\nelse\n    z()\n",
        );
    }

    #[test]
    fn comments_at_the_end_of_a_branch() {
        check(
            "if a\n  x()\n  # end of the branch\n# about else\nelse\n  if b\n    y()\n    # nested\n",
            "if a\n    x()\n    # end of the branch\n# about else\nelse\n    if b\n        y()\n        # nested\n",
        );
    }

    #[test]
    fn structures_enums_and_match() {
        check(
            "type Point = [x: float, y: float]\nenum Shape\n  circle( radius:float )\n  point\np=Point( x=1.0,y = 2.0 )\nq:geometry.Point=[x=0.0,y=p.y]\nmatch shape\n  case circle(r)  if r>1.0\n    print(r)\n  case _\n    print( \"other\" )\n",
            "type Point = [x: float, y: float]\nenum Shape\n    circle(radius: float)\n    point\np = Point(x = 1.0, y = 2.0)\nq: geometry.Point = [x = 0.0, y = p.y]\nmatch shape\n    case circle(r) if r > 1.0\n        print(r)\n    case _\n        print(\"other\")\n",
        );
    }

    #[test]
    fn invalid_source_is_rejected() {
        assert!(matches!(
            format_source(FileId(0), "x = (1 +\n"),
            Err(FormatError::Invalid(_))
        ));
    }

    #[test]
    fn crlf_and_bom_are_normalized() {
        check("\u{FEFF}x = 1\r\nprint(x)\r\n", "x = 1\nprint(x)\n");
    }
}
