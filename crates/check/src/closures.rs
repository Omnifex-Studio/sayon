//! Function values: named functions used as values, function literals and
//! the variables they capture, and calls through function values.

use crate::checker::{Checker, FnCtx, FunctionEntry};
use crate::hir::{self, Arg, Capture, ExprKind, FuncId, LocalId, LocalKind};
use crate::narrow::block_exits;
use crate::types::{FunctionType, Type};
use sayon_syntax::ast::{self, ParamMode};
use sayon_syntax::{Diagnostic, Span};

fn expr(kind: ExprKind, ty: Type, span: Span) -> hir::Expr {
    hir::Expr { kind, ty, span }
}

impl<'a> Checker<'a> {
    /// Calls a function value: a variable, a field, or any expression whose
    /// value is a function.
    pub(crate) fn call_value(
        &mut self,
        cx: &mut FnCtx,
        callee: hir::Expr,
        args: &[ast::Expr],
        span: Span,
    ) -> hir::Expr {
        let resolved = self.unifier.resolve(&callee.ty);
        let literal =
            matches!(resolved, Type::Var(var) if self.unifier.bounds(var).literal.is_some());
        let function = match resolved {
            Type::Function(function) => *function,
            Type::Var(_) if !literal => {
                // Calling an unknown value makes it a function of these
                // arguments, taken as read borrows.
                let params = args
                    .iter()
                    .map(|arg| (ParamMode::Read, self.fresh_var(arg.span)))
                    .collect();
                let function = FunctionType {
                    params,
                    ret: self.fresh_var(span),
                };
                self.unify_at(
                    &Type::function(function.clone()),
                    &callee.ty,
                    callee.span,
                    "called as a function",
                );
                function
            }
            Type::Unknown => {
                for arg in args {
                    self.check_expr(cx, arg, None);
                }
                return expr(ExprKind::Nothing, Type::Unknown, span);
            }
            _ => {
                let shown = self.unifier.resolve_for_display(&callee.ty);
                self.error(
                    Diagnostic::error("E_NOT_CALLABLE", format!("{shown} is not a function"))
                        .primary(callee.span, "this cannot be called"),
                );
                for arg in args {
                    self.check_expr(cx, arg, None);
                }
                return expr(ExprKind::Nothing, Type::Unknown, span);
            }
        };
        if args.len() != function.params.len() {
            let expected = function.params.len();
            self.error(
                Diagnostic::error(
                    "E_ARGUMENT_COUNT",
                    format!(
                        "this function takes {expected} argument{}, but {} {} given",
                        if expected == 1 { "" } else { "s" },
                        args.len(),
                        if args.len() == 1 { "was" } else { "were" }
                    ),
                )
                .primary(span, "")
                .note("a function value takes all its parameters; defaults apply only to direct calls"),
            );
            for arg in args {
                self.check_expr(cx, arg, None);
            }
            return expr(ExprKind::Nothing, function.ret, span);
        }
        let checked = args
            .iter()
            .zip(&function.params)
            .enumerate()
            .map(|(index, (arg, (mode, ty)))| Arg {
                expr: self.check_expr_as(cx, arg, ty, &format!("argument {}", index + 1)),
                mode: *mode,
            })
            .collect();
        expr(
            ExprKind::CallValue {
                callee: Box::new(callee),
                args: checked,
            },
            function.ret,
            span,
        )
    }

    /// A named function used as a value, such as `apply(add, 1, 2)`. A
    /// generic function is instantiated as for a call.
    pub(crate) fn function_value(&mut self, id: FuncId, span: Span) -> hir::Expr {
        let (signature, params, type_args, ret) = self.instantiate(id, &[], span);
        let params = params
            .into_iter()
            .zip(&signature.explicit_modes)
            .map(|(ty, mode)| (mode.unwrap_or(ParamMode::Read), ty))
            .collect();
        expr(
            ExprKind::FunctionRef {
                function: id,
                type_args,
            },
            Type::function(FunctionType { params, ret }),
            span,
        )
    }

    /// `function(x) ...` in an expression: a closure. Its body is checked as
    /// part of the enclosing function, with the enclosing variables it uses
    /// captured into its environment.
    pub(crate) fn check_function_literal(
        &mut self,
        cx: &mut FnCtx,
        literal: &ast::Function,
        expected: Option<&Type>,
        span: Span,
    ) -> hir::Expr {
        // An expected union with one function member, such as an optional
        // callback, gives the literal that member's type.
        let expected = expected.and_then(|ty| match self.unifier.resolve(ty) {
            Type::Function(function) => Some(*function),
            Type::Union(members) => {
                let mut functions = members.into_iter().filter_map(|member| match member {
                    Type::Function(function) => Some(*function),
                    _ => None,
                });
                match (functions.next(), functions.next()) {
                    (Some(function), None) => Some(function),
                    _ => None,
                }
            }
            _ => None,
        });
        let mismatched = expected
            .as_ref()
            .is_some_and(|expected| expected.params.len() != literal.params.len());
        if let Some(expected) = &expected
            && mismatched
        {
            let count = expected.params.len();
            self.error(
                Diagnostic::error(
                    "E_ARGUMENT_COUNT",
                    format!(
                        "this function takes {} parameter{}, but one taking {count} is expected here",
                        literal.params.len(),
                        if literal.params.len() == 1 { "" } else { "s" }
                    ),
                )
                .primary(literal.header_span, ""),
            );
        }
        // Without a usable type, the literal is checked for its own errors
        // and then has no type of its own.
        let untyped = mismatched || !literal.generics.is_empty();
        if !literal.generics.is_empty() {
            self.error(
                Diagnostic::error(
                    "E_TYPE_ARGUMENTS",
                    "a function literal cannot declare type parameters",
                )
                .primary(literal.header_span, "")
                .help("its types are inferred where it is used"),
            );
        }

        let expected = if mismatched { None } else { expected };
        let generic = !literal.generics.is_empty();
        let generics = cx.generics.clone();
        let ret = match &literal.ret {
            _ if generic => Type::Unknown,
            Some(annotation) => self.resolve_type(cx.module, annotation, &generics),
            None if untyped => Type::Unknown,
            None => match &expected {
                Some(expected) => expected.ret.clone(),
                None => self.fresh_var(literal.header_span),
            },
        };
        if let (Some(annotation), Some(expected)) = (&literal.ret, &expected) {
            self.unify_at(
                &expected.ret,
                &ret,
                annotation.span,
                "the result type expected here",
            );
        }
        let mut child = FnCtx::new(cx.module, cx.func, ret.clone(), false);
        child.generics = generics.clone();
        child.captures = Some(Vec::new());

        let mut params = Vec::new();
        let mut types = Vec::new();
        for (index, param) in literal.params.iter().enumerate() {
            let expected_param = expected.as_ref().and_then(|e| e.params.get(index));
            let ty = match &param.ty {
                _ if generic => Type::Unknown,
                Some(annotation) => {
                    let ty = self.resolve_type(cx.module, annotation, &generics);
                    if let Some((_, expected_ty)) = expected_param {
                        self.unify_at(
                            expected_ty,
                            &ty,
                            annotation.span,
                            "the parameter type expected here",
                        );
                    }
                    ty
                }
                None if untyped => Type::Unknown,
                None => match expected_param {
                    Some((_, ty)) => ty.clone(),
                    None => self.fresh_var(param.name.span),
                },
            };
            let mut mode = param
                .mode
                .map(|(mode, _)| mode)
                .or(expected_param.map(|(mode, _)| *mode))
                .unwrap_or(ParamMode::Read);
            if let Some((expected_mode, _)) = expected_param
                && *expected_mode != mode
            {
                // Reported once here rather than again as a type mismatch.
                mode = *expected_mode;
                self.error(
                    Diagnostic::error(
                        "E_PARAMETER_MODE",
                        format!(
                            "the parameter `{}` must match the function type expected here",
                            param.name.name
                        ),
                    )
                    .primary(param.span, "")
                    .help(match expected_mode {
                        ParamMode::Own => format!("write `own {}`", param.name.name),
                        ParamMode::Update => format!("write `update {}`", param.name.name),
                        ParamMode::Read => "remove the `own` or `update`".to_string(),
                    }),
                );
            }
            if param.default.is_some() {
                self.error(
                    Diagnostic::error(
                        "E_DEFAULT_ORDER",
                        "a function literal's parameters cannot have defaults",
                    )
                    .primary(param.span, "")
                    .help("a function value is always called with every argument"),
                );
            }
            if child.lookup(&param.name.name).is_some() {
                self.error(
                    Diagnostic::error(
                        "E_DUPLICATE_DECLARATION",
                        format!("the parameter `{}` appears twice", param.name.name),
                    )
                    .primary(param.name.span, ""),
                );
            }
            let local = child.declare(
                &param.name.name,
                ty.clone(),
                param.name.span,
                LocalKind::Param,
            );
            params.push(hir::Param {
                local,
                mode,
                explicit_mode: true,
                ty: ty.clone(),
                default: None,
                span: param.span,
            });
            types.push((mode, ty));
        }

        // Check the body in the literal's own context, with the enclosing
        // context set aside for capturing.
        let parent = std::mem::replace(cx, child);
        self.enclosing.push(parent);
        let body = self.check_block(cx, &literal.body);
        let parent = self.enclosing.pop().expect("enclosing context");
        let child = std::mem::replace(cx, parent);

        if literal.ret.is_none()
            && !block_exits(&body)
            && self.unifier.unify(&ret, &Type::Nothing).is_err()
        {
            let shown = self.unifier.resolve(&ret);
            self.error(
                Diagnostic::error(
                    "E_MISSING_RETURN",
                    format!(
                        "this function returns {shown} on some paths but can also reach its end"
                    ),
                )
                .primary(literal.header_span, "")
                .help("return a value on every path"),
            );
        }

        let declared_result = self.unifier.resolve(&ret);
        if literal.ret.is_some()
            && !matches!(declared_result, Type::Nothing | Type::Unknown)
            && !block_exits(&body)
        {
            self.error(
                Diagnostic::error(
                    "E_MISSING_RETURN",
                    format!("this function returns {declared_result} but can reach its end without returning"),
                )
                .primary(literal.header_span, "")
                .help("add a `return` on every path"),
            );
        }

        let slots = child.captures.unwrap_or_default();
        let inner: Vec<LocalId> = slots.iter().map(|slot| slot.inner).collect();
        let mut body = body;
        let mutated: Vec<bool> = inner
            .iter()
            .map(|local| changes_local(&mut body, *local))
            .collect();
        let captures = slots
            .into_iter()
            .zip(mutated)
            .map(|(slot, mutated)| Capture {
                value: slot.value,
                mutated,
            })
            .collect();
        let function = hir::Function {
            name: "<function literal>".to_string(),
            span: literal.header_span,
            generic_count: 0,
            params,
            ret: ret.clone(),
            locals: child.locals,
            body,
            is_entry: false,
            closure: true,
            captures: inner,
            numeric_generics: Vec::new(),
        };
        let id = FuncId(self.functions.len() as u32);
        self.functions.push(FunctionEntry {
            ast: None,
            module: cx.module,
            name: function.name.clone(),
            span: literal.header_span,
            signature: None,
            generalized: true,
            hir: Some(function),
        });
        self.closures.push((id, cx.func));
        expr(
            ExprKind::Closure {
                function: id,
                captures,
            },
            if untyped {
                Type::Unknown
            } else {
                Type::function(FunctionType { params: types, ret })
            },
            span,
        )
    }
}

/// Whether a block assigns to `local` or passes it for change, which makes a
/// closure take the variable over rather than a copy of it.
fn changes_local(block: &mut hir::Block, local: LocalId) -> bool {
    let mut changed = assigns_to(block, local);
    hir::visit_block_mut(block, &mut |expr| {
        let updates = |args: &[Arg]| {
            args.iter()
                .any(|arg| arg.mode == ParamMode::Update && root_local(&arg.expr) == Some(local))
        };
        changed |= match &expr.kind {
            ExprKind::Call { args, .. } | ExprKind::Builtin { args, .. } => updates(args),
            // Calling a captured function value may change its own captures.
            ExprKind::CallValue { callee, args } => {
                root_local(callee) == Some(local) || updates(args)
            }
            ExprKind::Closure { captures, .. } => captures
                .iter()
                .any(|capture| capture.mutated && root_local(&capture.value) == Some(local)),
            _ => false,
        };
    });
    changed
}

fn assigns_to(block: &hir::Block, local: LocalId) -> bool {
    block.stmts.iter().any(|stmt| match &stmt.kind {
        hir::StmtKind::Assign { place, .. } | hir::StmtKind::CompoundAssign { place, .. } => {
            place.root() == local
        }
        hir::StmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            assigns_to(then_block, local)
                || else_block
                    .as_ref()
                    .is_some_and(|block| assigns_to(block, local))
        }
        hir::StmtKind::While { body, .. }
        | hir::StmtKind::For { body, .. }
        | hir::StmtKind::Block(body) => assigns_to(body, local),
        hir::StmtKind::Try {
            body,
            catch,
            finally,
        } => {
            assigns_to(body, local)
                || catch
                    .as_ref()
                    .is_some_and(|(_, block)| assigns_to(block, local))
                || finally
                    .as_ref()
                    .is_some_and(|block| assigns_to(block, local))
        }
        hir::StmtKind::Match { arms, .. } => arms.iter().any(|arm| assigns_to(&arm.body, local)),
        _ => false,
    })
}

fn root_local(expr: &hir::Expr) -> Option<LocalId> {
    match &expr.kind {
        ExprKind::Local { local, .. } => Some(*local),
        ExprKind::Narrow(inner)
        | ExprKind::VariantPayload { value: inner, .. }
        | ExprKind::Index { target: inner, .. }
        | ExprKind::Field { value: inner, .. } => root_local(inner),
        _ => None,
    }
}
