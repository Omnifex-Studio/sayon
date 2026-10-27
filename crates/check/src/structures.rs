//! Structures and enums in expressions: construction, field access, and
//! variant values.

use crate::checker::{Checker, FnCtx, Item, OUTCOME};
use crate::hir::{self, ExprKind};
use crate::types::{Type, TypeId};
use sayon_syntax::ast;
use sayon_syntax::{Diagnostic, Span};
use std::collections::HashSet;

fn expr(kind: ExprKind, ty: Type, span: Span) -> hir::Expr {
    hir::Expr { kind, ty, span }
}

fn unknown(span: Span) -> hir::Expr {
    expr(ExprKind::Nothing, Type::Unknown, span)
}

impl<'a> Checker<'a> {
    /// The declared type an expression names, such as `Point` or
    /// `geometry.Point`, unless a local variable hides it.
    pub(crate) fn type_reference(&mut self, cx: &FnCtx, target: &ast::Expr) -> Option<TypeId> {
        match &target.kind {
            ast::ExprKind::Name(name) if !self.is_local_name(cx, &name.name) => {
                match self.item(cx.module, &name.name) {
                    Some(Item::Type(id)) => {
                        self.note(name.span, crate::index::Noted::Item(Item::Type(id)));
                        Some(id)
                    }
                    Some(_) => None,
                    None => (name.name == "Outcome").then_some(OUTCOME),
                }
            }
            ast::ExprKind::Member {
                target: alias,
                name,
            } => {
                let ast::ExprKind::Name(alias) = &alias.kind else {
                    return None;
                };
                if self.is_local_name(cx, &alias.name) {
                    return None;
                }
                let module = self.import(cx.module, &alias.name)?;
                self.note_alias(cx.module, alias);
                let Some(Item::Type(id)) = self.item(module, &name.name) else {
                    return None;
                };
                // Reports a private type; the reference still resolves.
                self.module_member(cx.module, module, name);
                Some(id)
            }
            _ => None,
        }
    }

    /// Reports a declared type used where a value is needed.
    pub(crate) fn type_as_value(&mut self, id: TypeId, span: Span) -> hir::Expr {
        let def = self.type_def(id);
        let name = def.name.clone();
        let diagnostic = if def.is_enum() {
            let example = def
                .variants()
                .first()
                .map(|v| v.name.clone())
                .unwrap_or_default();
            Diagnostic::error("E_TYPE_VALUE", format!("`{name}` is an enum, not a value"))
                .primary(span, "")
                .help(format!(
                    "use one of its variants, such as `{name}.{example}`"
                ))
        } else {
            let fields: Vec<String> = def
                .fields()
                .iter()
                .map(|f| format!("{} = ...", f.name))
                .collect();
            Diagnostic::error(
                "E_TYPE_VALUE",
                format!("`{name}` is a structure type, not a value"),
            )
            .primary(span, "")
            .help(format!(
                "build a value with `{name}({})`",
                fields.join(", ")
            ))
        };
        self.error(diagnostic);
        unknown(span)
    }

    /// `Point(x = 1.0, y = 2.0)`.
    pub(crate) fn check_construct(
        &mut self,
        cx: &mut FnCtx,
        callee: &ast::Expr,
        generic_args: &[ast::TypeExpr],
        fields: &[(ast::Ident, ast::Expr)],
        expected: Option<&Type>,
        span: Span,
    ) -> hir::Expr {
        let Some(id) = self.type_reference(cx, callee) else {
            if let ast::ExprKind::Name(name) = &callee.kind
                && !self.is_local_name(cx, &name.name)
                && self.item(cx.module, &name.name).is_none()
                && self.variant_for_name(cx, &name.name, expected).is_none()
            {
                self.undefined_name(cx, name);
            } else {
                self.error(
                    Diagnostic::error(
                        "E_NAMED_ARGUMENTS",
                        "only structures are built with named fields",
                    )
                    .primary(callee.span, "not a structure type")
                    .help("pass the arguments by position, without names"),
                );
            }
            for (_, value) in fields {
                self.check_expr(cx, value, None);
            }
            return unknown(span);
        };
        if self.type_def(id).is_enum() {
            let name = self.type_def(id).name.clone();
            self.error(
                Diagnostic::error(
                    "E_NAMED_ARGUMENTS",
                    format!("`{name}` is an enum; build one of its variants instead"),
                )
                .primary(callee.span, "")
                .help(format!(
                    "for example `{name}.{}(...)`, with values by position",
                    self.type_def(id)
                        .variants()
                        .first()
                        .map(|v| v.name.as_str())
                        .unwrap_or("variant")
                )),
            );
            for (_, value) in fields {
                self.check_expr(cx, value, None);
            }
            return unknown(span);
        }
        for (field, _) in fields {
            if let Some(index) = self.type_def(id).field_index(&field.name) {
                self.note(field.span, crate::index::Noted::Field(id, index));
            }
        }
        let ty = if !generic_args.is_empty() {
            let count = self.type_def(id).generics as usize;
            if generic_args.len() != count {
                let name = self.type_def(id).name.clone();
                self.error(
                    Diagnostic::error(
                        "E_TYPE_ARGUMENTS",
                        format!(
                            "`{name}` takes {count} type argument{}, found {}",
                            if count == 1 { "" } else { "s" },
                            generic_args.len()
                        ),
                    )
                    .primary(callee.span, ""),
                );
                self.instantiate_named(id, span)
            } else {
                let generics = cx.generics.clone();
                let args = generic_args
                    .iter()
                    .map(|arg| self.resolve_type(cx.module, arg, &generics))
                    .collect();
                self.named_type(id, args)
            }
        } else {
            self.expected_named(expected, id)
                .unwrap_or_else(|| self.instantiate_named(id, span))
        };
        self.build_record(cx, ty, fields, span)
    }

    /// `[x = 1.0, y = 2.0]`, whose structure comes from the expected type or,
    /// failing that, from the one structure of the module with exactly these
    /// fields.
    pub(crate) fn check_record(
        &mut self,
        cx: &mut FnCtx,
        fields: &[(ast::Ident, ast::Expr)],
        expected: Option<&Type>,
        span: Span,
    ) -> hir::Expr {
        let target = expected.map(|ty| self.unifier.resolve(ty));
        // Where a dictionary is expected, `[north = 0]` has keys named by
        // variables, constants, or variants.
        if let Some(Type::Dict(..)) = &target {
            let entries: Vec<(ast::Expr, ast::Expr)> = fields
                .iter()
                .map(|(name, value)| {
                    let key = ast::Expr {
                        id: ast::ExprId(u32::MAX),
                        kind: ast::ExprKind::Name(name.clone()),
                        span: name.span,
                    };
                    (key, value.clone())
                })
                .collect();
            return self.check_dict(cx, &entries, expected, span);
        }
        let structures: Vec<Type> = match &target {
            Some(Type::Named(named)) if !self.type_def(named.id).is_enum() => {
                vec![target.clone().expect("target")]
            }
            Some(Type::Union(members)) => members
                .iter()
                .filter(|m| matches!(m, Type::Named(n) if !self.type_def(n.id).is_enum()))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        if let [structure] = structures.as_slice() {
            return self.build_record(cx, structure.clone(), fields, span);
        }
        let names: HashSet<&str> = fields.iter().map(|(name, _)| name.name.as_str()).collect();
        let matching: Vec<TypeId> = (0..self.types.len())
            .map(|i| TypeId(i as u32))
            .filter(|id| {
                self.types[id.0 as usize].module == cx.module && {
                    let def = self.type_def(*id);
                    !def.is_enum()
                        && def.fields().len() == names.len()
                        && def.fields().iter().all(|f| names.contains(f.name.as_str()))
                }
            })
            .collect();
        if structures.is_empty()
            && !matches!(target, Some(ref t) if !matches!(t, Type::Var(_)))
            && let [id] = matching.as_slice()
        {
            let ty = self.instantiate_named(*id, span);
            return self.build_record(cx, ty, fields, span);
        }
        let mut diagnostic = Diagnostic::error(
            "E_RECORD_TYPE",
            "cannot tell which structure this field list builds",
        )
        .primary(span, "");
        diagnostic = match (&target, matching.len()) {
            (Some(ty), _) if !matches!(ty, Type::Var(_)) => {
                diagnostic.note(format!("a {ty} is expected here, which is not a structure"))
            }
            (_, 0) => diagnostic.help(
                "name the structure, for example `Point(x = 1.0, y = 2.0)`, or annotate the variable",
            ),
            _ => diagnostic
                .help("several structures have these fields; name the one you mean, for example `Point(x = 1.0, y = 2.0)`"),
        };
        self.error(diagnostic);
        for (_, value) in fields {
            self.check_expr(cx, value, None);
        }
        unknown(span)
    }

    /// The expected type, when it is (or is a union with exactly one member
    /// that is) the named type `id`.
    fn expected_named(&self, expected: Option<&Type>, id: TypeId) -> Option<Type> {
        let resolved = self.unifier.resolve(expected?);
        let found: Vec<&Type> = resolved
            .union_members()
            .iter()
            .filter(|m| matches!(m, Type::Named(n) if n.id == id))
            .collect();
        match found.as_slice() {
            [one] => Some((*one).clone()),
            _ => None,
        }
    }

    fn build_record(
        &mut self,
        cx: &mut FnCtx,
        ty: Type,
        fields: &[(ast::Ident, ast::Expr)],
        span: Span,
    ) -> hir::Expr {
        let named = ty.as_named().expect("structure type").clone();
        let def = self.type_def(named.id).clone();
        let mut given: Vec<Option<Span>> = vec![None; def.fields().len()];
        let mut values = Vec::new();
        for (name, value) in fields {
            let Some(index) = def.field_index(&name.name) else {
                let names: Vec<String> = def
                    .fields()
                    .iter()
                    .map(|f| format!("`{}`", f.name))
                    .collect();
                self.error(
                    Diagnostic::error(
                        "E_UNKNOWN_FIELD",
                        format!("`{}` has no field `{}`", def.name, name.name),
                    )
                    .primary(name.span, "")
                    .help(format!("its fields are {}", names.join(", "))),
                );
                self.check_expr(cx, value, None);
                continue;
            };
            if let Some(first) = given[index] {
                self.error(
                    Diagnostic::error(
                        "E_DUPLICATE_FIELD",
                        format!("the field `{}` is given twice", name.name),
                    )
                    .primary(name.span, "")
                    .secondary(first, "first given here"),
                );
                self.check_expr(cx, value, None);
                continue;
            }
            given[index] = Some(name.span);
            let field_ty = def.fields()[index].ty.substitute(&named.args);
            let context = format!("the field `{}` of `{}`", name.name, def.name);
            let checked = self.check_expr_as(cx, value, &field_ty, &context);
            values.push((index as u32, checked));
        }
        let missing: Vec<String> = def
            .fields()
            .iter()
            .zip(&given)
            .filter(|(_, given)| given.is_none())
            .map(|(field, _)| format!("`{}`", field.name))
            .collect();
        if !missing.is_empty() {
            let (noun, list) = if missing.len() == 1 {
                ("a value", missing[0].clone())
            } else {
                ("values", missing.join(", "))
            };
            self.error(
                Diagnostic::error(
                    "E_MISSING_FIELD",
                    format!("`{}` needs {noun} for {list}", def.name),
                )
                .primary(span, "")
                .help("every field must be given a value"),
            );
        }
        expr(ExprKind::Record(values), ty, span)
    }

    /// `value.name` on a structure (or a value whose type is not known yet).
    pub(crate) fn check_field(
        &mut self,
        cx: &mut FnCtx,
        value: hir::Expr,
        name: &ast::Ident,
        span: Span,
    ) -> hir::Expr {
        let ty = self.field_type(&value.ty, name, span, cx.module);
        if ty == Type::Unknown {
            return unknown(span);
        }
        expr(
            ExprKind::Field {
                value: Box::new(value),
                name: name.name.clone(),
            },
            ty,
            span,
        )
    }

    /// Enums of `module` that have a variant called `name`; failing those,
    /// the prelude's `Outcome` when it has one (`success` and `failure`).
    pub(crate) fn enums_with_variant(
        &self,
        module: crate::ModuleId,
        name: &str,
    ) -> Vec<(TypeId, u32)> {
        let found: Vec<(TypeId, u32)> = (0..self.types.len())
            .map(|i| TypeId(i as u32))
            .filter(|id| self.types[id.0 as usize].module == module)
            .filter_map(|id| {
                self.type_def(id)
                    .variant_index(name)
                    .map(|index| (id, index as u32))
            })
            .collect();
        if !found.is_empty() {
            return found;
        }
        self.type_def(OUTCOME)
            .variant_index(name)
            .map(|index| (OUTCOME, index as u32))
            .into_iter()
            .collect()
    }

    /// The enum type and variant an unqualified name refers to: a variant of
    /// the expected enum, or the only variant of that name in the module.
    pub(crate) fn variant_for_name(
        &mut self,
        cx: &FnCtx,
        name: &str,
        expected: Option<&Type>,
    ) -> Option<(Option<Type>, TypeId, u32)> {
        if let Some((ty, variant)) = self.expected_variant(expected, name) {
            let id = ty.as_named().expect("enum").id;
            return Some((Some(ty), id, variant));
        }
        match self.enums_with_variant(cx.module, name).as_slice() {
            [(id, variant)] => Some((None, *id, *variant)),
            _ => None,
        }
    }

    /// A variant of the expected enum (or of the one enum member of an
    /// expected union that has it).
    pub(crate) fn expected_variant(
        &self,
        expected: Option<&Type>,
        name: &str,
    ) -> Option<(Type, u32)> {
        let resolved = self.unifier.resolve(expected?);
        let found: Vec<(Type, u32)> = resolved
            .union_members()
            .iter()
            .filter_map(|member| match member {
                Type::Named(named) => self
                    .type_def(named.id)
                    .variant_index(name)
                    .map(|index| (member.clone(), index as u32)),
                _ => None,
            })
            .collect();
        match found.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }

    /// Several enums of the module share a variant name.
    pub(crate) fn ambiguous_variant(&mut self, cx: &FnCtx, name: &ast::Ident) -> bool {
        let candidates = self.enums_with_variant(cx.module, &name.name);
        if candidates.len() < 2 {
            return false;
        }
        let names: Vec<String> = candidates
            .iter()
            .map(|(id, _)| format!("`{}.{}`", self.type_def(*id).name, name.name))
            .collect();
        self.error(
            Diagnostic::error(
                "E_AMBIGUOUS_VARIANT",
                format!("several enums have a variant `{}`", name.name),
            )
            .primary(name.span, "")
            .help(format!("name the enum: {}", names.join(" or "))),
        );
        true
    }

    /// A variant value: `point`, `circle(5.0)`, or `Shape.circle(5.0)`.
    /// `args` is `None` when the variant is used without parentheses.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn check_variant(
        &mut self,
        cx: &mut FnCtx,
        id: TypeId,
        variant: u32,
        known: Option<Type>,
        name: &ast::Ident,
        args: Option<&[ast::Expr]>,
        expected: Option<&Type>,
        span: Span,
    ) -> hir::Expr {
        self.note(name.span, crate::index::Noted::Variant(id, variant));
        let ty = known
            .or_else(|| self.expected_named(expected, id))
            .unwrap_or_else(|| self.instantiate_named(id, span));
        let named = ty.as_named().expect("enum type").clone();
        let def = self.type_def(id).clone();
        let fields = &def.variants()[variant as usize].fields;
        let field_names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        let Some(args) = args else {
            if !fields.is_empty() {
                self.error(
                    Diagnostic::error(
                        "E_VARIANT_FIELDS",
                        format!("`{}` needs values for its fields", name.name),
                    )
                    .primary(span, "")
                    .help(format!(
                        "write `{}({})`",
                        name.name,
                        field_names.join(", ")
                    )),
                );
                return unknown(span);
            }
            return expr(
                ExprKind::Variant {
                    variant,
                    args: Vec::new(),
                },
                ty,
                span,
            );
        };
        if fields.is_empty() {
            self.error(
                Diagnostic::error("E_VARIANT_FIELDS", format!("`{}` has no fields", name.name))
                    .primary(span, "")
                    .help(format!("write `{}` without parentheses", name.name)),
            );
            for arg in args {
                self.check_expr(cx, arg, None);
            }
            return unknown(span);
        }
        if args.len() != fields.len() {
            self.error(
                Diagnostic::error(
                    "E_ARGUMENT_COUNT",
                    format!(
                        "`{}` takes {} value{} ({}), but {} {} given",
                        name.name,
                        fields.len(),
                        if fields.len() == 1 { "" } else { "s" },
                        field_names.join(", "),
                        args.len(),
                        if args.len() == 1 { "was" } else { "were" }
                    ),
                )
                .primary(span, ""),
            );
            for arg in args {
                self.check_expr(cx, arg, None);
            }
            return unknown(span);
        }
        let values = args
            .iter()
            .zip(fields)
            .map(|(arg, field)| {
                let field_ty = field.ty.substitute(&named.args);
                let context = format!("the field `{}` of `{}`", field.name, name.name);
                self.check_expr_as(cx, arg, &field_ty, &context)
            })
            .collect();
        expr(
            ExprKind::Variant {
                variant,
                args: values,
            },
            ty,
            span,
        )
    }

    /// `Shape.circle` or `Shape.circle(...)` on an enum type reference.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn qualified_variant(
        &mut self,
        cx: &mut FnCtx,
        id: TypeId,
        name: &ast::Ident,
        args: Option<&[ast::Expr]>,
        expected: Option<&Type>,
        span: Span,
    ) -> hir::Expr {
        let def = self.type_def(id);
        if !def.is_enum() {
            let type_name = def.name.clone();
            self.error(
                Diagnostic::error(
                    "E_UNKNOWN_MEMBER",
                    format!("the structure `{type_name}` has no member `{}`", name.name),
                )
                .primary(name.span, "")
                .help(format!(
                    "fields belong to values: build one with `{type_name}(...)` first"
                )),
            );
            for arg in args.unwrap_or_default() {
                self.check_expr(cx, arg, None);
            }
            return unknown(span);
        }
        let Some(variant) = def.variant_index(&name.name) else {
            let type_name = def.name.clone();
            let names: Vec<String> = def
                .variants()
                .iter()
                .map(|v| format!("`{}`", v.name))
                .collect();
            self.error(
                Diagnostic::error(
                    "E_UNKNOWN_VARIANT",
                    format!("`{type_name}` has no variant `{}`", name.name),
                )
                .primary(name.span, "")
                .help(format!("its variants are {}", names.join(", "))),
            );
            for arg in args.unwrap_or_default() {
                self.check_expr(cx, arg, None);
            }
            return unknown(span);
        };
        self.check_variant(cx, id, variant as u32, None, name, args, expected, span)
    }
}
