//! `match`: checking patterns, and working out which values the arms cover
//! so that a match handles every value and every arm can be reached.
//!
//! Coverage uses the usual pattern-matrix method: a pattern is *useful*
//! after some rows when at least one value matches it and none of the rows.
//! A match is exhaustive when a wildcard is no longer useful after its
//! unguarded arms; an arm is unreachable when its pattern is not useful
//! after the unguarded arms above it.

use crate::checker::{Checker, FnCtx, Scope};
use crate::hir::{self, ExprKind, LocalKind, Pattern, StmtKind, Usage};
use crate::infer::LiteralKind;
use crate::types::{Type, variant_fields};
use sayon_syntax::ast::{self, PatternKind};
use sayon_syntax::{Diagnostic, Span};

/// The values a pattern matches, for coverage analysis.
#[derive(Clone, Debug)]
enum Space {
    Any,
    Ctor(Ctor, Vec<Space>),
}

/// One way a value can be built.
#[derive(Clone, Debug, PartialEq)]
enum Ctor {
    Variant(u32),
    /// A union holding this member type.
    Member(Type),
    Bool(bool),
    Nothing,
    /// A number or string; there are always more of these.
    Literal(String),
}

type Row = Vec<Space>;

fn specialize(rows: &[Row], ctor: &Ctor, arity: usize) -> Vec<Row> {
    rows.iter()
        .filter_map(|row| {
            let (head, rest) = row.split_first()?;
            let mut out = match head {
                Space::Ctor(c, args) if c == ctor => args.clone(),
                Space::Ctor(..) => return None,
                Space::Any => vec![Space::Any; arity],
            };
            out.extend_from_slice(rest);
            Some(out)
        })
        .collect()
}

fn default_rows(rows: &[Row]) -> Vec<Row> {
    rows.iter()
        .filter_map(|row| match row.split_first()? {
            (Space::Any, rest) => Some(rest.to_vec()),
            _ => None,
        })
        .collect()
}

fn head_ctors(rows: &[Row]) -> Vec<Ctor> {
    let mut out: Vec<Ctor> = Vec::new();
    for row in rows {
        if let Some(Space::Ctor(c, _)) = row.first()
            && !out.contains(c)
        {
            out.push(c.clone());
        }
    }
    out
}

/// Whether the expression names storage the match can read in place.
fn is_place(value: &hir::Expr) -> bool {
    match &value.kind {
        ExprKind::Local { .. } | ExprKind::Constant(_) => true,
        ExprKind::Narrow(inner) => matches!(inner.kind, ExprKind::Local { .. }),
        ExprKind::Index { target, .. } | ExprKind::Field { value: target, .. } => is_place(target),
        _ => false,
    }
}

/// `move(value)`.
fn is_moved(value: &hir::Expr) -> bool {
    match &value.kind {
        ExprKind::Local { usage, .. } => *usage == Usage::Move,
        ExprKind::Narrow(inner) => is_moved(inner),
        _ => false,
    }
}

impl<'a> Checker<'a> {
    pub(crate) fn check_match(
        &mut self,
        cx: &mut FnCtx,
        scrutinee: &ast::Expr,
        arms: &[ast::MatchArm],
    ) -> StmtKind {
        let value = self.check_expr(cx, scrutinee, None);
        let owned = !is_place(&value) || is_moved(&value);
        let ty = value.ty.clone();
        let mut rows: Vec<Row> = Vec::new();
        let mut checked = Vec::new();
        let mut invalid = false;
        for arm in arms {
            cx.scopes.push(Scope::new());
            let mut names = Vec::new();
            let errors_before = self.diagnostics.error_count();
            let pattern = self.check_pattern(cx, &arm.pattern, &ty, Some(&rows), &mut names);
            // A pattern with errors says nothing reliable about coverage.
            let pattern_failed = self.diagnostics.error_count() > errors_before;
            invalid |= pattern_failed;
            let guard = arm.guard.as_ref().map(|guard| {
                self.check_expr_as(cx, guard, &Type::Boolean, "a `case` guard is a boolean")
            });
            let body = self.check_block(cx, &arm.body);
            cx.scopes.pop();

            if pattern_failed {
                checked.push(hir::MatchArm {
                    pattern,
                    guard,
                    body,
                });
                continue;
            }
            let space = self.space(&pattern);
            let resolved = self.unifier.resolve(&ty);
            if !self.useful(&rows, std::slice::from_ref(&space), &[resolved]) {
                self.error(
                    Diagnostic::warning("W_UNREACHABLE_CASE", "this case is never reached")
                        .primary(arm.pattern.span, "")
                        .note("the cases above it already match every value it would"),
                );
            }
            if guard.is_none() {
                rows.push(vec![space]);
            }
            checked.push(hir::MatchArm {
                pattern,
                guard,
                body,
            });
        }
        let resolved = self.unifier.resolve(&ty);
        let missing = if invalid {
            Vec::new()
        } else {
            self.missing_cases(&rows, &resolved)
        };
        if !missing.is_empty() {
            let shown: Vec<String> = missing.iter().take(4).map(|m| format!("`{m}`")).collect();
            let more = if missing.len() > 4 {
                format!(" and {} more", missing.len() - 4)
            } else {
                String::new()
            };
            let guarded = arms.iter().any(|arm| arm.guard.is_some());
            let mut diagnostic = Diagnostic::error(
                "E_NONEXHAUSTIVE",
                "`match` does not handle every possible value",
            )
            .primary(
                scrutinee.span,
                format!("not handled: {}{more}", shown.join(", ")),
            )
            .help("add a `case` for each, or end with `case _` to handle everything else");
            if guarded {
                diagnostic = diagnostic
                    .note("cases with an `if` guard do not count, since the guard can fail");
            }
            self.error(diagnostic);
        }
        StmtKind::Match {
            scrutinee: value,
            owned,
            arms: checked,
        }
    }

    /// Checks a pattern against the type it matches, declaring its bindings.
    /// `prior` holds the unguarded arms above, for a pattern at the top of an
    /// arm: a binding there takes only the union members still possible.
    fn check_pattern(
        &mut self,
        cx: &mut FnCtx,
        pattern: &ast::Pattern,
        ty: &Type,
        prior: Option<&[Row]>,
        names: &mut Vec<(String, Span)>,
    ) -> Pattern {
        match &pattern.kind {
            PatternKind::Wildcard => Pattern::Wildcard,
            PatternKind::Name(name) => {
                if let Some((enum_ty, member, variant)) =
                    self.pattern_variant(cx, ty, &name.name, name.span)
                {
                    let fields = self.variant_field_types(&enum_ty, variant);
                    if !fields.is_empty() {
                        let blanks = vec!["_"; fields.len()].join(", ");
                        self.error(
                            Diagnostic::error(
                                "E_PATTERN_ARITY",
                                format!(
                                    "`{}` has {} field{}",
                                    name.name,
                                    fields.len(),
                                    if fields.len() == 1 { "" } else { "s" }
                                ),
                            )
                            .primary(name.span, "")
                            .help(format!(
                                "write `{}({blanks})` to match any values",
                                name.name
                            )),
                        );
                        return Pattern::Wildcard;
                    }
                    let variant = Pattern::Variant {
                        ty: enum_ty,
                        variant,
                        fields: Vec::new(),
                    };
                    return wrap(member, variant);
                }
                if let Some((_, first)) = names.iter().find(|(n, _)| *n == name.name) {
                    let first = *first;
                    self.error(
                        Diagnostic::error(
                            "E_DUPLICATE_DECLARATION",
                            format!("`{}` is bound twice in one pattern", name.name),
                        )
                        .primary(name.span, "")
                        .secondary(first, "first bound here"),
                    );
                    return Pattern::Wildcard;
                }
                names.push((name.name.clone(), name.span));
                let binding_ty = match prior {
                    Some(rows) => self.remaining_type(ty, rows),
                    None => ty.clone(),
                };
                let local = cx.declare(&name.name, binding_ty, name.span, LocalKind::CaseBinding);
                Pattern::Binding(local)
            }
            PatternKind::Variant { name, args } => {
                let Some((enum_ty, member, variant)) =
                    self.pattern_variant(cx, ty, &name.name, name.span)
                else {
                    let resolved = self.unifier.resolve(ty);
                    let message = match &resolved {
                        Type::Unknown => None,
                        Type::Var(_) => {
                            Some(format!("`{}` is not a variant of any enum here", name.name))
                        }
                        other => Some(format!("{other} has no variant `{}`", name.name)),
                    };
                    if let Some(message) = message {
                        self.error(
                            Diagnostic::error("E_UNKNOWN_VARIANT", message).primary(name.span, ""),
                        );
                    }
                    for arg in args {
                        self.check_pattern(cx, arg, &Type::Unknown, None, names);
                    }
                    return Pattern::Wildcard;
                };
                let field_types = self.variant_field_types(&enum_ty, variant);
                if args.len() != field_types.len() {
                    let fields: Vec<String> = self
                        .type_def(enum_ty.as_named().expect("enum").id)
                        .variants()[variant as usize]
                        .fields
                        .iter()
                        .map(|f| f.name.clone())
                        .collect();
                    let message = if fields.is_empty() {
                        format!("`{}` has no fields", name.name)
                    } else {
                        format!(
                            "`{}` has {} field{} ({}), but the pattern gives {}",
                            name.name,
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" },
                            fields.join(", "),
                            args.len()
                        )
                    };
                    self.error(
                        Diagnostic::error("E_PATTERN_ARITY", message).primary(pattern.span, ""),
                    );
                }
                let mut fields = Vec::new();
                for (index, field_ty) in field_types.iter().enumerate() {
                    fields.push(match args.get(index) {
                        Some(arg) => self.check_pattern(cx, arg, field_ty, None, names),
                        None => Pattern::Wildcard,
                    });
                }
                for arg in args.iter().skip(field_types.len()) {
                    self.check_pattern(cx, arg, &Type::Unknown, None, names);
                }
                wrap(
                    member,
                    Pattern::Variant {
                        ty: enum_ty,
                        variant,
                        fields,
                    },
                )
            }
            PatternKind::Literal(literal) => self.literal_pattern(cx, literal, ty),
        }
    }

    fn literal_pattern(&mut self, cx: &mut FnCtx, literal: &ast::Expr, ty: &Type) -> Pattern {
        let value = self.check_expr(cx, literal, None);
        if !matches!(
            value.kind,
            ExprKind::Integer(_)
                | ExprKind::Float(_)
                | ExprKind::String(_)
                | ExprKind::Boolean(_)
                | ExprKind::Nothing
        ) {
            self.error(
                Diagnostic::error(
                    "E_PATTERN_LITERAL",
                    "a pattern can only compare with a fixed value",
                )
                .primary(literal.span, "this value is computed")
                .help("bind it to a name and compare in a guard: `case text if text == expected`"),
            );
            return Pattern::Wildcard;
        }
        let resolved = self.unifier.resolve(ty);
        let Type::Union(members) = &resolved else {
            self.unify_at(ty, &value.ty, literal.span, "this pattern's value");
            return Pattern::Literal(value);
        };
        if matches!(value.kind, ExprKind::Nothing) {
            if members.contains(&Type::Nothing) {
                return Pattern::Member {
                    member: Type::Nothing,
                    inner: Box::new(Pattern::Wildcard),
                };
            }
            self.error(
                Diagnostic::error("E_TYPE_MISMATCH", format!("{resolved} is never nothing"))
                    .primary(literal.span, ""),
            );
            return Pattern::Wildcard;
        }
        let found = self.unifier.shallow(&value.ty);
        let member = match found {
            Type::Var(var) => {
                let kind = self.unifier.bounds(var).literal;
                let candidates: Vec<&Type> = members
                    .iter()
                    .filter(|member| match (kind, member) {
                        (Some(LiteralKind::Integer), Type::Int(_))
                        | (Some(LiteralKind::Float), Type::Float(_)) => true,
                        (Some(LiteralKind::Unsigned), Type::Int(int)) => !int.signed(),
                        _ => false,
                    })
                    .collect();
                match (candidates.as_slice(), kind) {
                    ([one], _) => Some((*one).clone()),
                    (many, Some(kind)) if many.contains(&&kind.default_type()) => {
                        Some(kind.default_type())
                    }
                    _ => None,
                }
            }
            concrete => members.iter().find(|m| **m == concrete).cloned(),
        };
        let Some(member) = member else {
            let shown = self.unifier.resolve_for_display(&value.ty);
            self.error(
                Diagnostic::error(
                    "E_TYPE_MISMATCH",
                    format!("{resolved} can never hold {shown}"),
                )
                .primary(literal.span, ""),
            );
            return Pattern::Wildcard;
        };
        self.unify_at(&member, &value.ty, literal.span, "this pattern's value");
        Pattern::Member {
            member,
            inner: Box::new(Pattern::Literal(value)),
        }
    }

    /// The enum and variant a pattern name refers to, given the type it
    /// matches: a variant of that enum, or of the one enum member of a union
    /// that has it (returned as the member). For a value whose type is not
    /// known yet, the only enum of the module with that variant.
    fn pattern_variant(
        &mut self,
        cx: &FnCtx,
        ty: &Type,
        name: &str,
        span: Span,
    ) -> Option<(Type, Option<Type>, u32)> {
        let resolved = self.unifier.resolve(ty);
        match &resolved {
            Type::Named(named) => self
                .type_def(named.id)
                .variant_index(name)
                .map(|index| (resolved.clone(), None, index as u32)),
            Type::Union(members) => {
                let found: Vec<(Type, u32)> = members
                    .iter()
                    .filter_map(|member| match member {
                        Type::Named(named) => self
                            .type_def(named.id)
                            .variant_index(name)
                            .map(|index| (member.clone(), index as u32)),
                        _ => None,
                    })
                    .collect();
                if found.len() > 1 {
                    self.error(
                        Diagnostic::error(
                            "E_AMBIGUOUS_VARIANT",
                            format!("several enums in {resolved} have a variant `{name}`"),
                        )
                        .primary(span, "")
                        .help("match the enum value on its own first, after checking which kind of value it is"),
                    );
                }
                found
                    .into_iter()
                    .next()
                    .map(|(member, index)| (member.clone(), Some(member), index))
            }
            Type::Var(_) => match self.enums_with_variant(cx.module, name).as_slice() {
                [(id, variant)] => {
                    let enum_ty = self.instantiate_named(*id, span);
                    self.unify_at(ty, &enum_ty, span, "the matched value");
                    Some((enum_ty, None, *variant))
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn variant_field_types(&self, enum_ty: &Type, variant: u32) -> Vec<Type> {
        match enum_ty {
            Type::Named(named) => variant_fields(&self.type_defs, named, variant)
                .iter()
                .map(|ty| self.unifier.resolve(ty))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The type a binding at the top of an arm receives: the members of a
    /// union that the arms above have not fully handled.
    fn remaining_type(&mut self, ty: &Type, rows: &[Row]) -> Type {
        let resolved = self.unifier.resolve(ty);
        let Type::Union(members) = &resolved else {
            return ty.clone();
        };
        let left: Vec<Type> = members
            .iter()
            .filter(|member| {
                let space = Space::Ctor(Ctor::Member((*member).clone()), vec![Space::Any]);
                self.useful(rows, &[space], std::slice::from_ref(&resolved))
            })
            .cloned()
            .collect();
        if left.is_empty() || left.len() == members.len() {
            ty.clone()
        } else {
            Type::union(left)
        }
    }

    // ----- coverage -----------------------------------------------------------

    fn space(&self, pattern: &Pattern) -> Space {
        match pattern {
            Pattern::Wildcard | Pattern::Binding(_) => Space::Any,
            Pattern::Literal(value) => match &value.kind {
                ExprKind::Boolean(flag) => Space::Ctor(Ctor::Bool(*flag), Vec::new()),
                ExprKind::Nothing => Space::Ctor(Ctor::Nothing, Vec::new()),
                ExprKind::Integer(number) => {
                    Space::Ctor(Ctor::Literal(format!("i{number}")), Vec::new())
                }
                ExprKind::Float(number) => {
                    Space::Ctor(Ctor::Literal(format!("f{}", number.to_bits())), Vec::new())
                }
                ExprKind::String(text) => {
                    Space::Ctor(Ctor::Literal(format!("s{text}")), Vec::new())
                }
                _ => Space::Any,
            },
            Pattern::Variant {
                variant, fields, ..
            } => Space::Ctor(
                Ctor::Variant(*variant),
                fields.iter().map(|f| self.space(f)).collect(),
            ),
            Pattern::Member { member, inner } => Space::Ctor(
                Ctor::Member(self.unifier.resolve(member)),
                vec![self.space(inner)],
            ),
        }
    }

    /// Every constructor of a type, or `None` when there are too many to list.
    fn constructors(&self, ty: &Type) -> Option<Vec<Ctor>> {
        match ty {
            Type::Named(named) if self.type_def(named.id).is_enum() => Some(
                (0..self.type_def(named.id).variants().len())
                    .map(|i| Ctor::Variant(i as u32))
                    .collect(),
            ),
            Type::Union(members) => Some(members.iter().cloned().map(Ctor::Member).collect()),
            Type::Boolean => Some(vec![Ctor::Bool(false), Ctor::Bool(true)]),
            Type::Nothing => Some(vec![Ctor::Nothing]),
            _ => None,
        }
    }

    /// The types of a constructor's parts.
    fn ctor_types(&self, ty: &Type, ctor: &Ctor) -> Vec<Type> {
        match (ctor, ty) {
            (Ctor::Variant(variant), Type::Named(_)) => self.variant_field_types(ty, *variant),
            (Ctor::Member(member), _) => vec![member.clone()],
            _ => Vec::new(),
        }
    }

    fn useful(&self, rows: &[Row], vector: &[Space], types: &[Type]) -> bool {
        let Some((head, rest)) = vector.split_first() else {
            return rows.is_empty();
        };
        let ty = types.first().cloned().unwrap_or(Type::Unknown);
        let rest_types = types.get(1..).unwrap_or_default();
        match head {
            Space::Ctor(ctor, args) => {
                let mut sub_types = self.ctor_types(&ty, ctor);
                sub_types.resize(args.len(), Type::Unknown);
                sub_types.extend_from_slice(rest_types);
                let mut next = args.clone();
                next.extend_from_slice(rest);
                self.useful(&specialize(rows, ctor, args.len()), &next, &sub_types)
            }
            Space::Any => {
                let used = head_ctors(rows);
                match self.constructors(&ty) {
                    Some(all) if all.iter().all(|c| used.contains(c)) => all.iter().any(|ctor| {
                        let mut sub_types = self.ctor_types(&ty, ctor);
                        let arity = sub_types.len();
                        sub_types.extend_from_slice(rest_types);
                        let mut next = vec![Space::Any; arity];
                        next.extend_from_slice(rest);
                        self.useful(&specialize(rows, ctor, arity), &next, &sub_types)
                    }),
                    _ => self.useful(&default_rows(rows), rest, rest_types),
                }
            }
        }
    }

    /// A combination of values that no row matches, if there is one.
    fn witness(&self, rows: &[Row], types: &[Type]) -> Option<Vec<Space>> {
        let Some(ty) = types.first() else {
            return rows.is_empty().then(Vec::new);
        };
        let rest_types = &types[1..];
        let used = head_ctors(rows);
        match self.constructors(ty) {
            Some(all) if all.iter().all(|c| used.contains(c)) => {
                for ctor in all {
                    let mut sub_types = self.ctor_types(ty, &ctor);
                    let arity = sub_types.len();
                    sub_types.extend_from_slice(rest_types);
                    if let Some(found) = self.witness(&specialize(rows, &ctor, arity), &sub_types) {
                        let (args, rest) = found.split_at(arity);
                        let mut out = vec![Space::Ctor(ctor, args.to_vec())];
                        out.extend_from_slice(rest);
                        return Some(out);
                    }
                }
                None
            }
            all => {
                let found = self.witness(&default_rows(rows), rest_types)?;
                let head = match all.and_then(|all| all.into_iter().find(|c| !used.contains(c))) {
                    Some(ctor) => {
                        let arity = self.ctor_types(ty, &ctor).len();
                        Space::Ctor(ctor, vec![Space::Any; arity])
                    }
                    None => Space::Any,
                };
                let mut out = vec![head];
                out.extend(found);
                Some(out)
            }
        }
    }

    /// Descriptions of the values the rows leave unmatched: one per missing
    /// constructor of the matched type.
    fn missing_cases(&self, rows: &[Row], ty: &Type) -> Vec<String> {
        if !self.useful(rows, &[Space::Any], std::slice::from_ref(ty)) {
            return Vec::new();
        }
        let Some(all) = self.constructors(ty) else {
            return vec!["_".to_string()];
        };
        let mut missing = Vec::new();
        for ctor in all {
            let sub_types = self.ctor_types(ty, &ctor);
            let arity = sub_types.len();
            let probe = Space::Ctor(ctor.clone(), vec![Space::Any; arity]);
            if !self.useful(rows, &[probe], std::slice::from_ref(ty)) {
                continue;
            }
            let args = self
                .witness(&specialize(rows, &ctor, arity), &sub_types)
                .unwrap_or_else(|| vec![Space::Any; arity]);
            missing.push(self.describe(&Space::Ctor(ctor, args), ty));
        }
        missing
    }

    fn describe(&self, space: &Space, ty: &Type) -> String {
        match space {
            Space::Any => "_".to_string(),
            Space::Ctor(ctor, args) => match ctor {
                Ctor::Variant(variant) => {
                    let Type::Named(named) = ty else {
                        return "_".to_string();
                    };
                    let name = &self.type_def(named.id).variants()[*variant as usize].name;
                    if args.is_empty() {
                        name.clone()
                    } else {
                        let types = self.ctor_types(ty, ctor);
                        let parts: Vec<String> = args
                            .iter()
                            .zip(types.iter().chain(std::iter::repeat(&Type::Unknown)))
                            .map(|(arg, ty)| self.describe(arg, ty))
                            .collect();
                        format!("{name}({})", parts.join(", "))
                    }
                }
                Ctor::Member(member) => match args.first() {
                    Some(inner @ Space::Ctor(..)) => self.describe(inner, member),
                    _ => member.to_string(),
                },
                Ctor::Bool(flag) => flag.to_string(),
                Ctor::Nothing => "nothing".to_string(),
                Ctor::Literal(text) => text[1..].to_string(),
            },
        }
    }
}

fn wrap(member: Option<Type>, pattern: Pattern) -> Pattern {
    match member {
        Some(member) => Pattern::Member {
            member,
            inner: Box::new(pattern),
        },
        None => pattern,
    }
}
