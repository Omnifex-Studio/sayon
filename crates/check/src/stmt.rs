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
    /// The call that gets the next value of a loop over an iterator, whose
    /// type's `next` returns a value, or `nothing` at the end.
    fn iterator_next(
        &mut self,
        cx: &mut FnCtx,
        id: hir::FuncId,
        iterable: &hir::Expr,
        items: &[Type],
        span: Span,
    ) -> Option<Box<hir::NextCall>> {
        let (signature, params, type_args, ret) = self.instantiate(id, &[], span);
        let type_name = self.unifier.resolve(&iterable.ty).to_string();
        if params.is_empty() || signature.required > 1 {
            self.error(
                Diagnostic::error(
                    "E_NOT_ITERABLE",
                    format!("the `next` of `{type_name}` must take only the iterator"),
                )
                .primary(span, ""),
            );
            return None;
        }
        self.unify_at(&params[0], &iterable.ty, span, "iterator");
        let element = match self.unifier.resolve(&ret) {
            Type::Union(members) if members.contains(&Type::Nothing) => {
                Type::union(members.into_iter().filter(|m| *m != Type::Nothing))
            }
            _ => {
                self.error(
                    Diagnostic::error(
                        "E_NOT_ITERABLE",
                        format!("the `next` of `{type_name}` must return a value or `nothing`"),
                    )
                    .primary(span, "")
                    .help("declare its result as `T or nothing`, with `nothing` at the end"),
                );
                return None;
            }
        };
        if items.len() != 1 {
            self.error(
                Diagnostic::error("E_LOOP_BINDINGS", "this loop takes exactly one variable")
                    .primary(span, ""),
            );
            return None;
        }
        self.unify_at(&items[0], &element, span, "loop variable");
        let iterator = cx.hidden(iterable.ty.clone(), span);
        let mode = signature.explicit_modes[0].unwrap_or(ast::ParamMode::Read);
        let receiver = hir::Expr {
            kind: hir::ExprKind::Local {
                local: iterator,
                usage: hir::Usage::Unset,
            },
            ty: iterable.ty.clone(),
            span,
        };
        let call = hir::Expr {
            kind: hir::ExprKind::Call {
                function: id,
                type_args,
                args: vec![hir::Arg {
                    expr: receiver,
                    mode,
                }],
            },
            ty: ret,
            span,
        };
        Some(Box::new(hir::NextCall { iterator, call }))
    }

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
            ast::StmtKind::Expr(value) => {
                if let ast::ExprKind::TryExpr(_) = value.kind {
                    self.error(
                        Diagnostic::warning(
                            "W_UNUSED_OUTCOME",
                            "the result of this `try` is never inspected",
                        )
                        .primary(value.span, "an error here would go unnoticed")
                        .help("keep the result and test it with `is error`, or leave out `try` so the error propagates"),
                    );
                }
                let checked = self.check_expr(cx, value, None);
                if let Type::Wrapper(crate::types::Wrapper::Task, _) =
                    self.unifier.resolve(&checked.ty)
                {
                    self.error(
                        Diagnostic::warning(
                            "W_UNUSED_TASK",
                            "this task is cancelled at once, since nothing keeps its handle",
                        )
                        .primary(value.span, "")
                        .help("keep the handle and `threads.join` it, or `threads.detach` it to let it run on its own"),
                    );
                }
                StmtKind::Expr(checked)
            }
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
                let mut next = None;
                match self.unifier.shallow(&iterable_hir.ty) {
                    Type::Var(_) => self.deferred.push(Deferred::Iterate {
                        iterable: iterable_hir.ty.clone(),
                        items: item_types.clone(),
                        span: iterable.span,
                    }),
                    Type::Named(named) if self.method(named.id, "next").is_some() => {
                        let id = self.method(named.id, "next").expect("next");
                        next =
                            self.iterator_next(cx, id, &iterable_hir, &item_types, iterable.span);
                        if next.is_none() {
                            // Reported; the loop variables are unknown.
                            self.apply_iteration(&Type::Unknown, &item_types, iterable.span);
                        }
                    }
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
                    next,
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
            // A function declared in a function is a closure bound to a
            // variable: it can use the variables declared before it.
            ast::StmtKind::Function(function) => {
                let Some(name) = &function.name else {
                    return None;
                };
                if !function.generics.is_empty() {
                    self.error(
                        Diagnostic::error(
                            "E_TYPE_ARGUMENTS",
                            "a function declared inside another function cannot declare type parameters",
                        )
                        .primary(function.header_span, "")
                        .help("declare it at the top level of the module"),
                    );
                    return None;
                }
                if let Some(existing) = cx.lookup(&name.name)
                    && cx
                        .scopes
                        .last()
                        .is_some_and(|scope| scope.names.contains_key(&name.name))
                {
                    let previous = cx.locals[existing.0 as usize].span;
                    self.error(
                        Diagnostic::error(
                            "E_DUPLICATE_DECLARATION",
                            format!("`{}` is already declared in this scope", name.name),
                        )
                        .primary(name.span, "declared again here")
                        .secondary(previous, "first declared here"),
                    );
                    return None;
                }
                self.defining_nested.push(name.name.clone());
                let value = self.check_function_literal(cx, function, None, function.header_span);
                self.defining_nested.pop();
                let local =
                    cx.declare(&name.name, value.ty.clone(), name.span, LocalKind::Variable);
                return Some(stmt(StmtKind::Let { local, value }, span));
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
            ast::StmtKind::TypeDecl { name, .. } | ast::StmtKind::EnumDecl { name, .. } => {
                self.error(
                    Diagnostic::error(
                        "E_TYPE_POSITION",
                        "types are declared at the top level of a file",
                    )
                    .primary(name.span, "")
                    .help("move the declaration out of the block it is in"),
                );
                return None;
            }
            ast::StmtKind::Match { scrutinee, arms } => self.check_match(cx, scrutinee, arms),
            ast::StmtKind::Test { name_span, .. } => {
                self.error(
                    Diagnostic::error(
                        "E_TEST_POSITION",
                        "test blocks belong at the top level of a file",
                    )
                    .primary(*name_span, "")
                    .help("move the test out of the block it is in"),
                );
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
        if let Some(local) = self.find_local(cx, &name.name, name.span) {
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
        if self.import(cx.module, &name.name).is_some() {
            self.error(
                Diagnostic::error(
                    "E_ASSIGN_CONSTANT",
                    format!("`{}` names an imported module", name.name),
                )
                .primary(name.span, "cannot assign to a module"),
            );
            return None;
        }
        if let Some(item) = self.item(cx.module, &name.name) {
            let what = match item {
                Item::Function(_) => "function",
                Item::Constant(_) => "constant",
                Item::Type(_) => "type",
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
        cx.locals[local.0 as usize].implicit = true;
        Some(stmt(StmtKind::Let { local, value }, span))
    }

    fn assignable(&mut self, cx: &FnCtx, local: hir::LocalId, span: Span) -> bool {
        let info = &cx.locals[local.0 as usize];
        let message = match info.kind {
            LocalKind::Constant => format!("`{}` is a constant and cannot change", info.name),
            LocalKind::LoopVariable => {
                format!("the loop variable `{}` cannot be reassigned", info.name)
            }
            LocalKind::CaseBinding => {
                format!(
                    "`{}` is bound by a `case` and cannot be reassigned",
                    info.name
                )
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
            self.resolve_type(cx.module, ty, &generics)
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
        self.check_place_in(cx, target, false)
    }

    /// `inner` is set for the base of an element or field target, where a
    /// union local narrowed by a test is used with its narrowed type.
    fn check_place_in(
        &mut self,
        cx: &mut FnCtx,
        target: &ast::Expr,
        inner: bool,
    ) -> Option<(Place, Type)> {
        match &target.kind {
            ast::ExprKind::Name(name) => {
                let Some(local) = self.find_local(cx, &name.name, name.span) else {
                    if self.item(cx.module, &name.name).is_some() {
                        self.error(
                            Diagnostic::error(
                                "E_ASSIGN_CONSTANT",
                                format!("cannot change `{}`", name.name),
                            )
                            .primary(
                                name.span,
                                "module constants, functions, and types are fixed",
                            ),
                        );
                    } else {
                        self.undefined_name(cx, name);
                    }
                    return None;
                };
                if !self.assignable(cx, local, name.span) {
                    return None;
                }
                let declared = cx.locals[local.0 as usize].ty.clone();
                match cx.narrowed(local) {
                    Some(narrowed) if inner => Some((
                        Place::Narrow {
                            target: Box::new(Place::Local(local)),
                            ty: narrowed.ty.clone(),
                            variant: narrowed.variant,
                        },
                        narrowed.ty.clone(),
                    )),
                    _ => Some((Place::Local(local), declared)),
                }
            }
            ast::ExprKind::Member { target: base, name } => {
                if let ast::ExprKind::Name(alias) = &base.kind
                    && !self.is_local_name(cx, &alias.name)
                    && self.import(cx.module, &alias.name).is_some()
                {
                    self.error(
                        Diagnostic::error(
                            "E_ASSIGN_CONSTANT",
                            format!("cannot change `{}.{}`", alias.name, name.name),
                        )
                        .primary(target.span, "definitions of another module are fixed"),
                    );
                    return None;
                }
                let (place, base_ty) = self.check_place_in(cx, base, true)?;
                if let Type::Wrapper(crate::types::Wrapper::Shared, _) =
                    self.unifier.resolve(&base_ty)
                {
                    self.shared_immutable(target.span);
                    return None;
                }
                let field_ty = self.field_type(&base_ty, name, target.span, cx.module);
                if field_ty == Type::Unknown {
                    return None;
                }
                Some((
                    Place::Field {
                        target: Box::new(place),
                        target_ty: base_ty,
                        name: name.name.clone(),
                    },
                    field_ty,
                ))
            }
            ast::ExprKind::Index {
                target: base,
                index,
            } => {
                let (place, base_ty) = self.check_place_in(cx, base, true)?;
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
