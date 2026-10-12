//! Expression checking: name resolution, inference, and HIR construction.

use crate::builtins::{self, PreludeItem};
use crate::checker::{Checker, Deferred, FnCtx, Item};
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
    pub fn check_expr_as(&mut self, cx: &mut FnCtx, ast: &ast::Expr, expected: &Type, context: &str) -> hir::Expr {
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
                            many if many.contains(&&kind.default_type()) => Some(kind.default_type()),
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
                if members.iter().any(|member| self.unifier.resolve(member) == resolved) {
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
                    && found_members.union_members().iter().all(|m| target_members.union_members().contains(m))
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

    pub fn check_expr(&mut self, cx: &mut FnCtx, ast: &ast::Expr, expected: Option<&Type>) -> hir::Expr {
        let span = ast.span;
        match &ast.kind {
            ast::ExprKind::Integer { value, unsigned } => {
                let kind = if *unsigned { LiteralKind::Unsigned } else { LiteralKind::Integer };
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
            ast::ExprKind::Name(name) => self.check_name(cx, name),
            ast::ExprKind::Array(items) => self.check_array(cx, items, expected, span),
            ast::ExprKind::Dict(entries) => self.check_dict(cx, entries, expected, span),
            ast::ExprKind::Record(_) => {
                self.unavailable(span, "structure literals", "M2");
                expr(ExprKind::Nothing, Type::Unknown, span)
            }
            ast::ExprKind::Call { callee, generic_args, args } => {
                self.check_call(cx, callee, generic_args, args, expected, span)
            }
            ast::ExprKind::Index { target, index } => {
                let target = self.check_expr(cx, target, None);
                let index = self.check_expr(cx, index, None);
                let result = self.index_type(&target.ty, &index, span);
                let index = self.coerce_index(index, &target.ty);
                expr(ExprKind::Index { target: Box::new(target), index: Box::new(index) }, result, span)
            }
            ast::ExprKind::Member { target, name } => self.check_member(cx, target, name, span),
            ast::ExprKind::Unary { op, operand } => self.check_unary(cx, *op, operand, span),
            ast::ExprKind::Binary { op, lhs, rhs } => self.check_binary(cx, *op, lhs, rhs, span),
            ast::ExprKind::IsError(_) | ast::ExprKind::TryExpr(_) => {
                self.unavailable(span, "expression-level error capture (`try` expressions and `is error`)", "M2");
                expr(ExprKind::Nothing, Type::Unknown, span)
            }
            ast::ExprKind::Function(_) => {
                self.unavailable(span, "function literals and closures", "M2");
                expr(ExprKind::Nothing, Type::Unknown, span)
            }
            ast::ExprKind::Import { .. } => {
                self.error(
                    Diagnostic::error("E_IMPORT_POSITION", "imports must be bound to a name at the top of a file")
                        .primary(span, "")
                        .help("write `math = import(\"src/math.sayon\")` as a top-level statement"),
                );
                expr(ExprKind::Nothing, Type::Unknown, span)
            }
            ast::ExprKind::Error => expr(ExprKind::Nothing, Type::Unknown, span),
        }
    }

    fn check_string(&mut self, cx: &mut FnCtx, segments: &[ast::StringSegment], span: Span) -> hir::Expr {
        if segments.iter().all(|s| matches!(s, ast::StringSegment::Text(_))) {
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
                ast::StringSegment::Interpolation(inner) => Segment::Value(self.check_expr(cx, inner, None)),
            })
            .collect();
        expr(ExprKind::Interpolate(parts), Type::String, span)
    }

    pub fn local_use(&mut self, cx: &FnCtx, local: hir::LocalId, span: Span) -> hir::Expr {
        let declared = cx.locals[local.0 as usize].ty.clone();
        let base = expr(ExprKind::Local { local, usage: Usage::Unset }, declared, span);
        match cx.narrowed_type(local) {
            Some(narrowed) => expr(ExprKind::Narrow(Box::new(base)), narrowed.clone(), span),
            None => base,
        }
    }

    fn check_name(&mut self, cx: &mut FnCtx, name: &ast::Ident) -> hir::Expr {
        let span = name.span;
        if let Some(local) = cx.lookup(&name.name) {
            return self.local_use(cx, local, span);
        }
        match self.items.get(&name.name).copied() {
            Some(Item::Constant(id)) => {
                let ty = self.constant_type(id);
                return expr(ExprKind::Constant(id), ty, span);
            }
            Some(Item::Function(_)) => {
                self.unavailable(span, "function values", "M2");
                return expr(ExprKind::Nothing, Type::Unknown, span);
            }
            None => {}
        }
        if builtins::lookup(&name.name).is_some() {
            self.error(
                Diagnostic::error("E_BUILTIN_VALUE", format!("`{}` is a built-in function and must be called", name.name))
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
        let mut diagnostic = Diagnostic::error("E_UNDEFINED_NAME", format!("`{}` is not defined", name.name))
            .primary(name.span, "not found in this scope");
        if !cx.is_script && cx.func.is_some() && self.script_names.contains(&name.name) {
            diagnostic = diagnostic
                .note(format!("`{}` is a variable of the script's top level; functions cannot see script variables", name.name))
                .help("pass it to the function as a parameter, or declare it with `constant` if it never changes");
        } else if let Some(suggestion) = self.suggest_name(cx, &name.name) {
            diagnostic = diagnostic.help(format!("did you mean `{suggestion}`?"));
        }
        self.error(diagnostic);
    }

    fn suggest_name(&self, cx: &FnCtx, name: &str) -> Option<String> {
        let mut candidates: Vec<String> = cx.scopes.iter().flat_map(|s| s.names.keys().cloned()).collect();
        candidates.extend(self.items.keys().cloned());
        candidates.extend(
            [
                "print", "length", "typeof", "assert", "range", "tonumber", "tostring", "push", "pop", "haskey",
                "readfile", "writefile", "clone", "error", "raise", "move", "byte_length",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        candidates
            .into_iter()
            .filter(|candidate| candidate != name)
            .map(|candidate| (edit_distance(name, &candidate), candidate))
            .filter(|(distance, candidate)| *distance <= 2.max(candidate.len() / 4) && *distance < name.len())
            .min()
            .map(|(_, candidate)| candidate)
    }

    fn check_array(&mut self, cx: &mut FnCtx, items: &[ast::Expr], expected: Option<&Type>, span: Span) -> hir::Expr {
        let expected = expected.map(|e| self.unifier.shallow(e));
        if items.is_empty() {
            return match expected {
                Some(ty @ (Type::Array(_) | Type::Dict(..))) => expr(ExprKind::Array(Vec::new()), ty, span),
                _ => {
                    let ty = self.fresh_var(span);
                    self.deferred.push(Deferred::EmptyCollection { ty: ty.clone(), span });
                    expr(ExprKind::Array(Vec::new()), ty, span)
                }
            };
        }
        if let Some(Type::Array(element)) = &expected {
            let element = (**element).clone();
            let values = items.iter().map(|item| self.check_expr_as(cx, item, &element, "array element")).collect();
            return expr(ExprKind::Array(values), Type::array(element), span);
        }
        let mut values: Vec<hir::Expr> = items.iter().map(|item| self.check_expr(cx, item, None)).collect();
        if let Some(union) = self.heterogeneous_element_type(&values) {
            // A heterogeneous array infers the union of its element types.
            values = values.into_iter().map(|value| self.coerce(value, &union, "array element")).collect();
            return expr(ExprKind::Array(values), Type::array(union), span);
        }
        let element = values[0].ty.clone();
        for value in &values[1..] {
            self.unify_at(&element, &value.ty, value.span, "array elements must have one type");
        }
        expr(ExprKind::Array(values), Type::array(element), span)
    }

    /// The union type of an array literal whose elements have different
    /// types, or `None` when the elements should simply share one type.
    fn heterogeneous_element_type(&mut self, values: &[hir::Expr]) -> Option<Type> {
        let resolved: Vec<Type> = values.iter().map(|v| self.unifier.resolve(&v.ty)).collect();
        let concrete: Vec<Type> = resolved.iter().filter(|ty| !ty.has_vars()).cloned().collect();
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

    fn check_dict(&mut self, cx: &mut FnCtx, entries: &[(ast::Expr, ast::Expr)], expected: Option<&Type>, span: Span) -> hir::Expr {
        let value_type = match expected.map(|e| self.unifier.shallow(e)) {
            Some(Type::Dict(_, value)) => (*value).clone(),
            _ => self.fresh_var(span),
        };
        let mut checked = Vec::new();
        let mut seen: Vec<(String, Span)> = Vec::new();
        for (key, value) in entries {
            let key_expr = self.check_expr_as(cx, key, &Type::String, "dictionary keys are strings");
            if let ExprKind::String(text) = &key_expr.kind {
                if let Some((_, first)) = seen.iter().find(|(k, _)| k == text) {
                    self.error(
                        Diagnostic::error("E_DUPLICATE_KEY", format!("the key \"{text}\" appears more than once"))
                            .primary(key.span, "duplicate key")
                            .secondary(*first, "first used here"),
                    );
                } else {
                    seen.push((text.clone(), key.span));
                }
            }
            let value_expr = self.check_expr_as(cx, value, &value_type, "dictionary values must have one type");
            checked.push((key_expr, value_expr));
        }
        expr(ExprKind::Dict(checked), Type::dict(Type::String, value_type), span)
    }

    /// The element type produced by indexing `target`, deferring the decision
    /// while the target's type is unknown.
    pub fn index_type(&mut self, target: &Type, index: &hir::Expr, span: Span) -> Type {
        match self.unifier.shallow(target) {
            Type::Array(element) => *element,
            Type::Dict(_, value) => *value,
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
            Type::Array(_) => self.coerce(index, &Type::INTEGER, "array indexes are integers"),
            Type::Dict(key, _) => self.coerce(index, &key, "dictionary key"),
            _ => index,
        }
    }

    fn check_member(&mut self, cx: &mut FnCtx, target: &ast::Expr, name: &ast::Ident, span: Span) -> hir::Expr {
        let value = self.check_expr(cx, target, None);
        match self.unifier.shallow(&value.ty) {
            Type::Error => {
                let field = match name.name.as_str() {
                    "message" => ErrorField::Message,
                    "code" => ErrorField::Code,
                    other => {
                        self.error(
                            Diagnostic::error("E_UNKNOWN_FIELD", format!("Error has no field `{other}`"))
                                .primary(name.span, "")
                                .help("errors have `message` and `code`"),
                        );
                        return expr(ExprKind::Nothing, Type::Unknown, span);
                    }
                };
                expr(ExprKind::ErrorField { value: Box::new(value), field }, Type::String, span)
            }
            Type::Unknown => expr(ExprKind::Nothing, Type::Unknown, span),
            Type::Var(_) => {
                self.error(
                    Diagnostic::error("E_AMBIGUOUS_TYPE", format!("cannot infer the type that has `.{}`", name.name))
                        .primary(span, "")
                        .help("annotate the value's type"),
                );
                expr(ExprKind::Nothing, Type::Unknown, span)
            }
            other => {
                self.error(
                    Diagnostic::error("E_UNKNOWN_FIELD", format!("{other} has no field `{}`", name.name))
                        .primary(name.span, "")
                        .note("structures with named fields arrive in milestone M2"),
                );
                expr(ExprKind::Nothing, Type::Unknown, span)
            }
        }
    }

    fn check_unary(&mut self, cx: &mut FnCtx, op: UnaryOp, operand: &ast::Expr, span: Span) -> hir::Expr {
        match op {
            UnaryOp::Negate => {
                let value = self.check_expr(cx, operand, None);
                self.require_traits(&value.ty, Traits::NEGATE, span, "cannot be negated");
                if let ExprKind::Integer(literal) = value.kind {
                    return expr(ExprKind::Integer(-literal), value.ty, span);
                }
                if let ExprKind::Float(literal) = value.kind {
                    return expr(ExprKind::Float(-literal), value.ty, span);
                }
                let ty = value.ty.clone();
                expr(ExprKind::Unary { op, operand: Box::new(value) }, ty, span)
            }
            UnaryOp::Not => {
                let value = self.check_expr_as(cx, operand, &Type::Boolean, "`not` needs a boolean");
                expr(ExprKind::Unary { op, operand: Box::new(value) }, Type::Boolean, span)
            }
        }
    }

    fn check_binary(&mut self, cx: &mut FnCtx, op: BinaryOp, lhs: &ast::Expr, rhs: &ast::Expr, span: Span) -> hir::Expr {
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let left = self.check_expr_as(cx, lhs, &Type::Boolean, "logical operands are booleans");
                let facts = self.condition_facts(cx, lhs);
                let applied = if op == BinaryOp::And { facts.when_true } else { facts.when_false };
                cx.scopes.push(crate::checker::Scope::new());
                for (local, ty) in applied {
                    cx.scopes.last_mut().unwrap().narrowed.insert(local, ty);
                }
                let right = self.check_expr_as(cx, rhs, &Type::Boolean, "logical operands are booleans");
                cx.scopes.pop();
                expr(ExprKind::Binary { op, lhs: Box::new(left), rhs: Box::new(right) }, Type::Boolean, span)
            }
            BinaryOp::Equal | BinaryOp::NotEqual => self.check_equality(cx, op, lhs, rhs, span),
            BinaryOp::Less | BinaryOp::Greater | BinaryOp::LessEqual | BinaryOp::GreaterEqual => {
                let left = self.check_expr(cx, lhs, None);
                let right = self.check_expr(cx, rhs, None);
                self.unify_operands(&left, &right, op);
                self.require_traits(&left.ty, Traits::ORDER, span, "values must be numbers or strings to be ordered");
                expr(ExprKind::Binary { op, lhs: Box::new(left), rhs: Box::new(right) }, Type::Boolean, span)
            }
            _ => {
                let left = self.check_expr(cx, lhs, None);
                let right = self.check_expr(cx, rhs, None);
                self.unify_operands(&left, &right, op);
                let traits = if op == BinaryOp::Add { Traits::ADD } else { Traits::ARITHMETIC };
                let context = if op == BinaryOp::Add {
                    "`+` adds numbers or joins strings"
                } else {
                    "arithmetic needs numbers"
                };
                self.require_traits(&left.ty, traits, span, context);
                let ty = left.ty.clone();
                expr(ExprKind::Binary { op, lhs: Box::new(left), rhs: Box::new(right) }, ty, span)
            }
        }
    }

    fn unify_operands(&mut self, left: &hir::Expr, right: &hir::Expr, op: BinaryOp) {
        if let Err(error) = self.unifier.unify(&left.ty, &right.ty) {
            let l = self.unifier.resolve(&left.ty);
            let r = self.unifier.resolve(&right.ty);
            let mut diagnostic = Diagnostic::error(
                "E_TYPE_MISMATCH",
                format!("`{}` needs operands of the same type, found {l} and {r}", op.symbol()),
            )
            .primary(right.span, format!("this is {r}"))
            .secondary(left.span, format!("this is {l}"));
            let help = match (&l, &r) {
                (Type::Int(_), Type::Float(f)) | (Type::Float(f), Type::Int(_)) => {
                    Some(format!("Sayon never converts numbers implicitly; use {}(...) on the integer", f.name()))
                }
                (Type::String, Type::Int(_) | Type::Float(_)) | (Type::Int(_) | Type::Float(_), Type::String) => {
                    Some("use interpolation such as \"{value}\" or tostring(...) to build text".to_string())
                }
                (Type::Union(_), _) | (_, Type::Union(_)) => {
                    Some("check which kind of value it is first, for example `if value != nothing`".to_string())
                }
                _ => None,
            };
            if let Some(help) = help {
                diagnostic = diagnostic.help(help);
            }
            let _ = error;
            self.error(diagnostic);
        }
    }

    fn check_equality(&mut self, cx: &mut FnCtx, op: BinaryOp, lhs: &ast::Expr, rhs: &ast::Expr, span: Span) -> hir::Expr {
        let left = self.check_expr(cx, lhs, None);
        let right = self.check_expr(cx, rhs, None);
        let negate = op == BinaryOp::NotEqual;
        // `value == nothing` tests the union tag.
        for (value, other) in [(&left, &right), (&right, &left)] {
            if matches!(other.kind, ExprKind::Nothing) {
                if let Type::Union(members) = self.unifier.shallow(&value.ty) {
                    if members.contains(&Type::Nothing) {
                        let test = expr(
                            ExprKind::IsMember { value: Box::new(value.clone()), member: Type::Nothing },
                            Type::Boolean,
                            span,
                        );
                        return if negate {
                            expr(ExprKind::Unary { op: UnaryOp::Not, operand: Box::new(test) }, Type::Boolean, span)
                        } else {
                            test
                        };
                    }
                }
            }
        }
        // Comparing a union with one of its members compares as unions.
        let lt = self.unifier.shallow(&left.ty);
        let rt = self.unifier.shallow(&right.ty);
        let (left, right) = match (&lt, &rt) {
            (Type::Union(_), other) if !matches!(other, Type::Union(_) | Type::Var(_)) => {
                let right = self.coerce(right, &lt, "compared value");
                (left, right)
            }
            (other, Type::Union(_)) if !matches!(other, Type::Union(_) | Type::Var(_)) => {
                let left = self.coerce(left, &rt, "compared value");
                (left, right)
            }
            _ => {
                self.unify_operands(&left, &right, op);
                (left, right)
            }
        };
        self.require_traits(&left.ty, Traits::EQUALITY, span, "these values cannot be compared");
        expr(ExprKind::Binary { op, lhs: Box::new(left), rhs: Box::new(right) }, Type::Boolean, span)
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
            ast::ExprKind::Member { name, .. } => {
                self.error(
                    Diagnostic::error("E_UNKNOWN_METHOD", format!("no method `{}` is available here", name.name))
                        .primary(name.span, "")
                        .note("library methods such as `file.lines()` arrive with the standard library modules"),
                );
                for arg in args {
                    self.check_expr(cx, arg, None);
                }
                return expr(ExprKind::Nothing, Type::Unknown, span);
            }
            _ => {
                self.error(
                    Diagnostic::error("E_NOT_CALLABLE", "this expression cannot be called")
                        .primary(callee.span, "")
                        .note("calling function values arrives with closures in milestone M2"),
                );
                return expr(ExprKind::Nothing, Type::Unknown, span);
            }
        };

        if cx.lookup(&name.name).is_some() {
            self.unavailable(callee.span, "calling a function stored in a variable", "M2");
            return expr(ExprKind::Nothing, Type::Unknown, span);
        }
        match self.items.get(&name.name).copied() {
            Some(Item::Function(id)) => return self.call_function(cx, id, name, generic_args, args, span),
            Some(Item::Constant(_)) => {
                self.error(
                    Diagnostic::error("E_NOT_CALLABLE", format!("`{}` is a constant, not a function", name.name))
                        .primary(callee.span, ""),
                );
                return expr(ExprKind::Nothing, Type::Unknown, span);
            }
            None => {}
        }
        if !generic_args.is_empty() {
            self.error(
                Diagnostic::error("E_TYPE_ARGUMENTS", format!("`{}` does not take type arguments", name.name))
                    .primary(callee.span, ""),
            );
        }
        match builtins::lookup(&name.name) {
            Some(PreludeItem::Builtin(builtin)) => self.check_builtin(cx, builtin, name, args, expected, span),
            Some(PreludeItem::Move) => self.check_move(cx, args, span),
            Some(PreludeItem::Conversion(target)) => {
                if !self.arity(name, args, 1, 1, span) {
                    return expr(ExprKind::Nothing, target, span);
                }
                let value = self.check_expr(cx, &args[0], None);
                self.require_traits(&value.ty, Traits::NUMERIC, args[0].span, "only numbers can be converted");
                let args = vec![Arg { expr: value, mode: ParamMode::Read }];
                expr(ExprKind::Builtin { builtin: Builtin::Convert, args }, target, span)
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

    fn arity(&mut self, name: &ast::Ident, args: &[ast::Expr], min: usize, max: usize, span: Span) -> bool {
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
                format!("`{}` takes {expected}, but {} {} given", name.name, args.len(), if args.len() == 1 { "was" } else { "were" }),
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
        let explicit: Vec<Type> = generic_args.iter().map(|g| self.resolve_type(g, &cx.generics)).collect();
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
        expr(ExprKind::Call { function: id, type_args, args: checked }, ret, span)
    }

    fn check_move(&mut self, cx: &mut FnCtx, args: &[ast::Expr], span: Span) -> hir::Expr {
        let name = ast::Ident { name: "move".to_string(), span };
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
            ExprKind::Narrow(inner) => &mut inner.kind,
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
            Builtin::Push | Builtin::HasKey | Builtin::WriteFile => (2, 2),
            _ => (1, 1),
        };
        if !self.arity(name, args, min, max, span) {
            for arg in args {
                self.check_expr(cx, arg, None);
            }
            return expr(ExprKind::Nothing, Type::Unknown, span);
        }
        let read = |expr: hir::Expr| Arg { expr, mode: ParamMode::Read };
        let (checked, ty): (Vec<Arg>, Type) = match builtin {
            Builtin::Print => {
                let values = args.iter().map(|a| read(self.check_expr(cx, a, None))).collect();
                (values, Type::Nothing)
            }
            Builtin::Length => {
                let value = self.check_expr(cx, &args[0], None);
                self.require_traits(&value.ty, Traits::LENGTH, args[0].span, "`length` needs a string, array, or dictionary");
                (vec![read(value)], Type::INTEGER)
            }
            Builtin::ByteLength => {
                let value = self.check_expr_as(cx, &args[0], &Type::String, "`byte_length` needs a string");
                (vec![read(value)], Type::INTEGER)
            }
            Builtin::TypeOf => (vec![read(self.check_expr(cx, &args[0], None))], Type::String),
            Builtin::ToString => (vec![read(self.check_expr(cx, &args[0], None))], Type::String),
            Builtin::Assert => {
                let mut values = vec![read(self.check_expr_as(cx, &args[0], &Type::Boolean, "assertions test a boolean"))];
                if let Some(message) = args.get(1) {
                    values.push(read(self.check_expr_as(cx, message, &Type::String, "assertion message")));
                }
                (values, Type::Nothing)
            }
            Builtin::Range => {
                let values = args
                    .iter()
                    .map(|a| read(self.check_expr_as(cx, a, &Type::INTEGER, "range bounds are integers")))
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
                let array = self.check_expr_as(cx, &args[0], &Type::array(element.clone()), "`push` adds to an array");
                let value = self.check_expr_as(cx, &args[1], &element, "pushed value");
                (vec![Arg { expr: array, mode: ParamMode::Update }, Arg { expr: value, mode: ParamMode::Own }], Type::Nothing)
            }
            Builtin::Pop => {
                let element = self.fresh_var(span);
                let array = self.check_expr_as(cx, &args[0], &Type::array(element.clone()), "`pop` removes from an array");
                (vec![Arg { expr: array, mode: ParamMode::Update }], Type::union([element, Type::Nothing]))
            }
            Builtin::HasKey => {
                let value = self.fresh_var(span);
                let dict = self.check_expr_as(cx, &args[0], &Type::dict(Type::String, value), "`haskey` checks a dictionary");
                let key = self.check_expr_as(cx, &args[1], &Type::String, "dictionary key");
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
                let ty = value.ty.clone();
                (vec![read(value)], ty)
            }
            Builtin::Error => {
                let message = self.check_expr_as(cx, &args[0], &Type::String, "error message");
                (vec![read(message)], expected.cloned().unwrap_or(Type::Nothing))
            }
            Builtin::Raise => {
                let problem = self.check_expr_as(cx, &args[0], &Type::Error, "`raise` takes an Error");
                (vec![Arg { expr: problem, mode: ParamMode::Own }], expected.cloned().unwrap_or(Type::Nothing))
            }
            Builtin::Convert => unreachable!("conversions are handled separately"),
        };
        expr(ExprKind::Builtin { builtin, args: checked }, ty, span)
    }

    pub fn constant_type(&mut self, id: hir::ConstId) -> Type {
        self.constants[id.0 as usize].ty.clone().unwrap_or(Type::Unknown)
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
            current[j] = (previous[j] + 1).min(current[j - 1] + 1).min(previous[j - 1] + cost);
        }
        previous = current;
    }
    previous[b.len()]
}
