//! Statement checking.

use crate::checker::{Checker, Deferred, FnCtx, Item, Scope};
use crate::hir::{self, LocalKind, Place, StmtKind};
use crate::infer::Traits;
use crate::narrow::{assigned_names, block_exits};
use crate::types::Type;
use sayon_syntax::ast::{self, BinaryOp, DeclKind};
use sayon_syntax::{Diagnostic, Span};

fn stmt(kind: StmtKind, span: Span) -> hir::Stmt {
    hir::Stmt { kind, span }
}

impl<'a> Checker<'a> {
    pub fn check_block(&mut self, cx: &mut FnCtx, block: &ast::Block) -> hir::Block {
        cx.scopes.push(Scope::new());
        let stmts = self.check_stmts(cx, block.stmts.iter());
        let scope = cx.scopes.pop().expect("scope");
        hir::Block {
            stmts,
            locals: scope.declared,
        }
    }

    pub fn check_stmts<'s>(
        &mut self,
        cx: &mut FnCtx,
        stmts: impl Iterator<Item = &'s ast::Stmt>,
    ) -> Vec<hir::Stmt> {
        let mut out = Vec::new();
        for stmt in stmts {
            if let Some(checked) = self.check_stmt(cx, stmt) {
                out.push(checked);
            }
        }
        out
    }

    fn check_stmt(&mut self, cx: &mut FnCtx, ast: &ast::Stmt) -> Option<hir::Stmt> {
        let span = ast.span;
        let kind = match &ast.kind {
            ast::StmtKind::Expr(value) => StmtKind::Expr(self.check_expr(cx, value, None)),
            ast::StmtKind::Assign { target, value } => {
                return self.check_assign(cx, target, value, span);
            }
            ast::StmtKind::CompoundAssign {
                target,
                op,
                op_span,
                value,
            } => {
                let (place, ty) = self.check_place(cx, target)?;
                let value = self.check_expr_as(
                    cx,
                    value,
                    &ty,
                    &format!("`{}=` needs matching types", op.symbol()),
                );
                let traits = if *op == BinaryOp::Add {
                    Traits::ADD
                } else {
                    Traits::ARITHMETIC
                };
                self.require_traits(&ty, traits, *op_span, "unsupported operation");
                if let Place::Local(local) = place {
                    cx.invalidate_narrowing(local);
                }
                StmtKind::CompoundAssign {
                    place,
                    op: *op,
                    value,
                }
            }
            ast::StmtKind::Declare(decl) => {
                if decl.kind == DeclKind::Constant && cx.is_script && cx.scopes.len() == 1 {
                    // Module constants were collected and checked as items.
                    return None;
                }
                return self.check_declaration(cx, decl, span);
            }
            ast::StmtKind::If {
                branches,
                else_body,
            } => self.check_if(cx, branches, else_body.as_ref(), 0),
            ast::StmtKind::While { condition, body } => {
                let condition_hir = self.check_expr_as(
                    cx,
                    condition,
                    &Type::Boolean,
                    "`while` conditions are booleans",
                );
                let facts = self.condition_facts(cx, condition);
                self.drop_narrowing_for_loop(cx, body);
                cx.scopes.push(Scope::new());
                for (local, ty) in facts.when_true {
                    cx.scopes.last_mut().unwrap().narrowed.insert(local, ty);
                }
                cx.loop_depth += 1;
                let body = self.check_block(cx, body);
                cx.loop_depth -= 1;
                cx.scopes.pop();
                StmtKind::While {
                    condition: condition_hir,
                    body,
                }
            }
            ast::StmtKind::For {
                bindings,
                iterable,
                body,
            } => {
                let iterable_hir = self.check_expr(cx, iterable, None);
                let item_types: Vec<Type> =
                    bindings.iter().map(|b| self.fresh_var(b.span)).collect();
                match self.unifier.shallow(&iterable_hir.ty) {
                    Type::Var(_) => self.deferred.push(Deferred::Iterate {
                        iterable: iterable_hir.ty.clone(),
                        items: item_types.clone(),
                        span: iterable.span,
                    }),
                    resolved => self.apply_iteration(&resolved, &item_types, iterable.span),
                }
                self.drop_narrowing_for_loop(cx, body);
                cx.scopes.push(Scope::new());
                let mut locals = Vec::new();
                for (binding, ty) in bindings.iter().zip(item_types) {
                    if locals
                        .iter()
                        .any(|l: &hir::LocalId| cx.locals[l.0 as usize].name == binding.name)
                    {
                        self.error(
                            Diagnostic::error(
                                "E_DUPLICATE_DECLARATION",
                                format!("`{}` is bound twice", binding.name),
                            )
                            .primary(binding.span, ""),
                        );
                    }
                    locals.push(cx.declare(
                        &binding.name,
                        ty,
                        binding.span,
                        LocalKind::LoopVariable,
                    ));
                }
                cx.loop_depth += 1;
                let body = self.check_block(cx, body);
                cx.loop_depth -= 1;
                cx.scopes.pop();
                StmtKind::For {
                    bindings: locals,
                    iterable: iterable_hir,
                    body,
                }
            }
            ast::StmtKind::Break | ast::StmtKind::Continue => {
                let word = if matches!(ast.kind, ast::StmtKind::Break) {
                    "break"
                } else {
                    "continue"
                };
                if cx.loop_depth == 0 {
                    self.error(
                        Diagnostic::error("E_OUTSIDE_LOOP", format!("`{word}` outside of a loop"))
                            .primary(span, ""),
                    );
                    return None;
                }
                if cx
                    .finally_loop_depth
                    .is_some_and(|depth| cx.loop_depth <= depth)
                {
                    self.finally_transfer(span, word);
                    return None;
                }
                if word == "break" {
                    StmtKind::Break
                } else {
                    StmtKind::Continue
                }
            }
            ast::StmtKind::Return(value) => {
                if cx.is_script {
                    self.error(
                        Diagnostic::error("E_RETURN_OUTSIDE_FUNCTION", "`return` at the top level of a script")
                            .primary(span, "")
                            .help("a script ends after its last statement; use os.exit later for an explicit status"),
                    );
                    return None;
                }
                if cx.finally_loop_depth.is_some() {
                    self.finally_transfer(span, "return");
                    return None;
                }
                let ret = cx.ret.clone();
                let value = match value {
                    Some(value) => Some(self.check_expr_as(cx, value, &ret, "returned value")),
                    None => {
                        self.unify_at(&ret, &Type::Nothing, span, "this function returns a value");
                        None
                    }
                };
                StmtKind::Return(value)
            }
            ast::StmtKind::Function(function) => {
                self.unavailable(function.header_span, "nested functions", "M2");
                return None;
            }
            ast::StmtKind::Try {
                body,
                catch,
                finally,
            } => {
                let body = self.check_block(cx, body);
                let catch = catch.as_ref().map(|clause| {
                    cx.scopes.push(Scope::new());
                    let local = cx.declare(
                        &clause.binding.name,
                        Type::Error,
                        clause.binding.span,
                        LocalKind::CaughtError,
                    );
                    let block = self.check_block(cx, &clause.body);
                    cx.scopes.pop();
                    (local, block)
                });
                let finally = finally.as_ref().map(|block| {
                    let saved = cx.finally_loop_depth.replace(cx.loop_depth);
                    let checked = self.check_block(cx, block);
                    cx.finally_loop_depth = saved;
                    checked
                });
                StmtKind::Try {
                    body,
                    catch,
                    finally,
                }
            }
            ast::StmtKind::TypeDecl { name, .. } => {
                self.unavailable(name.span, "structure types", "M2");
                return None;
            }
            ast::StmtKind::EnumDecl { name, .. } => {
                self.unavailable(name.span, "enums", "M2");
                return None;
            }
            ast::StmtKind::Match { .. } => {
                self.unavailable(span, "`match` statements", "M2");
                return None;
            }
            ast::StmtKind::Unavailable { feature, milestone } => {
                self.unavailable(span, feature, milestone);
                return None;
            }
        };
        Some(stmt(kind, span))
    }

    fn finally_transfer(&mut self, span: Span, word: &str) {
        self.error(
            Diagnostic::error(
                "E_FINALLY_TRANSFER",
                format!("`{word}` cannot leave a `finally` block"),
            )
            .primary(span, "")
            .note("a finally block always runs to its end so that cleanup completes"),
        );
    }

    fn drop_narrowing_for_loop(&mut self, cx: &mut FnCtx, body: &ast::Block) {
        let mut names = Vec::new();
        assigned_names(body, &mut names);
        for name in names {
            if let Some(local) = cx.lookup(&name) {
                cx.invalidate_narrowing(local);
            }
        }
    }

    fn check_if(
        &mut self,
        cx: &mut FnCtx,
        branches: &[ast::IfBranch],
        else_body: Option<&ast::Block>,
        index: usize,
    ) -> StmtKind {
        let branch = &branches[index];
        let condition = self.check_expr_as(
            cx,
            &branch.condition,
            &Type::Boolean,
            "`if` conditions are booleans",
        );
        let facts = self.condition_facts(cx, &branch.condition);

        cx.scopes.push(Scope::new());
        for (local, ty) in &facts.when_true {
            cx.scopes
                .last_mut()
                .unwrap()
                .narrowed
                .insert(*local, ty.clone());
        }
        let then_block = self.check_block(cx, &branch.body);
        cx.scopes.pop();

        let has_else = index + 1 < branches.len() || else_body.is_some();
        let else_block = if has_else {
            cx.scopes.push(Scope::new());
            for (local, ty) in &facts.when_false {
                cx.scopes
                    .last_mut()
                    .unwrap()
                    .narrowed
                    .insert(*local, ty.clone());
            }
            let block = if index + 1 < branches.len() {
                let span = branches[index + 1].condition.span;
                let nested = self.check_if(cx, branches, else_body, index + 1);
                hir::Block {
                    stmts: vec![stmt(nested, span)],
                    locals: Vec::new(),
                }
            } else {
                self.check_block(cx, else_body.expect("else block"))
            };
            cx.scopes.pop();
            Some(block)
        } else {
            None
        };

        // After `if x == nothing` followed by a return, `x` is known to hold a value.
        let then_exits = block_exits(&then_block);
        let else_exits = else_block.as_ref().is_some_and(block_exits);
        if then_exits && !else_exits {
            for (local, ty) in facts.when_false {
                cx.scopes.last_mut().unwrap().narrowed.insert(local, ty);
            }
        } else if else_exits && !then_exits {
            for (local, ty) in facts.when_true {
                cx.scopes.last_mut().unwrap().narrowed.insert(local, ty);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        }
    }

    fn check_assign(
        &mut self,
        cx: &mut FnCtx,
        target: &ast::Expr,
        value: &ast::Expr,
        span: Span,
    ) -> Option<hir::Stmt> {
        let ast::ExprKind::Name(name) = &target.kind else {
            let (place, ty) = self.check_place(cx, target)?;
            let value = self.check_expr_as(cx, value, &ty, "assigned value");
            return Some(stmt(StmtKind::Assign { place, value }, span));
        };
        if let ast::ExprKind::Import { .. } = &value.kind {
            self.unavailable(value.span, "module imports", "M1 (next step)");
            return None;
        }
        if let Some(local) = cx.lookup(&name.name) {
            if !self.assignable(cx, local, name.span) {
                return None;
            }
            let declared = cx.locals[local.0 as usize].ty.clone();
            let value = self.check_expr_as(
                cx,
                value,
                &declared,
                &format!("`{}` holds {}", name.name, self.unifier.resolve(&declared)),
            );
            cx.invalidate_narrowing(local);
            return Some(stmt(
                StmtKind::Assign {
                    place: Place::Local(local),
                    value,
                },
                span,
            ));
        }
        if let Some(item) = self.items.get(&name.name) {
            let what = match item {
                Item::Function(_) => "function",
                Item::Constant(_) => "constant",
            };
            self.error(
                Diagnostic::error(
                    "E_ASSIGN_CONSTANT",
                    format!("cannot assign to the {what} `{}`", name.name),
                )
                .primary(name.span, ""),
            );
            return None;
        }
        // Introduces a new local, visible after its initializer.
        let value = self.check_expr(cx, value, None);
        let ty = value.ty.clone();
        let local = cx.declare(&name.name, ty, name.span, LocalKind::Variable);
        Some(stmt(StmtKind::Let { local, value }, span))
    }

    fn assignable(&mut self, cx: &FnCtx, local: hir::LocalId, span: Span) -> bool {
        let info = &cx.locals[local.0 as usize];
        let message = match info.kind {
            LocalKind::Constant => format!("`{}` is a constant and cannot change", info.name),
            LocalKind::LoopVariable => {
                format!("the loop variable `{}` cannot be reassigned", info.name)
            }
            _ => return true,
        };
        let declared = info.span;
        self.error(
            Diagnostic::error("E_ASSIGN_CONSTANT", message)
                .primary(span, "")
                .secondary(declared, "declared here"),
        );
        false
    }

    fn check_declaration(
        &mut self,
        cx: &mut FnCtx,
        decl: &ast::Declaration,
        span: Span,
    ) -> Option<hir::Stmt> {
        let scope = cx.scopes.last().expect("scope");
        if let Some(existing) = scope.names.get(&decl.name.name) {
            let previous = cx.locals[existing.0 as usize].span;
            self.error(
                Diagnostic::error(
                    "E_DUPLICATE_DECLARATION",
                    format!("`{}` is already declared in this scope", decl.name.name),
                )
                .primary(decl.name.span, "declared again here")
                .secondary(previous, "first declared here")
                .help("assign without `variable` to update it, or choose another name"),
            );
            return None;
        }
        let annotation = decl.ty.as_ref().map(|ty| {
            let generics = cx.generics.clone();
            self.resolve_type(ty, &generics)
        });
        let value = match &annotation {
            Some(ty) => self.check_expr_as(
                cx,
                &decl.value,
                ty,
                &format!("`{}` is declared as {ty}", decl.name.name),
            ),
            None => self.check_expr(cx, &decl.value, None),
        };
        let ty = annotation.unwrap_or_else(|| value.ty.clone());
        let kind = if decl.kind == DeclKind::Constant {
            LocalKind::Constant
        } else {
            LocalKind::Variable
        };
        let local = cx.declare(&decl.name.name, ty, decl.name.span, kind);
        Some(stmt(StmtKind::Let { local, value }, span))
    }

    /// Resolves an assignment target to a place and the type stored there.
    pub fn check_place(&mut self, cx: &mut FnCtx, target: &ast::Expr) -> Option<(Place, Type)> {
        match &target.kind {
            ast::ExprKind::Name(name) => {
                let Some(local) = cx.lookup(&name.name) else {
                    if self.items.contains_key(&name.name) {
                        self.error(
                            Diagnostic::error(
                                "E_ASSIGN_CONSTANT",
                                format!("cannot change `{}`", name.name),
                            )
                            .primary(name.span, "module constants and functions are fixed"),
                        );
                    } else {
                        self.undefined_name(cx, name);
                    }
                    return None;
                };
                if !self.assignable(cx, local, name.span) {
                    return None;
                }
                Some((Place::Local(local), cx.locals[local.0 as usize].ty.clone()))
            }
            ast::ExprKind::Index {
                target: base,
                index,
            } => {
                let (place, base_ty) = self.check_place(cx, base)?;
                let index_hir = self.check_expr(cx, index, None);
                let element = self.index_type(&base_ty, &index_hir, target.span);
                let index_hir = self.coerce_index(index_hir, &base_ty);
                Some((
                    Place::Index {
                        target: Box::new(place),
                        target_ty: base_ty,
                        index: Box::new(index_hir),
                    },
                    element,
                ))
            }
            ast::ExprKind::Member { .. } => {
                self.unavailable(target.span, "structure fields", "M2");
                None
            }
            _ => {
                self.error(
                    Diagnostic::error("E_INVALID_ASSIGNMENT", "cannot assign to this expression")
                        .primary(target.span, ""),
                );
                None
            }
        }
    }
}
