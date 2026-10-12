//! Flow-sensitive narrowing of union-typed locals.
//!
//! Recognized tests on a local `x` whose type is a union:
//! `x == nothing`, `x != nothing`, `typeof(x) == "name"`, `typeof(x) != "name"`,
//! combined with `not`, `and`, and `or`.

use crate::checker::{Checker, FnCtx};
use crate::hir::{self, LocalId};
use crate::types::Type;
use sayon_syntax::ast::{self, BinaryOp, UnaryOp};
use std::collections::HashMap;

#[derive(Default, Clone)]
pub(crate) struct Facts {
    pub when_true: Vec<(LocalId, Type)>,
    pub when_false: Vec<(LocalId, Type)>,
}

impl Facts {
    fn swap(self) -> Facts {
        Facts {
            when_true: self.when_false,
            when_false: self.when_true,
        }
    }
}

impl<'a> Checker<'a> {
    pub(crate) fn condition_facts(&mut self, cx: &FnCtx, condition: &ast::Expr) -> Facts {
        self.facts_with(cx, condition, &HashMap::new())
    }

    fn current_type(&self, cx: &FnCtx, local: LocalId, overlay: &HashMap<LocalId, Type>) -> Type {
        if let Some(ty) = overlay.get(&local) {
            return ty.clone();
        }
        match cx.narrowed_type(local) {
            Some(ty) => ty.clone(),
            None => self.unifier.resolve(&cx.locals[local.0 as usize].ty),
        }
    }

    fn facts_with(
        &mut self,
        cx: &FnCtx,
        condition: &ast::Expr,
        overlay: &HashMap<LocalId, Type>,
    ) -> Facts {
        match &condition.kind {
            ast::ExprKind::Unary {
                op: UnaryOp::Not,
                operand,
            } => self.facts_with(cx, operand, overlay).swap(),
            ast::ExprKind::Binary {
                op: BinaryOp::And,
                lhs,
                rhs,
            } => {
                let left = self.facts_with(cx, lhs, overlay);
                let mut inner = overlay.clone();
                inner.extend(left.when_true.iter().cloned());
                let right = self.facts_with(cx, rhs, &inner);
                let mut when_true = left.when_true;
                when_true.extend(right.when_true);
                Facts {
                    when_true,
                    when_false: Vec::new(),
                }
            }
            ast::ExprKind::Binary {
                op: BinaryOp::Or,
                lhs,
                rhs,
            } => {
                let left = self.facts_with(cx, lhs, overlay);
                let mut inner = overlay.clone();
                inner.extend(left.when_false.iter().cloned());
                let right = self.facts_with(cx, rhs, &inner);
                let mut when_false = left.when_false;
                when_false.extend(right.when_false);
                Facts {
                    when_true: Vec::new(),
                    when_false,
                }
            }
            ast::ExprKind::Binary {
                op: op @ (BinaryOp::Equal | BinaryOp::NotEqual),
                lhs,
                rhs,
            } => {
                let facts = self
                    .nothing_test(cx, lhs, rhs, overlay)
                    .or_else(|| self.nothing_test(cx, rhs, lhs, overlay))
                    .or_else(|| self.typeof_test(cx, lhs, rhs, overlay))
                    .or_else(|| self.typeof_test(cx, rhs, lhs, overlay))
                    .unwrap_or_default();
                if *op == BinaryOp::NotEqual {
                    facts.swap()
                } else {
                    facts
                }
            }
            _ => Facts::default(),
        }
    }

    fn union_local(
        &self,
        cx: &FnCtx,
        expr: &ast::Expr,
        overlay: &HashMap<LocalId, Type>,
    ) -> Option<(LocalId, Vec<Type>)> {
        let ast::ExprKind::Name(name) = &expr.kind else {
            return None;
        };
        let local = cx.lookup(&name.name)?;
        match self.current_type(cx, local, overlay) {
            Type::Union(members) => Some((local, members)),
            _ => None,
        }
    }

    /// Facts for `value == nothing`.
    fn nothing_test(
        &self,
        cx: &FnCtx,
        value: &ast::Expr,
        other: &ast::Expr,
        overlay: &HashMap<LocalId, Type>,
    ) -> Option<Facts> {
        if !matches!(other.kind, ast::ExprKind::Nothing) {
            return None;
        }
        let (local, members) = self.union_local(cx, value, overlay)?;
        if !members.contains(&Type::Nothing) {
            return None;
        }
        let rest = Type::union(
            members
                .iter()
                .filter(|m| **m != Type::Nothing)
                .cloned()
                .collect::<Vec<_>>(),
        );
        Some(Facts {
            when_true: vec![(local, Type::Nothing)],
            when_false: vec![(local, rest)],
        })
    }

    /// Facts for `typeof(value) == "name"`.
    fn typeof_test(
        &self,
        cx: &FnCtx,
        call: &ast::Expr,
        other: &ast::Expr,
        overlay: &HashMap<LocalId, Type>,
    ) -> Option<Facts> {
        let ast::ExprKind::Call { callee, args, .. } = &call.kind else {
            return None;
        };
        let ast::ExprKind::Name(name) = &callee.kind else {
            return None;
        };
        if name.name != "typeof"
            || args.len() != 1
            || cx.lookup("typeof").is_some()
            || self.items.contains_key("typeof")
        {
            return None;
        }
        let ast::ExprKind::String(segments) = &other.kind else {
            return None;
        };
        let [ast::StringSegment::Text(type_name)] = segments.as_slice() else {
            return None;
        };
        let (local, members) = self.union_local(cx, &args[0], overlay)?;
        let member = members
            .iter()
            .find(|m| m.type_name() == *type_name)?
            .clone();
        let rest = Type::union(
            members
                .iter()
                .filter(|m| **m != member)
                .cloned()
                .collect::<Vec<_>>(),
        );
        Some(Facts {
            when_true: vec![(local, member)],
            when_false: vec![(local, rest)],
        })
    }
}

/// Whether control can never continue past this block.
pub(crate) fn block_exits(block: &hir::Block) -> bool {
    block.stmts.iter().any(stmt_exits)
}

pub(crate) fn stmt_exits(stmt: &hir::Stmt) -> bool {
    use hir::StmtKind::*;
    match &stmt.kind {
        Return(_) | Break | Continue => true,
        Expr(expr) => matches!(
            expr.kind,
            hir::ExprKind::Builtin {
                builtin: hir::Builtin::Error | hir::Builtin::Raise,
                ..
            }
        ),
        If {
            then_block,
            else_block: Some(else_block),
            ..
        } => block_exits(then_block) && block_exits(else_block),
        Block(block) => block_exits(block),
        Try {
            body,
            catch,
            finally,
        } => {
            if finally.as_ref().is_some_and(block_exits) {
                return true;
            }
            match catch {
                Some((_, handler)) => block_exits(body) && block_exits(handler),
                None => block_exits(body),
            }
        }
        While { condition, body } => {
            matches!(condition.kind, hir::ExprKind::Boolean(true)) && !breaks_out(body)
        }
        _ => false,
    }
}

/// Whether a loop body contains a `break` for this loop (not a nested one).
fn breaks_out(block: &hir::Block) -> bool {
    use hir::StmtKind::*;
    block.stmts.iter().any(|stmt| match &stmt.kind {
        Break => true,
        If {
            then_block,
            else_block,
            ..
        } => breaks_out(then_block) || else_block.as_ref().is_some_and(breaks_out),
        Block(block) => breaks_out(block),
        Try {
            body,
            catch,
            finally,
        } => {
            breaks_out(body)
                || catch.as_ref().is_some_and(|(_, b)| breaks_out(b))
                || finally.as_ref().is_some_and(breaks_out)
        }
        _ => false,
    })
}

/// Names assigned anywhere inside a block (used to drop narrowing before a loop).
pub(crate) fn assigned_names(block: &ast::Block, out: &mut Vec<String>) {
    for stmt in &block.stmts {
        match &stmt.kind {
            ast::StmtKind::Assign { target, .. } | ast::StmtKind::CompoundAssign { target, .. } => {
                if let ast::ExprKind::Name(name) = &target.kind {
                    out.push(name.name.clone());
                }
            }
            ast::StmtKind::If {
                branches,
                else_body,
            } => {
                for branch in branches {
                    assigned_names(&branch.body, out);
                }
                if let Some(block) = else_body {
                    assigned_names(block, out);
                }
            }
            ast::StmtKind::While { body, .. } | ast::StmtKind::For { body, .. } => {
                assigned_names(body, out)
            }
            ast::StmtKind::Try {
                body,
                catch,
                finally,
            } => {
                assigned_names(body, out);
                if let Some(catch) = catch {
                    assigned_names(&catch.body, out);
                }
                if let Some(block) = finally {
                    assigned_names(block, out);
                }
            }
            _ => {}
        }
    }
}
