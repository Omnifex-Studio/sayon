//! Expression checking: name resolution, inference, and HIR construction.

use crate::builtins::{self, PreludeItem};
use crate::checker::{self, Checker, Deferred, FAILURE, FnCtx, Item, Narrowing, OUTCOME};
use crate::hir::{self, Arg, Builtin, ErrorField, ExprKind, Segment, Usage};
use crate::infer::{Bounds, LiteralKind, Traits};
use crate::types::Type;
use sayon_syntax::ast::{self, BinaryOp, ParamMode, UnaryOp};
use sayon_syntax::{Diagnostic, Span};

fn expr(kind: ExprKind, ty: Type, span: Span) -> hir::Expr {
    hir::Expr { kind, ty, span }
}

impl<'a> Checker<'a> {
    /// Checks an expression against an expected type, inserting union
    /// injections where a member value is used as a union.
    pub fn check_expr_as(
        &mut self,
        cx: &mut FnCtx,
        ast: &ast::Expr,
        expected: &Type,
        context: &str,
    ) -> hir::Expr {
        let value = self.check_expr(cx, ast, Some(expected));
        self.coerce(value, expected, context)
    }

    pub fn coerce(&mut self, value: hir::Expr, expected: &Type, context: &str) -> hir::Expr {
        let target = self.unifier.shallow(expected);
        let found = self.unifier.shallow(&value.ty);
        if let Type::Union(members) = &target {
            if !matches!(found, Type::Union(_)) {
                // A literal variable adopts the one union member it can be.
                if let Type::Var(var) = found {
                    if let Some(kind) = self.unifier.bounds(var).literal {
                        let candidates: Vec<&Type> = members
                            .iter()
                            .filter(|member| match (kind, member) {
                                (LiteralKind::Integer, Type::Int(_)) => true,
                                (LiteralKind::Unsigned, Type::Int(int)) => !int.signed(),
                                (LiteralKind::Float, Type::Float(_)) => true,
                                _ => false,
                            })
                            .collect();
                        let chosen = match candidates.as_slice() {
                            [one] => Some((*one).clone()),
                            many if many.contains(&&kind.default_type()) => {
                                Some(kind.default_type())
                            }
                            _ => None,
                        };
                        if let Some(member) = chosen {
                            self.unify_at(&member, &value.ty, value.span, context);
                            return self.inject(value, target.clone());
                        }
                    }
                    self.unify_at(expected, &value.ty, value.span, context);
                    return value;
                }
                let resolved = self.unifier.resolve(&found);
                if members
                    .iter()
                    .any(|member| self.unifier.resolve(member) == resolved)
                {
                    return self.inject(value, target.clone());
                }
                // A collection literal with unresolved elements matches the one
                // union member of the same shape.
                let same_shape: Vec<&Type> = members
                    .iter()
                    .filter(|member| {
                        matches!(
                            (member, &found),
                            (Type::Array(_), Type::Array(_)) | (Type::Dict(..), Type::Dict(..))
                        )
                    })
                    .collect();
                if let [member] = same_shape.as_slice() {
                    let member = (*member).clone();
                    if self.unify_at(&member, &value.ty, value.span, context) {
                        return self.inject(value, target.clone());
                    }
                    return value;
                }
            } else if found != target {
                let found_members = self.unifier.resolve(&found);
                let target_members = self.unifier.resolve(&target);
                if !found_members.has_vars()
                    && found_members
                        .union_members()
                        .iter()
                        .all(|m| target_members.union_members().contains(m))
                {
                    return self.inject(value, target.clone());
                }
            }
        }
        self.unify_at(expected, &value.ty, value.span, context);
        value
    }

    fn inject(&mut self, value: hir::Expr, union: Type) -> hir::Expr {
        let span = value.span;
        expr(ExprKind::Inject(Box::new(value)), union, span)
    }

    pub fn check_expr(
        &mut self,
        cx: &mut FnCtx,
        ast: &ast::Expr,
        expected: Option<&Type>,
    ) -> hir::Expr {
        let span = ast.span;
        match &ast.kind {
            ast::ExprKind::Integer { value, unsigned } => {
                let kind = if *unsigned {
                    LiteralKind::Unsigned
                } else {
                    LiteralKind::Integer
                };
                let ty = self.fresh_bounded(Bounds::literal(kind), span);
                expr(ExprKind::Integer(*value as i128), ty, span)
            }
            ast::ExprKind::Float(value) => {
                let ty = self.fresh_bounded(Bounds::literal(LiteralKind::Float), span);
                expr(ExprKind::Float(*value), ty, span)
            }
            ast::ExprKind::Boolean(value) => expr(ExprKind::Boolean(*value), Type::Boolean, span),
            ast::ExprKind::Nothing => expr(ExprKind::Nothing, Type::Nothing, span),
            ast::ExprKind::String(segments) => self.check_string(cx, segments, span),
            ast::ExprKind::Name(name) => self.check_name(cx, name, expected),
            ast::ExprKind::Array(items) => self.check_array(cx, items, expected, span),
            ast::ExprKind::Dict(entries) => self.check_dict(cx, entries, expected, span),
            ast::ExprKind::Record(fields) => self.check_record(cx, fields, expected, span),
            ast::ExprKind::Call {
                callee,
                generic_args,
                args,
            } => self.check_call(cx, callee, generic_args, args, expected, span),
            ast::ExprKind::Construct {
                callee,
                generic_args,
                fields,
            } => self.check_construct(cx, callee, generic_args, fields, expected, span),
            ast::ExprKind::Index { target, index } => {
                let target = self.check_expr(cx, target, None);
                let index = self.check_expr(cx, index, None);
                let result = self.index_type(&target.ty, &index, span);
                let index = self.coerce_index(index, &target.ty);
                expr(
                    ExprKind::Index {
                        target: Box::new(target),
                        index: Box::new(index),
                    },
                    result,
                    span,
                )
            }
            ast::ExprKind::Member { target, name } => {
                self.check_member(cx, target, name, expected, span)
            }
            ast::ExprKind::Unary { op, operand } => self.check_unary(cx, *op, operand, span),
            ast::ExprKind::Binary { op, lhs, rhs } => self.check_binary(cx, *op, lhs, rhs, span),
            ast::ExprKind::TryExpr(inner) => {
                // `result: Outcome<T> = try ...` gives the value the type T.
                let inside = expected.and_then(|e| match self.unifier.resolve(e) {
                    Type::Named(named) if named.id == OUTCOME => named.args.first().cloned(),
                    _ => None,
                });
                let value = match &inside {
                    Some(ty) => self.check_expr_as(cx, inner, ty, "the value of the `try`"),
                    None => self.check_expr(cx, inner, None),
                };
                let ty = self.named_type(OUTCOME, vec![value.ty.clone()]);
                expr(ExprKind::TryExpr(Box::new(value)), ty, span)
            }
            ast::ExprKind::IsError(inner) => self.check_is_error(cx, inner, span),
            ast::ExprKind::Function(function) => {
                self.check_function_literal(cx, function, expected, span)
            }
            ast::ExprKind::Import { .. } => {
                self.error(
                    Diagnostic::error(
                        "E_IMPORT_POSITION",
                        "imports must be bound to a name at the top of a file",
                    )
                    .primary(span, "")
                    .help("write `math = import(\"src/math.sayon\")` as a top-level statement"),
                );
                expr(ExprKind::Nothing, Type::Unknown, span)
            }
            ast::ExprKind::Error => expr(ExprKind::Nothing, Type::Unknown, span),
        }
    }

    /// `value is error`, on an `Outcome` or a union that can hold an `Error`.
    fn check_is_error(&mut self, cx: &mut FnCtx, inner: &ast::Expr, span: Span) -> hir::Expr {
        let value = self.check_expr(cx, inner, None);
        let test = |kind| expr(kind, Type::Boolean, span);
        match self.unifier.resolve(&value.ty) {
            Type::Named(named) if named.id == OUTCOME => test(ExprKind::IsVariant {
                value: Box::new(value),
                variant: FAILURE,
            }),
            Type::Union(members) if members.contains(&Type::Error) => test(ExprKind::IsMember {
                value: Box::new(value),
                member: Type::Error,
            }),
            Type::Error => test(ExprKind::IsMember {
                value: Box::new(value),
                member: Type::Error,
            }),
            Type::Var(var) if self.unifier.bounds(var).literal.is_none() => {
                let outcome = self.instantiate_named(OUTCOME, span);
                self.unify_at(
                    &outcome,
                    &value.ty,
                    inner.span,
                    "`is error` tests an Outcome",
                );
                test(ExprKind::IsVariant {
                    value: Box::new(value),
                    variant: FAILURE,
                })
            }
            Type::Unknown => test(ExprKind::Boolean(false)),
            unresolved => {
                let other = self.unifier.resolve_for_display(&unresolved);
                self.error(
                    Diagnostic::error(
                        "E_TYPE_MISMATCH",
                        format!("`is error` needs an Outcome or a union that can hold an Error, not {other}"),
                    )
                    .primary(inner.span, "")
                    .help("capture an error with `try`, for example `result = try readfile(path)`"),
                );
                test(ExprKind::Boolean(false))
            }
        }
    }

    fn check_string(
        &mut self,
        cx: &mut FnCtx,
        segments: &[ast::StringSegment],
        span: Span,
    ) -> hir::Expr {
        if segments
            .iter()
            .all(|s| matches!(s, ast::StringSegment::Text(_)))
        {
            let text: String = segments
                .iter()
                .map(|s| match s {
                    ast::StringSegment::Text(text) => text.as_str(),
                    ast::StringSegment::Interpolation(_) => "",
                })
                .collect();
            return expr(ExprKind::String(text), Type::String, span);
        }
        let parts = segments
            .iter()
            .map(|segment| match segment {
                ast::StringSegment::Text(text) => Segment::Text(text.clone()),
                ast::StringSegment::Interpolation(inner) => {
                    Segment::Value(self.check_expr(cx, inner, None))
                }
            })
            .collect();
        expr(ExprKind::Interpolate(parts), Type::String, span)
    }

    pub fn local_use(&mut self, cx: &FnCtx, local: hir::LocalId, span: Span) -> hir::Expr {
        let declared = cx.locals[local.0 as usize].ty.clone();
        let base = expr(
            ExprKind::Local {
                local,
                usage: Usage::Unset,
            },
            declared,
            span,
        );
        match cx.narrowed(local) {
            Some(Narrowing {
                ty,
                variant: Some(variant),
            }) => expr(
                ExprKind::VariantPayload {
                    value: Box::new(base),
                    variant: *variant,
                },
                ty.clone(),
                span,
            ),
            Some(narrowing) => expr(ExprKind::Narrow(Box::new(base)), narrowing.ty.clone(), span),
            None => base,
        }
    }

    fn check_name(
        &mut self,
        cx: &mut FnCtx,
        name: &ast::Ident,
        expected: Option<&Type>,
    ) -> hir::Expr {
        let span = name.span;
        if let Some(local) = self.find_local(cx, &name.name, span) {
            return self.local_use(cx, local, span);
        }
        if let Some(item) = self.item(cx.module, &name.name) {
            return self.item_value(item, span);
        }
        if name.name == "Outcome" {
            return self.type_as_value(OUTCOME, span);
        }
        if let Some((known, id, variant)) = self.variant_for_name(cx, &name.name, expected) {
            return self.check_variant(cx, id, variant, known, name, None, expected, span);
        }
        if self.ambiguous_variant(cx, name) {
            return expr(ExprKind::Nothing, Type::Unknown, span);
        }
        if self.import(cx.module, &name.name).is_some() {
            self.error(
                Diagnostic::error(
                    "E_MODULE_VALUE",
                    format!("`{}` is a module, not a value", name.name),
                )
                .primary(span, "")
                .help(format!(
                    "use one of its definitions, such as `{}.name`",
                    name.name
                )),
            );
            return expr(ExprKind::Nothing, Type::Unknown, span);
        }
        if builtins::lookup(&name.name, self.modules[cx.module.0 as usize].standard).is_some() {
            self.error(
                Diagnostic::error(
                    "E_BUILTIN_VALUE",
                    format!("`{}` is a built-in function and must be called", name.name),
                )
                .primary(span, "add arguments in parentheses"),
            );
            return expr(ExprKind::Nothing, Type::Unknown, span);
        }
        self.undefined_name(cx, name);
        expr(ExprKind::Nothing, Type::Unknown, span)
    }

    pub fn undefined_name(&mut self, cx: &FnCtx, name: &ast::Ident) {
        if let Some((feature, milestone)) = builtins::planned(&name.name) {
            self.unavailable(name.span, feature, milestone);
            return;
        }
        let mut diagnostic = Diagnostic::error(
            "E_UNDEFINED_NAME",
            format!("`{}` is not defined", name.name),
        )
        .primary(name.span, "not found in this scope");
        if !cx.is_script
            && cx.func.is_some()
            && self.modules[cx.module.0 as usize].entry
            && self.script_names.contains(&name.name)
        {
            diagnostic = diagnostic
                .note(format!("`{}` is a variable of the script's top level; functions cannot see script variables", name.name))
                .help("pass it to the function as a parameter, or declare it with `constant` if it never changes");
        } else if let Some(suggestion) = self.suggest_name(cx, &name.name) {
            diagnostic = diagnostic.help(format!("did you mean `{suggestion}`?"));
        }
        self.error(diagnostic);
    }

    fn suggest_name(&self, cx: &FnCtx, name: &str) -> Option<String> {
        let mut candidates: Vec<String> = cx
            .scopes
            .iter()
            .flat_map(|s| s.names.keys().cloned())
            .collect();
        candidates.extend(self.modules[cx.module.0 as usize].items.keys().cloned());
        candidates.extend(self.modules[cx.module.0 as usize].imports.keys().cloned());
        candidates.extend(
            [
                "print",
                "length",
                "typeof",
                "assert",
                "range",
                "tonumber",
                "tostring",
                "push",
                "pop",
                "haskey",
                "readfile",
                "writefile",
                "clone",
                "error",
                "raise",
                "move",
                "bytelength",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        candidates
            .into_iter()
            .filter(|candidate| candidate != name)
            .map(|candidate| (edit_distance(name, &candidate), candidate))
            .filter(|(distance, candidate)| {
                *distance <= 2.max(candidate.len() / 4) && *distance < name.len()
            })
            .min()
            .map(|(_, candidate)| candidate)
    }

    fn check_array(
        &mut self,
        cx: &mut FnCtx,
        items: &[ast::Expr],
        expected: Option<&Type>,
        span: Span,
    ) -> hir::Expr {
        let expected = expected.map(|e| self.unifier.shallow(e));
        if items.is_empty() {
            return match expected {
                Some(ty @ (Type::Array(_) | Type::Dict(..))) => {
                    if let Type::Dict(..) = ty {
                        self.typed_dict_literals.insert(span);
                    }
                    expr(ExprKind::Array(Vec::new()), ty, span)
                }
                _ => {
                    let ty = self.fresh_var(span);
                    self.deferred.push(Deferred::EmptyCollection {
                        ty: ty.clone(),
                        span,
                    });
                    expr(ExprKind::Array(Vec::new()), ty, span)
                }
            };
        }
        if let Some(Type::Array(element)) = &expected {
            let element = (**element).clone();
            let values = items
                .iter()
                .map(|item| self.check_expr_as(cx, item, &element, "array element"))
                .collect();
            return expr(ExprKind::Array(values), Type::array(element), span);
        }
        let mut values: Vec<hir::Expr> = items
            .iter()
            .map(|item| self.check_expr(cx, item, None))
            .collect();
        if let Some(union) = self.heterogeneous_element_type(&values) {
            // A heterogeneous array infers the union of its element types.
            values = values
                .into_iter()
                .map(|value| self.coerce(value, &union, "array element"))
                .collect();
            return expr(ExprKind::Array(values), Type::array(union), span);
        }
        let element = values[0].ty.clone();
        for value in &values[1..] {
            self.unify_at(
                &element,
                &value.ty,
                value.span,
                "array elements must have one type",
            );
        }
        expr(ExprKind::Array(values), Type::array(element), span)
    }

    /// The union type of an array literal whose elements have different
    /// types, or `None` when the elements should simply share one type.
    fn heterogeneous_element_type(&mut self, values: &[hir::Expr]) -> Option<Type> {
        let resolved: Vec<Type> = values.iter().map(|v| self.unifier.resolve(&v.ty)).collect();
        let concrete: Vec<Type> = resolved
            .iter()
            .filter(|ty| !ty.has_vars())
            .cloned()
            .collect();
        if concrete.is_empty() {
            return None;
        }
        let mut members: Vec<Type> = Vec::new();
        for ty in &resolved {
            if !ty.has_vars() {
                members.push(ty.clone());
                continue;
            }
            // Only literal variables can join a union; anything else unifies.
            let Type::Var(var) = ty else { return None };
            let kind = self.unifier.bounds(*var).literal?;
            let fits = |candidate: &&Type| match (kind, *candidate) {
                (LiteralKind::Integer, Type::Int(_)) | (LiteralKind::Float, Type::Float(_)) => true,
                (LiteralKind::Unsigned, Type::Int(int)) => !int.signed(),
                _ => false,
            };
            match concrete.iter().find(fits) {
                Some(found) => members.push(found.clone()),
                None => members.push(kind.default_type()),
            }
        }
        let union = Type::union(members);
        matches!(union, Type::Union(_)).then_some(union)
    }

    pub(crate) fn check_dict(
        &mut self,
        cx: &mut FnCtx,
        entries: &[(ast::Expr, ast::Expr)],
        expected: Option<&Type>,
        span: Span,
    ) -> hir::Expr {
        let (key_type, value_type) = match expected.map(|e| self.unifier.shallow(e)) {
            Some(Type::Dict(key, value)) => {
                self.typed_dict_literals.insert(span);
                ((*key).clone(), (*value).clone())
            }
            _ => (self.fresh_var(span), self.fresh_var(span)),
        };
        let mut checked = Vec::new();
        let mut seen: Vec<(String, Span)> = Vec::new();
        for (key, value) in entries {
            let key_expr =
                self.check_expr_as(cx, key, &key_type, "dictionary keys must have one type");
            let literal = match &key_expr.kind {
                ExprKind::String(text) => Some(format!("\"{text}\"")),
                ExprKind::Integer(number) => Some(number.to_string()),
                ExprKind::Boolean(flag) => Some(flag.to_string()),
                _ => None,
            };
            if let Some(shown) = literal {
                if let Some((_, first)) = seen.iter().find(|(k, _)| *k == shown) {
                    self.error(
                        Diagnostic::error(
                            "E_DUPLICATE_KEY",
                            format!("the key {shown} appears more than once"),
                        )
                        .primary(key.span, "duplicate key")
                        .secondary(*first, "first used here"),
                    );
                } else {
                    seen.push((shown, key.span));
                }
            }
            let value_expr = self.check_expr_as(
                cx,
                value,
                &value_type,
                "dictionary values must have one type",
            );
            checked.push((key_expr, value_expr));
        }
        expr(
            ExprKind::Dict(checked),
            Type::dict(key_type, value_type),
            span,
        )
    }

    /// The element type produced by indexing `target`, deferring the decision
    /// while the target's type is unknown.
    pub fn index_type(&mut self, target: &Type, index: &hir::Expr, span: Span) -> Type {
        match self.unifier.shallow(target) {
            Type::Array(element) | Type::View(element, _) => *element,
            Type::Dict(_, value) => *value,
            // A string's character at a position, as a string.
            Type::String => Type::String,
            Type::Unknown => Type::Unknown,
            Type::Var(_) => {
                let result = self.fresh_var(span);
                self.deferred.push(Deferred::Index {
                    container: target.clone(),
                    index: index.ty.clone(),
                    result: result.clone(),
                    span,
                });
                result
            }
            other => {
                self.not_indexable(&other, span);
                Type::Unknown
            }
        }
    }

    pub fn coerce_index(&mut self, index: hir::Expr, target: &Type) -> hir::Expr {
        match self.unifier.shallow(target) {
            Type::Array(_) | Type::View(..) => {
                self.coerce(index, &Type::INTEGER, "array indexes are integers")
            }
            Type::String => self.coerce(index, &Type::INTEGER, "string positions are integers"),
            Type::Dict(key, _) => self.coerce(index, &key, "dictionary key"),
            _ => index,
        }
    }

    fn check_member(
        &mut self,
        cx: &mut FnCtx,
        target: &ast::Expr,
        name: &ast::Ident,
        expected: Option<&Type>,
        span: Span,
    ) -> hir::Expr {
        if let Some(module) = self.module_alias(cx, target) {
            return match self.module_member(cx.module, module, name) {
                Some(item) => self.item_value(item, span),
                None => expr(ExprKind::Nothing, Type::Unknown, span),
            };
        }
        if let Some(id) = self.type_reference(cx, target) {
            return self.qualified_variant(cx, id, name, None, expected, span);
        }
        let value = self.check_expr(cx, target, None);
        match self.unifier.shallow(&value.ty) {
            Type::Error => {
                let field = match name.name.as_str() {
                    "message" => ErrorField::Message,
                    "code" => ErrorField::Code,
                    other => {
                        self.error(
                            Diagnostic::error(
                                "E_UNKNOWN_FIELD",
                                format!("Error has no field `{other}`"),
                            )
                            .primary(name.span, "")
                            .help("errors have `message` and `code`"),
                        );
                        return expr(ExprKind::Nothing, Type::Unknown, span);
                    }
                };
                expr(
                    ExprKind::ErrorField {
                        value: Box::new(value),
                        field,
                    },
                    Type::String,
                    span,
                )
            }
            Type::Unknown => expr(ExprKind::Nothing, Type::Unknown, span),
            _ => self.check_field(cx, value, name, span),
        }
    }

    fn check_unary(
        &mut self,
        cx: &mut FnCtx,
        op: UnaryOp,
        operand: &ast::Expr,
        span: Span,
    ) -> hir::Expr {
        match op {
            UnaryOp::Negate => {
                let value = self.check_expr(cx, operand, None);
                if let ExprKind::Integer(literal) = value.kind {
                    // The literal's range check reports `-1` for an unsigned type.
                    return expr(ExprKind::Integer(-literal), value.ty, span);
                }
                self.require_traits(&value.ty, Traits::NEGATE, span, "cannot be negated");
                if let ExprKind::Float(literal) = value.kind {
                    return expr(ExprKind::Float(-literal), value.ty, span);
                }
                let ty = value.ty.clone();
                expr(
                    ExprKind::Unary {
                        op,
                        operand: Box::new(value),
                    },
                    ty,
                    span,
                )
            }
            UnaryOp::Not => {
                let value =
                    self.check_expr_as(cx, operand, &Type::Boolean, "`not` needs a boolean");
                expr(
                    ExprKind::Unary {
                        op,
                        operand: Box::new(value),
                    },
                    Type::Boolean,
                    span,
                )
            }
        }
    }

    fn check_binary(
        &mut self,
        cx: &mut FnCtx,
        op: BinaryOp,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        span: Span,
    ) -> hir::Expr {
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let left =
                    self.check_expr_as(cx, lhs, &Type::Boolean, "logical operands are booleans");
                let facts = self.condition_facts(cx, lhs);
                let applied = if op == BinaryOp::And {
                    facts.when_true
                } else {
                    facts.when_false
                };
                cx.scopes.push(crate::checker::Scope::new());
                for (local, ty) in applied {
                    cx.scopes.last_mut().unwrap().narrowed.insert(local, ty);
                }
                let right =
                    self.check_expr_as(cx, rhs, &Type::Boolean, "logical operands are booleans");
                cx.scopes.pop();
                expr(
                    ExprKind::Binary {
                        op,
                        lhs: Box::new(left),
                        rhs: Box::new(right),
                    },
                    Type::Boolean,
                    span,
                )
            }
            BinaryOp::Equal | BinaryOp::NotEqual => self.check_equality(cx, op, lhs, rhs, span),
            BinaryOp::Less | BinaryOp::Greater | BinaryOp::LessEqual | BinaryOp::GreaterEqual => {
                let left = self.check_expr(cx, lhs, None);
                let right = self.check_expr(cx, rhs, None);
                self.unify_operands(&left, &right, op);
                self.require_traits(
                    &left.ty,
                    Traits::ORDER,
                    span,
                    "values must be numbers or strings to be ordered",
                );
                expr(
                    ExprKind::Binary {
                        op,
                        lhs: Box::new(left),
                        rhs: Box::new(right),
                    },
                    Type::Boolean,
                    span,
                )
            }
            _ => {
                let left = self.check_expr(cx, lhs, None);
                let right = self.check_expr(cx, rhs, None);
                self.unify_operands(&left, &right, op);
                let traits = if op == BinaryOp::Add {
                    Traits::ADD
                } else {
                    Traits::ARITHMETIC
                };
                let context = if op == BinaryOp::Add {
                    "`+` adds numbers or joins strings"
                } else {
                    "arithmetic needs numbers"
                };
                self.require_traits(&left.ty, traits, span, context);
                let ty = left.ty.clone();
                expr(
                    ExprKind::Binary {
                        op,
                        lhs: Box::new(left),
                        rhs: Box::new(right),
                    },
                    ty,
                    span,
                )
            }
        }
    }

    fn unify_operands(&mut self, left: &hir::Expr, right: &hir::Expr, op: BinaryOp) {
        if let Err(error) = self.unifier.unify(&left.ty, &right.ty) {
            let l = self.unifier.resolve_for_display(&left.ty);
            let r = self.unifier.resolve_for_display(&right.ty);
            let mut diagnostic = Diagnostic::error(
                "E_TYPE_MISMATCH",
                format!(
                    "`{}` needs operands of the same type, found {l} and {r}",
                    op.symbol()
                ),
            )
            .primary(right.span, format!("this is {r}"))
            .secondary(left.span, format!("this is {l}"));
            let help = match (&l, &r) {
                (Type::Int(_), Type::Float(f)) | (Type::Float(f), Type::Int(_)) => Some(format!(
                    "Sayon never converts numbers implicitly; use {}(...) on the integer",
                    f.name()
                )),
                (Type::String, Type::Int(_) | Type::Float(_))
                | (Type::Int(_) | Type::Float(_), Type::String) => Some(
                    "use interpolation such as \"{value}\" or tostring(...) to build text"
                        .to_string(),
                ),
                (Type::Union(_), _) | (_, Type::Union(_)) => Some(
                    "check which kind of value it is first, for example `if value != nothing`"
                        .to_string(),
                ),
                _ => None,
            };
            if let Some(help) = help {
                diagnostic = diagnostic.help(help);
            }
            let _ = error;
            self.error(diagnostic);
        }
    }

    fn check_equality(
        &mut self,
        cx: &mut FnCtx,
        op: BinaryOp,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        span: Span,
    ) -> hir::Expr {
        let left = self.check_expr(cx, lhs, None);
        let right = self.check_expr(cx, rhs, None);
        let negate = op == BinaryOp::NotEqual;
        // `value == nothing` tests the union tag.
        for (value, other) in [(&left, &right), (&right, &left)] {
            if matches!(other.kind, ExprKind::Nothing)
                && let Type::Union(members) = self.unifier.shallow(&value.ty)
                && members.contains(&Type::Nothing)
            {
                let test = expr(
                    ExprKind::IsMember {
                        value: Box::new(value.clone()),
                        member: Type::Nothing,
                    },
                    Type::Boolean,
                    span,
                );
                return if negate {
                    expr(
                        ExprKind::Unary {
                            op: UnaryOp::Not,
                            operand: Box::new(test),
                        },
                        Type::Boolean,
                        span,
                    )
                } else {
                    test
                };
            }
        }
        // A union compared with one of its members is equal when it holds that
        // member with an equal value; no union value is built for the member.
        let lt = self.unifier.shallow(&left.ty);
        let rt = self.unifier.shallow(&right.ty);
        match (&lt, &rt) {
            (Type::Union(members), other) if !matches!(other, Type::Union(_)) => {
                self.require_member(members, &right, op);
            }
            (other, Type::Union(members)) if !matches!(other, Type::Union(_)) => {
                self.require_member(members, &left, op);
            }
            _ => self.unify_operands(&left, &right, op),
        }
        self.require_traits(
            &left.ty,
            Traits::EQUALITY,
            span,
            "these values cannot be compared",
        );
        expr(
            ExprKind::Binary {
                op,
                lhs: Box::new(left),
                rhs: Box::new(right),
            },
            Type::Boolean,
            span,
        )
    }

    /// Checks that `value` has the type of one member of a union, letting a
    /// literal adopt the member it can be.
    fn require_member(&mut self, members: &[Type], value: &hir::Expr, op: BinaryOp) {
        let found = self.unifier.shallow(&value.ty);
        if let Type::Var(var) = found {
            if let Some(kind) = self.unifier.bounds(var).literal {
                let candidates: Vec<&Type> = members
                    .iter()
                    .filter(|member| match (kind, member) {
                        (LiteralKind::Integer, Type::Int(_))
                        | (LiteralKind::Float, Type::Float(_)) => true,
                        (LiteralKind::Unsigned, Type::Int(int)) => !int.signed(),
                        _ => false,
                    })
                    .collect();
                let chosen = match candidates.as_slice() {
                    [one] => Some((*one).clone()),
                    many if many.contains(&&kind.default_type()) => Some(kind.default_type()),
                    _ => None,
                };
                if let Some(member) = chosen {
                    self.unify_at(&member, &value.ty, value.span, "compared value");
                    return;
                }
            }
            let union = Type::union(members.to_vec());
            self.unify_at(&union, &value.ty, value.span, "compared value");
            return;
        }
        let resolved = self.unifier.resolve(&found);
        if !members
            .iter()
            .any(|member| self.unifier.resolve(member) == resolved)
        {
            let union = self.unifier.resolve(&Type::union(members.to_vec()));
            self.error(
                Diagnostic::error(
                    "E_TYPE_MISMATCH",
                    format!(
                        "`{}` compares {union} with {resolved}, which it can never hold",
                        op.symbol()
                    ),
                )
                .primary(value.span, format!("this is {resolved}")),
            );
        }
    }

    // ----- calls ----------------------------------------------------------------

    fn check_call(
        &mut self,
        cx: &mut FnCtx,
        callee: &ast::Expr,
        generic_args: &[ast::TypeExpr],
        args: &[ast::Expr],
        expected: Option<&Type>,
        span: Span,
    ) -> hir::Expr {
        let name = match &callee.kind {
            ast::ExprKind::Name(name) => name,
            ast::ExprKind::Member { target, name } if self.module_alias(cx, target).is_some() => {
                let module = self.module_alias(cx, target).expect("module");
                return match self.module_member(cx.module, module, name) {
                    Some(Item::Function(id)) => {
                        self.call_function(cx, id, name, generic_args, args, span)
                    }
                    Some(Item::Constant(_)) => {
                        let value = self.check_expr(cx, callee, None);
                        self.call_value(cx, value, args, span)
                    }
                    Some(Item::Type(id)) => self.positional_construct(cx, id, args, span),
                    None => {
                        for arg in args {
                            self.check_expr(cx, arg, None);
                        }
                        expr(ExprKind::Nothing, Type::Unknown, span)
                    }
                };
            }
            ast::ExprKind::Member { target, name } if self.type_reference(cx, target).is_some() => {
                let id = self.type_reference(cx, target).expect("type");
                return self.qualified_variant(cx, id, name, Some(args), expected, span);
            }
            ast::ExprKind::Member { target, name } => {
                let value = self.check_expr(cx, target, None);
                let resolved = self.unifier.resolve(&value.ty);
                if let Type::Named(named) = &resolved {
                    // A structure field holding a function value can be called.
                    let def = self.type_def(named.id);
                    if !def.is_enum() && def.field_index(&name.name).is_some() {
                        let field = self.check_field(cx, value, name, callee.span);
                        return self.call_value(cx, field, args, span);
                    }
                    if let Some(id) = self.method(named.id, &name.name) {
                        return self.call_method(cx, id, name, value, generic_args, args, span);
                    }
                }
                self.unknown_method(&resolved, name);
                for arg in args {
                    self.check_expr(cx, arg, None);
                }
                return expr(ExprKind::Nothing, Type::Unknown, span);
            }
            _ => {
                let value = self.check_expr(cx, callee, None);
                return self.call_value(cx, value, args, span);
            }
        };

        if let Some(local) = self.find_local(cx, &name.name, name.span) {
            if !generic_args.is_empty() {
                self.error(
                    Diagnostic::error(
                        "E_TYPE_ARGUMENTS",
                        format!(
                            "`{}` holds a function value, which takes no type arguments",
                            name.name
                        ),
                    )
                    .primary(callee.span, ""),
                );
            }
            let value = self.local_use(cx, local, name.span);
            return self.call_value(cx, value, args, span);
        }
        match self.item(cx.module, &name.name) {
            Some(Item::Function(id)) => {
                return self.call_function(cx, id, name, generic_args, args, span);
            }
            // A constant holding a function value can be called.
            Some(Item::Constant(_)) => {
                if !generic_args.is_empty() {
                    self.error(
                        Diagnostic::error(
                            "E_TYPE_ARGUMENTS",
                            format!(
                                "`{}` holds a function value, which takes no type arguments",
                                name.name
                            ),
                        )
                        .primary(callee.span, ""),
                    );
                }
                let value = self.check_expr(cx, callee, None);
                return self.call_value(cx, value, args, span);
            }
            Some(Item::Type(id)) => return self.positional_construct(cx, id, args, span),
            None => {}
        }
        // A variant of the expected enum comes before the prelude, so that an
        // enum may have a variant called, for example, `error`.
        if let Some((ty, variant)) = self.expected_variant(expected, &name.name) {
            let id = ty.as_named().expect("enum").id;
            return self.check_variant(cx, id, variant, Some(ty), name, Some(args), expected, span);
        }
        if builtins::lookup(&name.name, self.modules[cx.module.0 as usize].standard).is_none() {
            if let Some((known, id, variant)) = self.variant_for_name(cx, &name.name, expected) {
                return self.check_variant(
                    cx,
                    id,
                    variant,
                    known,
                    name,
                    Some(args),
                    expected,
                    span,
                );
            }
            if self.ambiguous_variant(cx, name) {
                for arg in args {
                    self.check_expr(cx, arg, None);
                }
                return expr(ExprKind::Nothing, Type::Unknown, span);
            }
        }
        if !generic_args.is_empty() {
            self.error(
                Diagnostic::error(
                    "E_TYPE_ARGUMENTS",
                    format!("`{}` does not take type arguments", name.name),
                )
                .primary(callee.span, ""),
            );
        }
        match builtins::lookup(&name.name, self.modules[cx.module.0 as usize].standard) {
            Some(PreludeItem::Builtin(builtin)) => {
                self.check_builtin(cx, builtin, name, args, expected, span)
            }
            Some(PreludeItem::Move) => self.check_move(cx, args, span),
            Some(PreludeItem::Conversion(target)) => {
                if !self.arity(name, args, 1, 1, span) {
                    return expr(ExprKind::Nothing, target, span);
                }
                let value = self.check_expr(cx, &args[0], None);
                self.require_traits(
                    &value.ty,
                    Traits::NUMERIC,
                    args[0].span,
                    "only numbers can be converted",
                );
                let args = vec![Arg {
                    expr: value,
                    mode: ParamMode::Read,
                }];
                expr(
                    ExprKind::Builtin {
                        builtin: Builtin::Convert,
                        args,
                    },
                    target,
                    span,
                )
            }
            None => {
                self.undefined_name(cx, name);
                for arg in args {
                    self.check_expr(cx, arg, None);
                }
                expr(ExprKind::Nothing, Type::Unknown, span)
            }
        }
    }

    /// The module an expression names, when it is an import alias not hidden
    /// by a local variable.
    fn module_alias(&self, cx: &FnCtx, target: &ast::Expr) -> Option<checker::ModuleId> {
        match &target.kind {
            ast::ExprKind::Name(alias) if !self.is_local_name(cx, &alias.name) => {
                self.import(cx.module, &alias.name)
            }
            _ => None,
        }
    }

    /// The value of a module-level item used as an expression.
    fn item_value(&mut self, item: Item, span: Span) -> hir::Expr {
        match item {
            Item::Constant(id) => {
                let ty = self.constant_type(id);
                expr(ExprKind::Constant(id), ty, span)
            }
            Item::Function(id) => self.function_value(id, span),
            Item::Type(id) => self.type_as_value(id, span),
        }
    }

    /// `Point(1.0, 2.0)`: structures need their field names.
    fn positional_construct(
        &mut self,
        cx: &mut FnCtx,
        id: crate::types::TypeId,
        args: &[ast::Expr],
        span: Span,
    ) -> hir::Expr {
        let def = self.type_def(id);
        let name = def.name.clone();
        let diagnostic = if def.is_enum() {
            Diagnostic::error(
                "E_NOT_CALLABLE",
                format!("`{name}` is an enum; build one of its variants instead"),
            )
            .primary(span, "")
            .help(format!(
                "for example `{name}.{}(...)`",
                def.variants()
                    .first()
                    .map(|v| v.name.as_str())
                    .unwrap_or("variant")
            ))
        } else {
            let fields: Vec<String> = def
                .fields()
                .iter()
                .map(|f| format!("{} = ...", f.name))
                .collect();
            Diagnostic::error(
                "E_NAMED_FIELDS",
                format!("a `{name}` is built with named fields"),
            )
            .primary(span, "")
            .help(format!("write `{name}({})`", fields.join(", ")))
        };
        self.error(diagnostic);
        for arg in args {
            self.check_expr(cx, arg, None);
        }
        expr(ExprKind::Nothing, Type::Unknown, span)
    }

    fn arity(
        &mut self,
        name: &ast::Ident,
        args: &[ast::Expr],
        min: usize,
        max: usize,
        span: Span,
    ) -> bool {
        if args.len() >= min && args.len() <= max {
            return true;
        }
        let expected = if min == max {
            format!("{min} argument{}", if min == 1 { "" } else { "s" })
        } else if max == usize::MAX {
            format!("at least {min} argument{}", if min == 1 { "" } else { "s" })
        } else {
            format!("{min} to {max} arguments")
        };
        self.error(
            Diagnostic::error(
                "E_ARGUMENT_COUNT",
                format!(
                    "`{}` takes {expected}, but {} {} given",
                    name.name,
                    args.len(),
                    if args.len() == 1 { "was" } else { "were" }
                ),
            )
            .primary(span, ""),
        );
        false
    }

    fn call_function(
        &mut self,
        cx: &mut FnCtx,
        id: hir::FuncId,
        name: &ast::Ident,
        generic_args: &[ast::TypeExpr],
        args: &[ast::Expr],
        span: Span,
    ) -> hir::Expr {
        let explicit: Vec<Type> = generic_args
            .iter()
            .map(|g| self.resolve_type(cx.module, g, &cx.generics))
            .collect();
        let (signature, params, type_args, ret) = self.instantiate(id, &explicit, span);
        if !self.arity(name, args, signature.required, signature.params.len(), span) {
            for arg in args {
                self.check_expr(cx, arg, None);
            }
            return expr(ExprKind::Nothing, ret, span);
        }
        let mut checked = Vec::new();
        for (index, (arg, param)) in args.iter().zip(&params).enumerate() {
            let context = format!("argument for `{}`", signature.param_names[index]);
            let value = self.check_expr_as(cx, arg, param, &context);
            let mode = signature.explicit_modes[index].unwrap_or(ParamMode::Read);
            checked.push(Arg { expr: value, mode });
        }
        expr(
            ExprKind::Call {
                function: id,
                type_args,
                args: checked,
            },
            ret,
            span,
        )
    }

    /// `value.name(args)`: a call of the method `name` of the value's type,
    /// with the value as the first argument.
    #[allow(clippy::too_many_arguments)]
    fn call_method(
        &mut self,
        cx: &mut FnCtx,
        id: hir::FuncId,
        name: &ast::Ident,
        receiver: hir::Expr,
        generic_args: &[ast::TypeExpr],
        args: &[ast::Expr],
        span: Span,
    ) -> hir::Expr {
        if self.item_is_private(Item::Function(id))
            && self.functions[id.0 as usize].module != cx.module
        {
            let function = self.functions[id.0 as usize].name.clone();
            let module = self.modules[self.functions[id.0 as usize].module.0 as usize]
                .name
                .clone();
            self.error(
                Diagnostic::error(
                    "E_PRIVATE",
                    format!("`{function}` is private to `{module}`"),
                )
                .primary(name.span, "not exported")
                .help("remove `private` from its definition to export it"),
            );
        }
        let explicit: Vec<Type> = generic_args
            .iter()
            .map(|g| self.resolve_type(cx.module, g, &cx.generics))
            .collect();
        let (signature, params, type_args, ret) = self.instantiate(id, &explicit, span);
        // The receiver is the first argument, so the counts leave it out.
        let given = args.len() + 1;
        if given < signature.required || given > signature.params.len() {
            let (min, max) = (signature.required - 1, signature.params.len() - 1);
            self.arity(name, args, min, max, span);
            for arg in args {
                self.check_expr(cx, arg, None);
            }
            return expr(ExprKind::Nothing, ret, span);
        }
        let context = format!("value whose `{}` is called", name.name);
        let receiver = self.coerce(receiver, &params[0], &context);
        let mut checked = vec![Arg {
            expr: receiver,
            mode: signature.explicit_modes[0].unwrap_or(ParamMode::Read),
        }];
        for (index, arg) in args.iter().enumerate() {
            let position = index + 1;
            let context = format!("argument for `{}`", signature.param_names[position]);
            let value = self.check_expr_as(cx, arg, &params[position], &context);
            let mode = signature.explicit_modes[position].unwrap_or(ParamMode::Read);
            checked.push(Arg { expr: value, mode });
        }
        expr(
            ExprKind::Call {
                function: id,
                type_args,
                args: checked,
            },
            ret,
            span,
        )
    }

    fn unknown_method(&mut self, receiver: &Type, name: &ast::Ident) {
        let method = &name.name;
        let diagnostic = match receiver {
            Type::Var(_) => Diagnostic::error(
                "E_UNKNOWN_METHOD",
                format!("cannot tell which `{method}` to call before this value's type is known"),
            )
            .primary(name.span, "")
            .help("declare the type of the variable or parameter, for example `file: io.File`"),
            Type::Unknown => return,
            Type::Named(named) => {
                let type_name = self.types[named.id.0 as usize].name.clone();
                let module = self.types[named.id.0 as usize].module;
                let prefixed = format!("{}_{method}", checker::snake_case(&type_name));
                let diagnostic = Diagnostic::error(
                    "E_UNKNOWN_METHOD",
                    format!("`{type_name}` has no method `{method}`"),
                )
                .primary(name.span, "")
                .note(format!(
                    "`.{method}(...)` calls a function `{prefixed}` or `{method}` declared beside `{type_name}`, whose first parameter is a `{type_name}`"
                ));
                // A function of that name whose first parameter is not
                // declared with the type.
                let near = [prefixed.as_str(), method.as_str()]
                    .into_iter()
                    .find(|candidate| {
                        matches!(self.item(module, candidate), Some(Item::Function(_)))
                    });
                match near {
                    Some(function) => diagnostic.help(format!(
                        "`{function}` is not a method of `{type_name}` until its first parameter is declared as one, such as `value: {type_name}`"
                    )),
                    None => diagnostic,
                }
            }
            other => Diagnostic::error(
                "E_UNKNOWN_METHOD",
                format!("{other} has no method `{method}`"),
            )
            .primary(name.span, "")
            .note("methods belong to structures and enums; call a function with the value instead"),
        };
        self.error(diagnostic);
    }

    fn check_move(&mut self, cx: &mut FnCtx, args: &[ast::Expr], span: Span) -> hir::Expr {
        let name = ast::Ident {
            name: "move".to_string(),
            span,
        };
        if !self.arity(&name, args, 1, 1, span) {
            return expr(ExprKind::Nothing, Type::Unknown, span);
        }
        let ast::ExprKind::Name(_) = &args[0].kind else {
            self.error(
                Diagnostic::error("E_MOVE_TARGET", "`move` takes a variable name")
                    .primary(args[0].span, "not a variable"),
            );
            return self.check_expr(cx, &args[0], None);
        };
        let mut value = self.check_expr(cx, &args[0], None);
        let target = match &mut value.kind {
            ExprKind::Narrow(inner) | ExprKind::VariantPayload { value: inner, .. } => {
                &mut inner.kind
            }
            other => other,
        };
        if let ExprKind::Local { usage, .. } = target {
            *usage = Usage::Move;
        }
        value.span = span;
        value
    }

    fn check_builtin(
        &mut self,
        cx: &mut FnCtx,
        builtin: Builtin,
        name: &ast::Ident,
        args: &[ast::Expr],
        expected: Option<&Type>,
        span: Span,
    ) -> hir::Expr {
        let (min, max) = match builtin {
            Builtin::Print => (0, usize::MAX),
            Builtin::Assert => (1, 2),
            Builtin::Range => (1, 3),
            Builtin::Swap => (3, 3),
            Builtin::View | Builtin::MutableView => (1, 3),
            Builtin::Push
            | Builtin::HasKey
            | Builtin::WriteFile
            | Builtin::Remove
            | Builtin::SplitView => (2, 2),
            Builtin::Host(host) => (host.params().len(), host.params().len()),
            _ => (1, 1),
        };
        if !self.arity(name, args, min, max, span) {
            for arg in args {
                self.check_expr(cx, arg, None);
            }
            return expr(ExprKind::Nothing, Type::Unknown, span);
        }
        let read = |expr: hir::Expr| Arg {
            expr,
            mode: ParamMode::Read,
        };
        let (checked, ty): (Vec<Arg>, Type) = match builtin {
            Builtin::Print => {
                let values = args
                    .iter()
                    .map(|a| read(self.check_expr(cx, a, None)))
                    .collect();
                (values, Type::Nothing)
            }
            Builtin::Length => {
                let value = self.check_expr(cx, &args[0], None);
                self.require_traits(
                    &value.ty,
                    Traits::LENGTH,
                    args[0].span,
                    "`length` needs a string, array, or dictionary",
                );
                (vec![read(value)], Type::INTEGER)
            }
            Builtin::ByteLength => {
                let value =
                    self.check_expr_as(cx, &args[0], &Type::String, "`bytelength` needs a string");
                (vec![read(value)], Type::INTEGER)
            }
            Builtin::TypeOf => (
                vec![read(self.check_expr(cx, &args[0], None))],
                Type::String,
            ),
            Builtin::ToString => (
                vec![read(self.check_expr(cx, &args[0], None))],
                Type::String,
            ),
            Builtin::Assert => {
                let mut values = vec![read(self.check_expr_as(
                    cx,
                    &args[0],
                    &Type::Boolean,
                    "assertions test a boolean",
                ))];
                if let Some(message) = args.get(1) {
                    values.push(read(self.check_expr_as(
                        cx,
                        message,
                        &Type::String,
                        "assertion message",
                    )));
                }
                (values, Type::Nothing)
            }
            Builtin::Range => {
                let values = args
                    .iter()
                    .map(|a| {
                        read(self.check_expr_as(cx, a, &Type::INTEGER, "range bounds are integers"))
                    })
                    .collect();
                (values, Type::Range)
            }
            Builtin::ToNumber => {
                let value = self.check_expr(cx, &args[0], None);
                let resolved = self.unifier.shallow(&value.ty);
                if resolved.is_numeric() {
                    let ty = value.ty.clone();
                    (vec![read(value)], ty)
                } else {
                    let value = self.coerce(value, &Type::String, "`tonumber` parses a string");
                    (vec![read(value)], Type::union([Type::INTEGER, Type::FLOAT]))
                }
            }
            Builtin::Push => {
                let element = self.fresh_var(span);
                let array = self.check_expr_as(
                    cx,
                    &args[0],
                    &Type::array(element.clone()),
                    "`push` adds to an array",
                );
                let value = self.check_expr_as(cx, &args[1], &element, "pushed value");
                (
                    vec![
                        Arg {
                            expr: array,
                            mode: ParamMode::Update,
                        },
                        Arg {
                            expr: value,
                            mode: ParamMode::Own,
                        },
                    ],
                    Type::Nothing,
                )
            }
            Builtin::Pop => {
                let element = self.fresh_var(span);
                let array = self.check_expr_as(
                    cx,
                    &args[0],
                    &Type::array(element.clone()),
                    "`pop` removes from an array",
                );
                (
                    vec![Arg {
                        expr: array,
                        mode: ParamMode::Update,
                    }],
                    Type::union([element, Type::Nothing]),
                )
            }
            Builtin::View | Builtin::MutableView => {
                let mutable = builtin == Builtin::MutableView;
                let base = self.check_expr(cx, &args[0], None);
                let standard = self.modules[cx.module.0 as usize].standard;
                let element = match self.unifier.resolve(&base.ty) {
                    Type::Array(element) => *element,
                    Type::View(element, base_mutable) if base_mutable || !mutable => *element,
                    // `strings.byte_view`: the UTF-8 bytes of a string, which
                    // cannot change.
                    Type::String if standard && !mutable => Type::Int(crate::types::IntType::U8),
                    Type::Unknown => Type::Unknown,
                    Type::Var(_) => {
                        let element = self.fresh_var(span);
                        let array = Type::array(element.clone());
                        self.unify_at(&array, &base.ty, args[0].span, "views are made of arrays");
                        element
                    }
                    other => {
                        let what = if mutable {
                            "an array or a mutable view"
                        } else {
                            "an array or a view"
                        };
                        self.error(
                            Diagnostic::error(
                                "E_TYPE_MISMATCH",
                                format!("`{}` takes {what}, not {other}", builtin.name()),
                            )
                            .primary(args[0].span, ""),
                        );
                        Type::Unknown
                    }
                };
                let mut checked = vec![Arg {
                    expr: base,
                    mode: if mutable {
                        ParamMode::Update
                    } else {
                        ParamMode::Read
                    },
                }];
                for bound in &args[1..] {
                    checked.push(read(self.check_expr_as(
                        cx,
                        bound,
                        &Type::INTEGER,
                        "view bounds are integers",
                    )));
                }
                (checked, Type::view(element, mutable))
            }
            Builtin::SplitView => {
                let rest = self.check_expr(cx, &args[0], None);
                let ty = match self.unifier.resolve(&rest.ty) {
                    Type::View(element, mutable) => Type::view(*element, mutable),
                    Type::Unknown => Type::Unknown,
                    other => {
                        self.error(
                            Diagnostic::error(
                                "E_TYPE_MISMATCH",
                                format!("`split_view` takes a view, not {other}"),
                            )
                            .primary(args[0].span, "")
                            .help("make one first, for example `rest = mutable_view(items)`"),
                        );
                        Type::Unknown
                    }
                };
                let count = self.check_expr_as(
                    cx,
                    &args[1],
                    &Type::INTEGER,
                    "the number of elements to split off",
                );
                let rest = Arg {
                    expr: rest,
                    mode: ParamMode::Update,
                };
                (vec![rest, read(count)], ty)
            }
            Builtin::Swap => {
                let element = self.fresh_var(span);
                let array = self.check_expr_as(
                    cx,
                    &args[0],
                    &Type::array(element),
                    "`swap` exchanges two elements of an array",
                );
                let first = self.check_expr_as(
                    cx,
                    &args[1],
                    &Type::INTEGER,
                    "array positions are integers",
                );
                let second = self.check_expr_as(
                    cx,
                    &args[2],
                    &Type::INTEGER,
                    "array positions are integers",
                );
                (
                    vec![
                        Arg {
                            expr: array,
                            mode: ParamMode::Update,
                        },
                        read(first),
                        read(second),
                    ],
                    Type::Nothing,
                )
            }
            Builtin::Remove => {
                let collection = self.check_expr(cx, &args[0], None);
                let key = self.check_expr(cx, &args[1], None);
                if let Type::Var(_) = self.unifier.shallow(&collection.ty) {
                    // Unknown: an integer position means an array.
                    let integer = match self.unifier.shallow(&key.ty) {
                        Type::Int(_) => true,
                        Type::Var(var) => self.unifier.bounds(var).literal.is_some(),
                        _ => false,
                    };
                    let shape = if integer {
                        Type::array(self.fresh_var(span))
                    } else {
                        Type::dict(key.ty.clone(), self.fresh_var(span))
                    };
                    self.unify_at(
                        &shape,
                        &collection.ty,
                        args[0].span,
                        "`remove` takes an array or a dictionary",
                    );
                }
                let (key, ty) = match self.unifier.resolve(&collection.ty) {
                    Type::Array(element) => (
                        self.coerce(key, &Type::INTEGER, "array positions are integers"),
                        *element,
                    ),
                    Type::Dict(key_type, value) => (
                        self.coerce(key, &key_type, "dictionary key"),
                        Type::union([*value, Type::Nothing]),
                    ),
                    Type::Unknown => (key, Type::Unknown),
                    other => {
                        self.error(
                            Diagnostic::error(
                                "E_TYPE_MISMATCH",
                                format!("`remove` takes an array or a dictionary, not {other}"),
                            )
                            .primary(args[0].span, ""),
                        );
                        (key, Type::Unknown)
                    }
                };
                (
                    vec![
                        Arg {
                            expr: collection,
                            mode: ParamMode::Update,
                        },
                        read(key),
                    ],
                    ty,
                )
            }
            Builtin::HasKey => {
                let key_type = self.fresh_var(span);
                let value = self.fresh_var(span);
                let dict = self.check_expr_as(
                    cx,
                    &args[0],
                    &Type::dict(key_type.clone(), value),
                    "`haskey` checks a dictionary",
                );
                let key = self.check_expr_as(cx, &args[1], &key_type, "dictionary key");
                (vec![read(dict), read(key)], Type::Boolean)
            }
            Builtin::ReadFile => {
                let path = self.check_expr_as(cx, &args[0], &Type::String, "file path");
                (vec![read(path)], Type::String)
            }
            Builtin::WriteFile => {
                let path = self.check_expr_as(cx, &args[0], &Type::String, "file path");
                let content = self.check_expr_as(cx, &args[1], &Type::String, "file content");
                (vec![read(path), read(content)], Type::Nothing)
            }
            Builtin::Clone => {
                let value = self.check_expr(cx, &args[0], None);
                self.require_traits(
                    &value.ty,
                    Traits::CLONE,
                    args[0].span,
                    "an open file has one owner; pass it with `update` instead",
                );
                // Cloning a view copies the elements it shows into an array.
                let ty = match self.unifier.resolve(&value.ty) {
                    Type::View(element, _) => Type::array(*element),
                    _ => value.ty.clone(),
                };
                (vec![read(value)], ty)
            }
            Builtin::Error => {
                let message = self.check_expr_as(cx, &args[0], &Type::String, "error message");
                (
                    vec![read(message)],
                    expected.cloned().unwrap_or(Type::Nothing),
                )
            }
            Builtin::Raise => {
                let problem =
                    self.check_expr_as(cx, &args[0], &Type::Error, "`raise` takes an Error");
                (
                    vec![Arg {
                        expr: problem,
                        mode: ParamMode::Own,
                    }],
                    expected.cloned().unwrap_or(Type::Nothing),
                )
            }
            Builtin::Convert => unreachable!("conversions are handled separately"),
            Builtin::Host(host) => {
                let values = args
                    .iter()
                    .zip(host.params())
                    .map(|(arg, param)| {
                        let ty = host_type(*param);
                        read(self.check_expr_as(
                            cx,
                            arg,
                            &ty,
                            &format!("argument for `{}`", name.name),
                        ))
                    })
                    .collect();
                (values, host_type(host.result()))
            }
        };
        expr(
            ExprKind::Builtin {
                builtin,
                args: checked,
            },
            ty,
            span,
        )
    }

    pub fn constant_type(&mut self, id: hir::ConstId) -> Type {
        self.constants[id.0 as usize]
            .ty
            .clone()
            .unwrap_or(Type::Unknown)
    }
}

/// The Sayon type of a value crossing the host boundary.
pub(crate) fn host_type(ty: sayon_runtime::host::HostType) -> Type {
    use sayon_runtime::host::HostType;
    match ty {
        HostType::String => Type::String,
        HostType::Integer => Type::INTEGER,
        HostType::Float => Type::FLOAT,
        HostType::Boolean => Type::Boolean,
        HostType::Nothing => Type::Nothing,
        HostType::StringArray => Type::array(Type::String),
        HostType::OptionalString => Type::union([Type::String, Type::Nothing]),
        HostType::Stream => Type::Stream,
        HostType::Bytes => Type::array(Type::Int(crate::types::IntType::U8)),
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut current = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            current[j] = (previous[j] + 1)
                .min(current[j - 1] + 1)
                .min(previous[j - 1] + cost);
        }
        previous = current;
    }
    previous[b.len()]
}
