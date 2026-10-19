//! Compile-time names of the target: `platform`, `processor`, and
//! `supports("capability")`.
//!
//! Each is the compiler's own only where nothing the program declares hides
//! it. An `if` condition made of nothing but these, compared with written
//! names and joined by `and`, `or`, and `not`, is decided while checking:
//! only the branch it selects is checked and compiled, so a branch can call
//! what only its platform has. Any other condition, including one that
//! merely holds the text "windows", is an ordinary condition.

use crate::checker::{Checker, FnCtx};
use sayon_syntax::Diagnostic;
use sayon_syntax::ast::{self, BinaryOp, ExprKind, StringSegment, UnaryOp};
use sayon_target::{Capability, PLATFORMS, PROCESSORS};

/// The text of a string literal without interpolation.
fn literal(expr: &ast::Expr) -> Option<&str> {
    let ExprKind::String(segments) = &expr.kind else {
        return None;
    };
    match segments.as_slice() {
        [] => Some(""),
        [StringSegment::Text(text)] => Some(text),
        _ => None,
    }
}

impl<'a> Checker<'a> {
    /// Whether `name` means the compiler's own name in this code.
    fn compiler_name(&self, cx: &FnCtx, name: &str) -> bool {
        !self.is_local_name(cx, name)
            && self.item(cx.module, name).is_none()
            && self.import(cx.module, name).is_none()
    }

    /// The value of `platform` or `processor` for the target, where the name
    /// is the compiler's.
    pub fn compile_time_name(&self, cx: &FnCtx, name: &str) -> Option<&'static str> {
        let value = match name {
            "platform" => self.target.platform,
            "processor" => self.target.processor,
            _ => return None,
        };
        self.compiler_name(cx, name).then_some(value)
    }

    /// Whether `expr` is a condition the compiler decides.
    fn decided(&self, cx: &FnCtx, expr: &ast::Expr) -> bool {
        match &expr.kind {
            ExprKind::Binary {
                op: BinaryOp::And | BinaryOp::Or,
                lhs,
                rhs,
            } => self.decided(cx, lhs) && self.decided(cx, rhs),
            ExprKind::Unary {
                op: UnaryOp::Not,
                operand,
            } => self.decided(cx, operand),
            ExprKind::Binary {
                op: BinaryOp::Equal | BinaryOp::NotEqual,
                lhs,
                rhs,
            } => {
                let named = |side: &ast::Expr| {
                    matches!(&side.kind, ExprKind::Name(name)
                        if self.compile_time_name(cx, &name.name).is_some())
                };
                (named(lhs) && literal(rhs).is_some()) || (named(rhs) && literal(lhs).is_some())
            }
            ExprKind::Call {
                callee,
                generic_args,
                args,
            } => {
                matches!(&callee.kind, ExprKind::Name(name)
                    if name.name == "supports" && self.compiler_name(cx, "supports"))
                    && generic_args.is_empty()
                    && args.len() == 1
                    && literal(&args[0]).is_some()
            }
            _ => false,
        }
    }

    /// Decides an `if` condition made only of compile-time names, reporting
    /// names that are no platform, processor, or capability; `None` for any
    /// other condition.
    pub fn compile_time_condition(&mut self, cx: &FnCtx, expr: &ast::Expr) -> Option<bool> {
        if !self.decided(cx, expr) {
            return None;
        }
        Some(self.decide(cx, expr))
    }

    fn decide(&mut self, cx: &FnCtx, expr: &ast::Expr) -> bool {
        match &expr.kind {
            ExprKind::Binary {
                op: BinaryOp::And,
                lhs,
                rhs,
            } => {
                // Both sides are checked for misspellings, whatever the first gives.
                let left = self.decide(cx, lhs);
                let right = self.decide(cx, rhs);
                left && right
            }
            ExprKind::Binary {
                op: BinaryOp::Or,
                lhs,
                rhs,
            } => {
                let left = self.decide(cx, lhs);
                let right = self.decide(cx, rhs);
                left || right
            }
            ExprKind::Unary { operand, .. } => !self.decide(cx, operand),
            ExprKind::Binary { op, lhs, rhs } => {
                let (name, text, text_span) = match (&lhs.kind, literal(rhs)) {
                    (ExprKind::Name(name), Some(text)) => (name, text, rhs.span),
                    _ => match &rhs.kind {
                        ExprKind::Name(name) => (name, literal(lhs).unwrap_or(""), lhs.span),
                        _ => unreachable!("decided"),
                    },
                };
                let value = self
                    .compile_time_name(cx, &name.name)
                    .expect("compile-time name");
                self.known_value(&name.name, text, text_span);
                (value == text) == (*op == BinaryOp::Equal)
            }
            ExprKind::Call { args, .. } => self.supports(&args[0]),
            _ => unreachable!("decided"),
        }
    }

    /// Reports a written platform or processor that does not exist, which
    /// would otherwise make its branch silently never apply.
    fn known_value(&mut self, name: &str, text: &str, span: sayon_syntax::Span) {
        let known = if name == "platform" {
            PLATFORMS
        } else {
            PROCESSORS
        };
        if known.contains(&text) {
            return;
        }
        let mut diagnostic =
            Diagnostic::error("E_UNKNOWN_PLATFORM", format!("\"{text}\" is not a {name}"))
                .primary(span, format!("`{name}` is never \"{text}\""));
        diagnostic = match known.iter().find(|k| close(text, k)) {
            Some(suggestion) => diagnostic.help(format!("did you mean \"{suggestion}\"?")),
            None => diagnostic.help(format!("`{name}` is one of {}", quoted(known))),
        };
        self.error(diagnostic);
    }

    /// `supports("name")`: whether the target provides the capability.
    fn supports(&mut self, arg: &ast::Expr) -> bool {
        let text = literal(arg).unwrap_or("");
        match Capability::named(text) {
            Some(capability) => self.target.provides(capability),
            None => {
                self.unknown_capability(text, arg.span);
                false
            }
        }
    }

    fn unknown_capability(&mut self, text: &str, span: sayon_syntax::Span) {
        let names: Vec<&str> = Capability::ALL.iter().map(|c| c.name()).collect();
        let mut diagnostic = Diagnostic::error(
            "E_UNKNOWN_CAPABILITY",
            format!("\"{text}\" is not a capability"),
        )
        .primary(span, "");
        diagnostic = match names.iter().find(|n| close(text, n)) {
            Some(suggestion) => diagnostic.help(format!("did you mean \"{suggestion}\"?")),
            None => diagnostic.help(format!("the capabilities are {}", quoted(&names))),
        };
        self.error(diagnostic);
    }

    /// `supports(...)` called where its name is the compiler's: a constant
    /// boolean, also outside `if` conditions.
    pub fn check_supports(
        &mut self,
        cx: &mut FnCtx,
        args: &[ast::Expr],
        span: sayon_syntax::Span,
    ) -> crate::hir::Expr {
        let value = match args {
            [arg] if literal(arg).is_some() => self.supports(arg),
            _ => {
                for arg in args {
                    self.check_expr(cx, arg, None);
                }
                self.error(
                    Diagnostic::error(
                        "E_UNKNOWN_CAPABILITY",
                        "`supports` takes the name of one capability, written as a string",
                    )
                    .primary(span, "")
                    .help("for example `supports(\"network\")`; the compiler decides it for each target"),
                );
                false
            }
        };
        crate::hir::Expr {
            kind: crate::hir::ExprKind::Boolean(value),
            ty: crate::types::Type::Boolean,
            span,
        }
    }
}

fn quoted(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    quoted.join(", ")
}

/// Whether two names are at most two edits apart.
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
