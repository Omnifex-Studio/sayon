//! Item collection, dependency ordering, and per-function type inference.

use crate::hir::{self, ConstId, FuncId, LocalId, LocalKind};
use crate::infer::{Bounds, LiteralKind, Traits, Unifier, UnifyError};
use crate::types::{FieldDef, NamedType, TyVar, Type, TypeDef, TypeDefKind, TypeId, VariantDef};
use sayon_syntax::ast::{self, DeclKind, ParamMode, StmtKind};
use sayon_syntax::{Diagnostic, Diagnostics, Span};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Item {
    Function(FuncId),
    Constant(ConstId),
    Type(TypeId),
}

/// The source of a structure or enum declaration.
pub(crate) enum TypeSource<'a> {
    Structure(&'a [ast::Field]),
    Enum(&'a [ast::Variant]),
}

pub(crate) struct TypeEntry<'a> {
    pub module: ModuleId,
    pub name: String,
    pub span: Span,
    pub visibility: ast::Visibility,
    pub generics: &'a [ast::Ident],
    pub source: TypeSource<'a>,
}

/// A source file being checked. Each file is a module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleId(pub u32);

pub(crate) struct ModuleInfo {
    /// The import path or file name shown in messages.
    pub name: String,
    pub items: HashMap<String, Item>,
    /// Import aliases and the modules they name.
    pub imports: HashMap<String, (ModuleId, Span)>,
    /// Standard library modules may use the host primitives.
    pub standard: bool,
    pub entry: bool,
}

/// A function signature. Inside a function's own strongly connected component
/// the types may contain inference variables; afterwards generic parameters
/// appear as `Type::Param`.
#[derive(Clone, Debug)]
pub(crate) struct Signature {
    pub params: Vec<Type>,
    pub param_names: Vec<String>,
    pub explicit_modes: Vec<Option<ParamMode>>,
    /// Number of leading parameters without a default value.
    pub required: usize,
    pub ret: Type,
    pub generic_count: u32,
    pub explicit_generics: u32,
    pub bounds: Vec<Bounds>,
}

pub(crate) struct FunctionEntry<'a> {
    pub ast: Option<&'a ast::Function>,
    pub module: ModuleId,
    pub name: String,
    pub span: Span,
    pub signature: Option<Signature>,
    pub generalized: bool,
    pub hir: Option<hir::Function>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstState {
    Unchecked,
    InProgress,
    Done,
}

pub(crate) struct ConstantEntry<'a> {
    pub decl: &'a ast::Declaration,
    pub module: ModuleId,
    pub span: Span,
    pub ty: Option<Type>,
    pub state: ConstState,
    pub hir: Option<hir::Expr>,
}

/// A constraint whose shape depends on a type that may not be known yet.
pub(crate) enum Deferred {
    /// `container[index]` producing `result`.
    Index {
        container: Type,
        index: Type,
        result: Type,
        span: Span,
    },
    /// `for item in iterable` (or `for key, value in iterable`).
    Iterate {
        iterable: Type,
        items: Vec<Type>,
        span: Span,
    },
    /// `[]` must become an array or a dictionary.
    EmptyCollection { ty: Type, span: Span },
    /// `target.name` producing `result`, in code of `module`.
    Field {
        target: Type,
        name: String,
        result: Type,
        span: Span,
        module: ModuleId,
    },
}

pub(crate) struct Scope {
    pub names: HashMap<String, LocalId>,
    pub declared: Vec<LocalId>,
    pub narrowed: HashMap<LocalId, Type>,
}

impl Scope {
    pub fn new() -> Scope {
        Scope {
            names: HashMap::new(),
            declared: Vec::new(),
            narrowed: HashMap::new(),
        }
    }
}

/// State for the function (or constant initializer) being checked.
pub(crate) struct FnCtx {
    pub module: ModuleId,
    pub func: Option<FuncId>,
    pub locals: Vec<hir::Local>,
    pub scopes: Vec<Scope>,
    pub ret: Type,
    pub loop_depth: u32,
    /// Loop depth at which the innermost `finally` began, if any.
    pub finally_loop_depth: Option<u32>,
    pub generics: HashMap<String, u32>,
    pub is_script: bool,
}

impl FnCtx {
    pub fn new(module: ModuleId, func: Option<FuncId>, ret: Type, is_script: bool) -> FnCtx {
        FnCtx {
            module,
            func,
            locals: Vec::new(),
            scopes: vec![Scope::new()],
            ret,
            loop_depth: 0,
            finally_loop_depth: None,
            generics: HashMap::new(),
            is_script,
        }
    }

    pub fn lookup(&self, name: &str) -> Option<LocalId> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.names.get(name).copied())
    }

    pub fn narrowed_type(&self, local: LocalId) -> Option<&Type> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.narrowed.get(&local))
    }

    pub fn declare(&mut self, name: &str, ty: Type, span: Span, kind: LocalKind) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(hir::Local {
            name: name.to_string(),
            ty,
            span,
            kind,
            borrowed: false,
        });
        let scope = self.scopes.last_mut().expect("scope");
        scope.names.insert(name.to_string(), id);
        scope.declared.push(id);
        id
    }

    pub fn invalidate_narrowing(&mut self, local: LocalId) {
        for scope in &mut self.scopes {
            scope.narrowed.remove(&local);
        }
    }
}

pub(crate) struct Checker<'a> {
    pub unifier: Unifier,
    pub var_spans: Vec<Span>,
    pub diagnostics: Diagnostics,
    pub modules: Vec<ModuleInfo>,
    pub entry_module: ModuleId,
    pub functions: Vec<FunctionEntry<'a>>,
    pub constants: Vec<ConstantEntry<'a>>,
    pub types: Vec<TypeEntry<'a>>,
    /// The resolved definitions of `types`, indexed by `TypeId`.
    pub type_defs: Vec<TypeDef>,
    /// Names bound by top-level script statements, for a clearer error when a
    /// function tries to use one.
    pub script_names: HashSet<String>,
    pub deferred: Vec<Deferred>,
    /// Functions in the component currently being inferred.
    pub current_component: Vec<FuncId>,
    pub reported_vars: HashSet<TyVar>,
    pub scc_order: Vec<Vec<FuncId>>,
    /// Test blocks of the entry module: module, description, span, body.
    pub test_blocks: Vec<(ModuleId, String, Span, &'a ast::Block)>,
}

pub struct CheckOutput {
    pub program: Option<hir::Program>,
    pub diagnostics: Diagnostics,
}

impl<'a> Checker<'a> {
    pub fn new() -> Checker<'a> {
        Checker {
            unifier: Unifier::new(),
            var_spans: Vec::new(),
            diagnostics: Diagnostics::new(),
            modules: Vec::new(),
            entry_module: ModuleId(0),
            functions: Vec::new(),
            constants: Vec::new(),
            types: Vec::new(),
            type_defs: Vec::new(),
            script_names: HashSet::new(),
            deferred: Vec::new(),
            current_component: Vec::new(),
            reported_vars: HashSet::new(),
            scc_order: Vec::new(),
            test_blocks: Vec::new(),
        }
    }

    // ----- diagnostics helpers ---------------------------------------------

    pub fn error(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    pub fn unavailable(&mut self, span: Span, feature: &str, milestone: &str) {
        self.error(
            Diagnostic::error(
                "E_UNAVAILABLE",
                format!(
                    "{feature} {} not available yet",
                    if feature.ends_with('s') { "are" } else { "is" }
                ),
            )
            .primary(span, format!("planned for milestone {milestone}"))
            .note(format!(
                "this release does not implement {feature}; it is scheduled for {milestone}"
            )),
        );
    }

    pub fn fresh_var(&mut self, span: Span) -> Type {
        self.fresh_bounded(Bounds::default(), span)
    }

    pub fn fresh_bounded(&mut self, bounds: Bounds, span: Span) -> Type {
        let ty = self.unifier.fresh_bounded(bounds);
        self.var_spans.push(span);
        ty
    }

    /// Unifies and reports a mismatch at `span`. Returns false on failure.
    pub fn unify_at(&mut self, expected: &Type, found: &Type, span: Span, context: &str) -> bool {
        match self.unifier.unify(expected, found) {
            Ok(()) => true,
            Err(error) => {
                self.report_unify(error, span, context);
                false
            }
        }
    }

    pub fn report_unify(&mut self, error: UnifyError, span: Span, context: &str) {
        let (code, message, help) = match &error {
            UnifyError::Mismatch(expected, found) => {
                let help = conversion_help(expected, found);
                ("E_TYPE_MISMATCH", format!("expected {expected}, found {found}"), help)
            }
            UnifyError::LiteralConflict => ("E_TYPE_MISMATCH", error.describe(), Some(
                "write the literal with a decimal point, or convert with integer(...) or float(...)".to_string(),
            )),
            UnifyError::Literal { ty, kind } => {
                let help = match (kind, ty) {
                    (LiteralKind::Integer, Type::Float(_)) => Some("write the number as a float literal, such as 2.0".to_string()),
                    (LiteralKind::Float, Type::Int(_)) => Some("convert explicitly, for example integer(2.5)".to_string()),
                    _ => None,
                };
                ("E_TYPE_MISMATCH", error.describe(), help)
            }
            UnifyError::Unsupported { ty, .. } => {
                let help = match ty {
                    Type::Union(members) if members.contains(&Type::Nothing) => Some(
                        "the value might be nothing; check first, for example `if value != nothing`".to_string(),
                    ),
                    Type::Union(_) => Some(
                        "check which kind of value it holds first, for example `if typeof(value) == \"integer\"`"
                            .to_string(),
                    ),
                    _ => None,
                };
                ("E_UNSUPPORTED_OPERATION", error.describe(), help)
            }
            UnifyError::Infinite => ("E_TYPE_MISMATCH", error.describe(), None),
        };
        let mut diagnostic = Diagnostic::error(code, message).primary(span, context.to_string());
        if let Some(help) = help {
            diagnostic = diagnostic.help(help);
        }
        self.error(diagnostic);
    }

    pub fn add_bounds_at(&mut self, ty: &Type, bounds: Bounds, span: Span, context: &str) {
        if let Err(error) = self.unifier.add_bounds(ty, bounds) {
            self.report_unify(error, span, context);
        }
    }

    pub fn require_traits(&mut self, ty: &Type, traits: Traits, span: Span, context: &str) {
        self.add_bounds_at(ty, Bounds::traits(traits), span, context);
    }

    // ----- items --------------------------------------------------------------

    pub fn item(&self, module: ModuleId, name: &str) -> Option<Item> {
        self.modules[module.0 as usize].items.get(name).copied()
    }

    pub fn import(&self, module: ModuleId, name: &str) -> Option<ModuleId> {
        self.modules[module.0 as usize]
            .imports
            .get(name)
            .map(|(target, _)| *target)
    }

    pub fn item_module(&self, item: Item) -> ModuleId {
        match item {
            Item::Function(id) => self.functions[id.0 as usize].module,
            Item::Constant(id) => self.constants[id.0 as usize].module,
            Item::Type(id) => self.types[id.0 as usize].module,
        }
    }

    fn item_span(&self, item: Item) -> Span {
        match item {
            Item::Function(id) => self.functions[id.0 as usize].span,
            Item::Constant(id) => self.constants[id.0 as usize].span,
            Item::Type(id) => self.types[id.0 as usize].span,
        }
    }

    pub fn item_is_private(&self, item: Item) -> bool {
        let visibility = match item {
            Item::Function(id) => self.functions[id.0 as usize].ast.map(|f| f.visibility),
            Item::Constant(id) => Some(self.constants[id.0 as usize].decl.visibility),
            Item::Type(id) => Some(self.types[id.0 as usize].visibility),
        };
        visibility == Some(ast::Visibility::Private)
    }

    /// Resolves `alias.member` from `module`, reporting missing and private members.
    pub fn module_member(
        &mut self,
        module: ModuleId,
        target: ModuleId,
        member: &ast::Ident,
    ) -> Option<Item> {
        let found = self.item(target, &member.name);
        let module_name = self.modules[target.0 as usize].name.clone();
        match found {
            Some(item) if self.item_is_private(item) && self.item_module(item) != module => {
                self.error(
                    Diagnostic::error(
                        "E_PRIVATE",
                        format!("`{}` is private to `{module_name}`", member.name),
                    )
                    .primary(member.span, "not exported")
                    .help("remove `private` from its definition to export it"),
                );
                None
            }
            Some(item) => Some(item),
            None => {
                self.error(
                    Diagnostic::error(
                        "E_UNKNOWN_MEMBER",
                        format!("`{module_name}` has no `{}`", member.name),
                    )
                    .primary(member.span, "not defined in that module"),
                );
                None
            }
        }
    }

    /// Collects functions and constants from a module's top level and returns
    /// the remaining script statements. Only the entry module may have script
    /// statements; imports were resolved by the loader.
    pub fn collect_items(
        &mut self,
        module_id: ModuleId,
        module: &'a ast::Module,
    ) -> Vec<&'a ast::Stmt> {
        let mut script = Vec::new();
        let entry = self.modules[module_id.0 as usize].entry;
        for stmt in &module.stmts {
            match &stmt.kind {
                StmtKind::Assign { value, .. }
                    if matches!(value.kind, ast::ExprKind::Import { .. }) => {}
                // Only the tests of the file being tested run.
                StmtKind::Test {
                    name,
                    name_span,
                    body,
                } => {
                    if entry {
                        self.test_blocks
                            .push((module_id, name.clone(), *name_span, body));
                    }
                }
                StmtKind::Function(function) => {
                    let name = function.name.as_ref().expect("named function");
                    let id = FuncId(self.functions.len() as u32);
                    if self.define_item(module_id, &name.name, name.span, Item::Function(id)) {
                        self.functions.push(FunctionEntry {
                            ast: Some(function),
                            module: module_id,
                            name: name.name.clone(),
                            span: name.span,
                            signature: None,
                            generalized: false,
                            hir: None,
                        });
                    }
                }
                StmtKind::TypeDecl {
                    visibility,
                    name,
                    generics,
                    fields,
                } => self.collect_type(
                    module_id,
                    name,
                    *visibility,
                    generics,
                    TypeSource::Structure(fields),
                ),
                StmtKind::EnumDecl {
                    visibility,
                    name,
                    generics,
                    variants,
                } => self.collect_type(
                    module_id,
                    name,
                    *visibility,
                    generics,
                    TypeSource::Enum(variants),
                ),
                StmtKind::Declare(decl) if decl.kind == DeclKind::Constant => {
                    let id = ConstId(self.constants.len() as u32);
                    if self.define_item(
                        module_id,
                        &decl.name.name,
                        decl.name.span,
                        Item::Constant(id),
                    ) {
                        self.constants.push(ConstantEntry {
                            decl,
                            module: module_id,
                            span: decl.name.span,
                            ty: None,
                            state: ConstState::Unchecked,
                            hir: None,
                        });
                    }
                }
                _ if entry => {
                    collect_script_names(stmt, &mut self.script_names);
                    script.push(stmt);
                }
                _ => {
                    let name = self.modules[module_id.0 as usize].name.clone();
                    self.error(
                        Diagnostic::error(
                            "E_MODULE_STATEMENT",
                            format!(
                                "`{name}` is imported as a module, so it can only define things"
                            ),
                        )
                        .primary(
                            stmt.span,
                            "a statement that would run when the module is imported",
                        )
                        .help("move this into a function, or declare a value with `constant`"),
                    );
                }
            }
        }
        script
    }

    fn collect_type(
        &mut self,
        module: ModuleId,
        name: &'a ast::Ident,
        visibility: ast::Visibility,
        generics: &'a [ast::Ident],
        source: TypeSource<'a>,
    ) {
        let id = TypeId(self.types.len() as u32);
        if self.define_item(module, &name.name, name.span, Item::Type(id)) {
            self.types.push(TypeEntry {
                module,
                name: name.name.clone(),
                span: name.span,
                visibility,
                generics,
                source,
            });
        }
    }

    fn define_item(&mut self, module: ModuleId, name: &str, span: Span, item: Item) -> bool {
        if let Some((_, import_span)) = self.modules[module.0 as usize].imports.get(name) {
            let import_span = *import_span;
            self.error(
                Diagnostic::error(
                    "E_DUPLICATE_DEFINITION",
                    format!("`{name}` is already an imported module"),
                )
                .primary(span, "defined here")
                .secondary(import_span, "imported here"),
            );
            return false;
        }
        if let Some(existing) = self.modules[module.0 as usize].items.get(name) {
            let previous = self.item_span(*existing);
            self.error(
                Diagnostic::error(
                    "E_DUPLICATE_DEFINITION",
                    format!("`{name}` is defined more than once"),
                )
                .primary(span, "redefined here")
                .secondary(previous, "first defined here"),
            );
            return false;
        }
        if crate::builtins::lookup(name, false).is_some() {
            self.error(
                Diagnostic::warning(
                    "W_SHADOWED_BUILTIN",
                    format!("`{name}` hides the built-in function of the same name"),
                )
                .primary(span, ""),
            );
        }
        self.modules[module.0 as usize]
            .items
            .insert(name.to_string(), item);
        true
    }

    /// Orders functions and constants so that every item is checked after the
    /// items it uses. Returns strongly connected components in dependency order.
    pub fn dependency_order(&mut self) -> Vec<Vec<Item>> {
        let mut nodes: Vec<Item> = Vec::new();
        nodes.extend((0..self.functions.len()).map(|i| Item::Function(FuncId(i as u32))));
        nodes.extend((0..self.constants.len()).map(|i| Item::Constant(ConstId(i as u32))));
        let index_of: HashMap<Item, usize> =
            nodes.iter().enumerate().map(|(i, n)| (*n, i)).collect();

        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        for (index, node) in nodes.iter().enumerate() {
            let mut names = Vec::new();
            let module = self.item_module(*node);
            match node {
                Item::Function(id) => {
                    if let Some(function) = self.functions[id.0 as usize].ast {
                        collect_function_names(function, &mut names);
                    }
                }
                Item::Constant(id) => {
                    collect_expr_names(&self.constants[id.0 as usize].decl.value, &mut names)
                }
                Item::Type(_) => {}
            }
            for name in names {
                // Qualified references are recorded as `alias.member`.
                let found = match name.split_once('.') {
                    Some((alias, member)) => self
                        .import(module, alias)
                        .and_then(|target| self.item(target, member)),
                    None => self.item(module, &name),
                };
                // Types are not checked as items, so they add no dependency.
                if let Some(&target) = found.and_then(|item| index_of.get(&item))
                    && !edges[index].contains(&target)
                {
                    edges[index].push(target);
                }
            }
        }

        let components = tarjan(&edges);
        components
            .into_iter()
            .map(|c| c.into_iter().map(|i| nodes[i]).collect())
            .collect()
    }

    // ----- type annotations -----------------------------------------------------

    /// Resolves a type annotation written in `module`.
    pub fn resolve_type(
        &mut self,
        module: ModuleId,
        ty: &ast::TypeExpr,
        generics: &HashMap<String, u32>,
    ) -> Type {
        match &ty.kind {
            ast::TypeExprKind::Error => Type::Unknown,
            ast::TypeExprKind::Union(members) => Type::union(
                members
                    .iter()
                    .map(|m| self.resolve_type(module, m, generics))
                    .collect::<Vec<_>>(),
            ),
            ast::TypeExprKind::Named {
                module: Some(alias),
                name,
                args,
            } => {
                let Some(target) = self.import(module, &alias.name) else {
                    self.error(
                        Diagnostic::error(
                            "E_UNKNOWN_TYPE",
                            format!("`{}` is not an imported module", alias.name),
                        )
                        .primary(alias.span, "")
                        .help(format!(
                            "import it first, for example `{} = import(\"...\")`",
                            alias.name
                        )),
                    );
                    return Type::Unknown;
                };
                match self.module_member(module, target, name) {
                    Some(Item::Type(id)) => self.named_annotation(module, id, ty, args, generics),
                    Some(_) => {
                        self.error(
                            Diagnostic::error(
                                "E_UNKNOWN_TYPE",
                                format!("`{}.{}` is not a type", alias.name, name.name),
                            )
                            .primary(ty.span, ""),
                        );
                        Type::Unknown
                    }
                    None => Type::Unknown,
                }
            }
            ast::TypeExprKind::Named {
                module: None,
                name,
                args,
            } => {
                let arity = |checker: &mut Checker, expected: usize| {
                    if args.len() != expected {
                        checker.error(
                            Diagnostic::error(
                                "E_TYPE_ARGUMENTS",
                                format!(
                                    "`{}` takes {expected} type argument{}, found {}",
                                    name.name,
                                    if expected == 1 { "" } else { "s" },
                                    args.len()
                                ),
                            )
                            .primary(ty.span, ""),
                        );
                        false
                    } else {
                        true
                    }
                };
                if let Some(index) = generics.get(&name.name) {
                    if !args.is_empty() {
                        arity(self, 0);
                    }
                    return Type::Param(*index);
                }
                if let Some(int) = crate::infer::int_type_by_name(&name.name) {
                    arity(self, 0);
                    return Type::Int(int);
                }
                if let Some(float) = crate::infer::float_type_by_name(&name.name) {
                    arity(self, 0);
                    return Type::Float(float);
                }
                match name.name.as_str() {
                    "boolean" => Type::Boolean,
                    "string" => Type::String,
                    "nothing" => Type::Nothing,
                    "Error" => Type::Error,
                    "Range" => Type::Range,
                    "Array" => {
                        if !arity(self, 1) {
                            return Type::Unknown;
                        }
                        Type::array(self.resolve_type(module, &args[0], generics))
                    }
                    "Dict" => {
                        if !arity(self, 2) {
                            return Type::Unknown;
                        }
                        let key = self.resolve_type(module, &args[0], generics);
                        let value = self.resolve_type(module, &args[1], generics);
                        if key != Type::String && key != Type::Unknown {
                            self.error(
                                Diagnostic::error(
                                    "E_UNAVAILABLE",
                                    "dictionary keys are strings in this release",
                                )
                                .primary(
                                    args[0].span,
                                    "other key types are planned for milestone M2",
                                ),
                            );
                        }
                        Type::dict(key, value)
                    }
                    other => {
                        match self.item(module, other) {
                            Some(Item::Type(id)) => {
                                return self.named_annotation(module, id, ty, args, generics);
                            }
                            Some(_) => {
                                self.error(
                                    Diagnostic::error(
                                        "E_UNKNOWN_TYPE",
                                        format!("`{other}` is not a type"),
                                    )
                                    .primary(name.span, "a function or constant"),
                                );
                                return Type::Unknown;
                            }
                            None => {}
                        }
                        if matches!(
                            other,
                            "File"
                                | "Path"
                                | "Outcome"
                                | "List"
                                | "Map"
                                | "Set"
                                | "Queue"
                                | "Stack"
                                | "Channel"
                        ) {
                            self.unavailable(ty.span, &format!("the `{other}` type"), "M2");
                        } else if let Some((feature, milestone)) = crate::builtins::planned(other) {
                            self.unavailable(ty.span, feature, milestone);
                        } else {
                            self.error(
                                Diagnostic::error("E_UNKNOWN_TYPE", format!("unknown type `{other}`"))
                                    .primary(name.span, "not a known type")
                                    .help("built-in types include integer, float, boolean, string, nothing, Array<T> and Dict<string, T>; declare your own with `type` or `enum`"),
                            );
                        }
                        Type::Unknown
                    }
                }
            }
        }
    }

    /// A declared structure or enum named in an annotation.
    fn named_annotation(
        &mut self,
        module: ModuleId,
        id: TypeId,
        ty: &ast::TypeExpr,
        args: &[ast::TypeExpr],
        generics: &HashMap<String, u32>,
    ) -> Type {
        let expected = self.types[id.0 as usize].generics.len();
        if args.len() != expected {
            let name = self.types[id.0 as usize].name.clone();
            self.error(
                Diagnostic::error(
                    "E_TYPE_ARGUMENTS",
                    format!(
                        "`{name}` takes {expected} type argument{}, found {}",
                        if expected == 1 { "" } else { "s" },
                        args.len()
                    ),
                )
                .primary(ty.span, ""),
            );
            return Type::Unknown;
        }
        let args = args
            .iter()
            .map(|arg| self.resolve_type(module, arg, generics))
            .collect();
        self.named_type(id, args)
    }

    /// The type of a declared structure or enum with the given arguments.
    pub fn named_type(&self, id: TypeId, args: Vec<Type>) -> Type {
        Type::named(NamedType {
            id,
            name: self.types[id.0 as usize].name.clone(),
            args,
            copy: self
                .type_defs
                .get(id.0 as usize)
                .is_some_and(|def| def.copy),
        })
    }

    /// The type of a declared structure or enum with fresh type arguments.
    pub fn instantiate_named(&mut self, id: TypeId, span: Span) -> Type {
        let count = self.types[id.0 as usize].generics.len();
        let args = (0..count).map(|_| self.fresh_var(span)).collect();
        self.named_type(id, args)
    }

    pub fn type_def(&self, id: TypeId) -> &TypeDef {
        &self.type_defs[id.0 as usize]
    }

    /// Resolves the field types of every declared structure and enum, decides
    /// which are Copy, and rejects types that contain themselves.
    pub fn resolve_type_defs(&mut self) {
        // Placeholders so that named types can be built while fields resolve.
        self.type_defs = self
            .types
            .iter()
            .map(|entry| TypeDef {
                name: entry.name.clone(),
                generics: entry.generics.len() as u32,
                kind: TypeDefKind::Structure(Vec::new()),
                copy: true,
            })
            .collect();
        for index in 0..self.types.len() {
            let entry = &self.types[index];
            let module = entry.module;
            let generics: HashMap<String, u32> = entry
                .generics
                .iter()
                .enumerate()
                .map(|(i, g)| (g.name.clone(), i as u32))
                .collect();
            let kind = match entry.source {
                TypeSource::Structure(fields) => {
                    if fields.is_empty() {
                        let span = entry.span;
                        self.error(
                            Diagnostic::error(
                                "E_EMPTY_STRUCTURE",
                                "a structure needs at least one field",
                            )
                            .primary(span, "")
                            .help("for a value without fields, use an enum variant such as `enum Marker` with `ready`"),
                        );
                    }
                    TypeDefKind::Structure(self.resolve_fields(module, fields, &generics))
                }
                TypeSource::Enum(variants) => {
                    let mut seen: Vec<&ast::Ident> = Vec::new();
                    let mut resolved = Vec::new();
                    for variant in variants {
                        if let Some(first) = seen.iter().find(|v| v.name == variant.name.name) {
                            let first = first.span;
                            self.error(
                                Diagnostic::error(
                                    "E_DUPLICATE_VARIANT",
                                    format!(
                                        "the variant `{}` is declared twice",
                                        variant.name.name
                                    ),
                                )
                                .primary(variant.name.span, "")
                                .secondary(first, "first declared here"),
                            );
                            continue;
                        }
                        seen.push(&variant.name);
                        resolved.push(VariantDef {
                            name: variant.name.name.clone(),
                            fields: self.resolve_fields(module, &variant.fields, &generics),
                        });
                    }
                    TypeDefKind::Enum(resolved)
                }
            };
            self.type_defs[index].kind = kind;
        }

        // A definition is Copy when all its fields are; decreasing to a fixed point.
        loop {
            let mut changed = false;
            for index in 0..self.type_defs.len() {
                if !self.type_defs[index].copy {
                    continue;
                }
                let copy = all_field_types(&self.type_defs[index])
                    .iter()
                    .all(|ty| self.copy_assuming_params(ty));
                if !copy {
                    self.type_defs[index].copy = false;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let flags: Vec<bool> = self.type_defs.iter().map(|def| def.copy).collect();
        for def in &mut self.type_defs {
            match &mut def.kind {
                TypeDefKind::Structure(fields) => fields
                    .iter_mut()
                    .for_each(|f| set_copy_flags(&mut f.ty, &flags)),
                TypeDefKind::Enum(variants) => variants
                    .iter_mut()
                    .flat_map(|v| v.fields.iter_mut())
                    .for_each(|f| set_copy_flags(&mut f.ty, &flags)),
            }
        }
        self.unifier.field_types = self.type_defs.iter().map(all_field_types).collect();

        for index in 0..self.type_defs.len() {
            let id = TypeId(index as u32);
            let args = (0..self.type_defs[index].generics)
                .map(Type::Param)
                .collect();
            let start = self.named_type(id, args);
            if let Some(path) = self.inline_cycle(&start, &mut vec![id]) {
                let name = self.types[index].name.clone();
                let span = self.types[index].span;
                let through = if path.len() > 1 {
                    let names: Vec<String> = path[1..]
                        .iter()
                        .map(|id| format!("`{}`", self.types[id.0 as usize].name))
                        .collect();
                    format!(" (through {})", names.join(", "))
                } else {
                    String::new()
                };
                self.error(
                    Diagnostic::error(
                        "E_RECURSIVE_TYPE",
                        format!("`{name}` contains itself{through}, so it would have no size"),
                    )
                    .primary(span, "")
                    .help(format!(
                        "store nested values in a collection, for example a field `children: Array<{name}>`"
                    )),
                );
            }
        }
    }

    fn resolve_fields(
        &mut self,
        module: ModuleId,
        fields: &[ast::Field],
        generics: &HashMap<String, u32>,
    ) -> Vec<FieldDef> {
        let mut resolved: Vec<FieldDef> = Vec::new();
        let mut seen: Vec<&ast::Ident> = Vec::new();
        for field in fields {
            if let Some(first) = seen.iter().find(|f| f.name == field.name.name) {
                let first = first.span;
                self.error(
                    Diagnostic::error(
                        "E_DUPLICATE_FIELD",
                        format!("the field `{}` is declared twice", field.name.name),
                    )
                    .primary(field.name.span, "")
                    .secondary(first, "first declared here"),
                );
                continue;
            }
            seen.push(&field.name);
            resolved.push(FieldDef {
                name: field.name.name.clone(),
                ty: self.resolve_type(module, &field.ty, generics),
            });
        }
        resolved
    }

    /// Whether a field type is Copy when type parameters are.
    fn copy_assuming_params(&self, ty: &Type) -> bool {
        match ty {
            Type::Param(_) => true,
            Type::Union(members) => members.iter().all(|m| self.copy_assuming_params(m)),
            Type::Named(named) => {
                self.type_defs[named.id.0 as usize].copy
                    && named.args.iter().all(|a| self.copy_assuming_params(a))
            }
            other => other.is_copy(),
        }
    }

    /// Follows the fields a value of `ty` stores inline (not through a
    /// collection), looking for a type already on `stack`. Returns the chain
    /// of types that leads back.
    fn inline_cycle(&self, ty: &Type, stack: &mut Vec<TypeId>) -> Option<Vec<TypeId>> {
        let Type::Named(named) = ty else {
            return None;
        };
        let def = &self.type_defs[named.id.0 as usize];
        for field in all_field_types(def) {
            let field = field.substitute(&named.args);
            for inner in inline_named(&field) {
                if stack.first() == Some(&inner.id) {
                    return Some(stack.clone());
                }
                if stack.contains(&inner.id) {
                    // A cycle that does not include the starting type is
                    // reported when that type is checked.
                    continue;
                }
                stack.push(inner.id);
                let found = self.inline_cycle(&Type::Named(Box::new(inner.clone())), stack);
                stack.pop();
                if found.is_some() {
                    return found;
                }
            }
        }
        None
    }

    // ----- signatures and instantiation ---------------------------------------

    /// Creates the monomorphic signature used while a function's component is
    /// being inferred.
    pub fn initial_signature(&mut self, id: FuncId) -> Signature {
        let function = self.functions[id.0 as usize].ast.expect("function source");
        let module = self.functions[id.0 as usize].module;
        let generics: HashMap<String, u32> = function
            .generics
            .iter()
            .enumerate()
            .map(|(i, g)| (g.name.clone(), i as u32))
            .collect();
        let mut params = Vec::new();
        let mut explicit_modes = Vec::new();
        let mut required = 0;
        let mut seen_default = false;
        for param in &function.params {
            let ty = match &param.ty {
                Some(annotation) => self.resolve_type(module, annotation, &generics),
                None => self.fresh_var(param.name.span),
            };
            params.push(ty);
            explicit_modes.push(param.mode.map(|(mode, _)| mode));
            if param.default.is_some() {
                seen_default = true;
            } else if seen_default {
                self.error(
                    Diagnostic::error(
                        "E_DEFAULT_ORDER",
                        "parameters without defaults must come before parameters with defaults",
                    )
                    .primary(param.span, ""),
                );
            } else {
                required += 1;
            }
        }
        let ret = match &function.ret {
            Some(annotation) => self.resolve_type(module, annotation, &generics),
            None => self.fresh_var(function.header_span),
        };
        Signature {
            params,
            param_names: function
                .params
                .iter()
                .map(|p| p.name.name.clone())
                .collect(),
            explicit_modes,
            required,
            ret,
            generic_count: function.generics.len() as u32,
            explicit_generics: function.generics.len() as u32,
            bounds: vec![Bounds::default(); function.generics.len()],
        }
    }

    /// Returns parameter types, the return type, and the type arguments for a
    /// call to `id`.
    pub fn instantiate(
        &mut self,
        id: FuncId,
        explicit: &[Type],
        span: Span,
    ) -> (Signature, Vec<Type>, Vec<Type>, Type) {
        let entry = &self.functions[id.0 as usize];
        let signature = entry
            .signature
            .clone()
            .expect("signature available before use");
        if !entry.generalized {
            // A recursive call inside the component being inferred; type
            // arguments are filled in once the component is generalized.
            if !explicit.is_empty() && !signature.params.is_empty() {
                let _ = span;
            }
            let params = signature.params.clone();
            let ret = signature.ret.clone();
            return (signature, params, Vec::new(), ret);
        }
        let mut type_args = Vec::new();
        for bounds in &signature.bounds {
            type_args.push(self.fresh_bounded(*bounds, span));
        }
        if !explicit.is_empty() {
            if explicit.len() as u32 != signature.explicit_generics {
                self.error(
                    Diagnostic::error(
                        "E_TYPE_ARGUMENTS",
                        format!(
                            "`{}` takes {} type argument{}, found {}",
                            self.functions[id.0 as usize].name,
                            signature.explicit_generics,
                            if signature.explicit_generics == 1 {
                                ""
                            } else {
                                "s"
                            },
                            explicit.len()
                        ),
                    )
                    .primary(span, ""),
                );
            } else {
                for (arg, given) in type_args.clone().iter().zip(explicit) {
                    self.unify_at(arg, given, span, "explicit type argument");
                }
            }
        }
        let params = signature
            .params
            .iter()
            .map(|p| p.substitute(&type_args))
            .collect();
        let ret = signature.ret.substitute(&type_args);
        (signature, params, type_args, ret)
    }

    // ----- deferred constraints ---------------------------------------------------

    /// Applies deferred constraints whose types have become known. Returns the
    /// constraints that remain unresolved.
    pub fn solve_deferred(&mut self, final_pass: bool) {
        loop {
            let pending = std::mem::take(&mut self.deferred);
            let before = pending.len();
            let mut remaining = Vec::new();
            for constraint in pending {
                if !self.try_apply(&constraint) {
                    remaining.push(constraint);
                }
            }
            let progressed = remaining.len() < before;
            self.deferred = remaining;
            if !progressed || self.deferred.is_empty() {
                break;
            }
        }
        if final_pass && !self.deferred.is_empty() {
            // An unknown iterated or indexed value is taken to be an array, or a
            // dictionary when it has key and value bindings or a string key.
            let pending = std::mem::take(&mut self.deferred);
            let mut defaulted = false;
            for constraint in &pending {
                match constraint {
                    Deferred::Iterate {
                        iterable, items, ..
                    } => {
                        if let Type::Var(_) = self.unifier.shallow(iterable) {
                            let shape = match items.as_slice() {
                                [element] => Type::array(element.clone()),
                                [key, value] => Type::dict(key.clone(), value.clone()),
                                _ => continue,
                            };
                            defaulted |= self.unifier.unify(iterable, &shape).is_ok();
                        }
                    }
                    Deferred::Index {
                        container,
                        index,
                        result,
                        ..
                    } => {
                        if let Type::Var(_) = self.unifier.shallow(container) {
                            let shape = if self.unifier.resolve(index) == Type::String {
                                Type::dict(Type::String, result.clone())
                            } else {
                                Type::array(result.clone())
                            };
                            defaulted |= self.unifier.unify(container, &shape).is_ok();
                        }
                    }
                    Deferred::EmptyCollection { .. } => {}
                    Deferred::Field {
                        target,
                        name,
                        module,
                        span,
                        ..
                    } => {
                        // The one structure of the module with this field.
                        if let Type::Var(_) = self.unifier.shallow(target) {
                            let candidates = self.structures_with_field(*module, name);
                            if let [id] = candidates.as_slice() {
                                let shape = self.instantiate_named(*id, *span);
                                defaulted |= self.unifier.unify(target, &shape).is_ok();
                            }
                        }
                    }
                }
            }
            self.deferred = pending;
            if defaulted {
                self.solve_deferred(false);
            }
        }
        if final_pass {
            for constraint in std::mem::take(&mut self.deferred) {
                let (span, message, help) = match &constraint {
                    Deferred::Index { span, .. } => (
                        *span,
                        "cannot infer what kind of collection is indexed here",
                        "annotate the collection, for example `values: Array<integer>`",
                    ),
                    Deferred::Iterate { span, .. } => (
                        *span,
                        "cannot infer what this loop iterates over",
                        "annotate the collection, for example `values: Array<integer>`",
                    ),
                    Deferred::EmptyCollection { span, .. } => (
                        *span,
                        "cannot infer the type of this empty collection",
                        "annotate the binding, for example `names: Array<string> = []`",
                    ),
                    Deferred::Field {
                        name, module, span, ..
                    } => {
                        let count = self.structures_with_field(*module, name).len();
                        let message = if count > 1 {
                            format!(
                                "several structures have a field `{name}`, so the type of this value is unclear"
                            )
                        } else {
                            format!("cannot infer the type that has `.{name}`")
                        };
                        self.error(
                            Diagnostic::error("E_AMBIGUOUS_TYPE", message)
                                .primary(*span, "")
                                .help("annotate the value's type, for example a parameter `point: Point`"),
                        );
                        continue;
                    }
                };
                self.error(
                    Diagnostic::error("E_AMBIGUOUS_TYPE", message)
                        .primary(span, "")
                        .help(help),
                );
            }
        }
    }

    /// Structures declared in `module` that have a field called `name`.
    pub fn structures_with_field(&self, module: ModuleId, name: &str) -> Vec<TypeId> {
        (0..self.types.len())
            .map(|i| TypeId(i as u32))
            .filter(|id| {
                self.types[id.0 as usize].module == module
                    && self.type_defs[id.0 as usize].field_index(name).is_some()
            })
            .collect()
    }

    /// The type of `target.name` when `target` has type `target_ty`; reports
    /// unknown fields and defers the decision while the type is unknown.
    pub fn field_type(
        &mut self,
        target_ty: &Type,
        name: &ast::Ident,
        span: Span,
        module: ModuleId,
    ) -> Type {
        match self.unifier.shallow(target_ty) {
            Type::Var(_) => {
                let result = self.fresh_var(span);
                self.deferred.push(Deferred::Field {
                    target: target_ty.clone(),
                    name: name.name.clone(),
                    result: result.clone(),
                    span,
                    module,
                });
                result
            }
            resolved => self.known_field_type(&resolved, &name.name, name.span),
        }
    }

    fn known_field_type(&mut self, ty: &Type, name: &str, span: Span) -> Type {
        match ty {
            Type::Unknown => Type::Unknown,
            Type::Named(named) if !self.type_def(named.id).is_enum() => {
                let def = self.type_def(named.id);
                match def.field_index(name) {
                    Some(index) => def.fields()[index].ty.substitute(&named.args),
                    None => {
                        let names: Vec<String> = def
                            .fields()
                            .iter()
                            .map(|f| format!("`{}`", f.name))
                            .collect();
                        let structure = def.name.clone();
                        self.error(
                            Diagnostic::error(
                                "E_UNKNOWN_FIELD",
                                format!("`{structure}` has no field `{name}`"),
                            )
                            .primary(span, "")
                            .help(format!("its fields are {}", names.join(", "))),
                        );
                        Type::Unknown
                    }
                }
            }
            Type::Named(named) => {
                self.error(
                    Diagnostic::error(
                        "E_UNKNOWN_FIELD",
                        format!("the enum `{}` has no fields", named.name),
                    )
                    .primary(span, "")
                    .help("use `match` to read the values inside a variant"),
                );
                Type::Unknown
            }
            Type::Union(_) => {
                self.error(
                    Diagnostic::error(
                        "E_UNKNOWN_FIELD",
                        format!("{ty} has no field `{name}`"),
                    )
                    .primary(span, "")
                    .help("check which kind of value it holds first, for example `if value != nothing`"),
                );
                Type::Unknown
            }
            other => {
                self.error(
                    Diagnostic::error("E_UNKNOWN_FIELD", format!("{other} has no field `{name}`"))
                        .primary(span, ""),
                );
                Type::Unknown
            }
        }
    }

    fn try_apply(&mut self, constraint: &Deferred) -> bool {
        match constraint {
            Deferred::Index {
                container,
                index,
                result,
                span,
            } => match self.unifier.shallow(container) {
                Type::Array(element) => {
                    self.unify_at(&Type::INTEGER, index, *span, "array indexes are integers");
                    self.unify_at(result, &element, *span, "");
                    true
                }
                Type::Dict(key, value) => {
                    self.unify_at(&key, index, *span, "dictionary key");
                    self.unify_at(result, &value, *span, "");
                    true
                }
                Type::Var(_) => false,
                Type::Unknown => true,
                other => {
                    self.not_indexable(&other, *span);
                    true
                }
            },
            Deferred::Iterate {
                iterable,
                items,
                span,
            } => match self.unifier.shallow(iterable) {
                Type::Var(_) => false,
                other => {
                    self.apply_iteration(&other, items, *span);
                    true
                }
            },
            Deferred::EmptyCollection { ty, .. } => {
                !matches!(self.unifier.shallow(ty), Type::Var(_))
            }
            Deferred::Field {
                target,
                name,
                result,
                span,
                ..
            } => match self.unifier.shallow(target) {
                Type::Var(_) => false,
                resolved => {
                    let field = self.known_field_type(&resolved, name, *span);
                    self.unify_at(result, &field, *span, "");
                    true
                }
            },
        }
    }

    pub fn not_indexable(&mut self, ty: &Type, span: Span) {
        if *ty == Type::String {
            self.unavailable(span, "string indexing and slicing", "M2");
        } else {
            self.error(
                Diagnostic::error("E_NOT_INDEXABLE", format!("{ty} cannot be indexed"))
                    .primary(span, "")
                    .help("only arrays and dictionaries support `[...]`"),
            );
        }
    }

    /// Unifies loop variable types with the element types of `iterable`.
    pub fn apply_iteration(&mut self, iterable: &Type, items: &[Type], span: Span) {
        let element_types: Vec<Type> = match iterable {
            Type::Array(element) => vec![(**element).clone()],
            Type::Range => vec![Type::INTEGER],
            Type::Dict(key, value) => {
                if items.len() == 2 {
                    vec![(**key).clone(), (**value).clone()]
                } else {
                    vec![(**key).clone()]
                }
            }
            Type::Unknown => return,
            Type::String => {
                self.unavailable(span, "iteration over the characters of a string", "M2");
                return;
            }
            other => {
                self.error(
                    Diagnostic::error("E_NOT_ITERABLE", format!("cannot iterate over {other}"))
                        .primary(span, "")
                        .help("`for` loops iterate over arrays, dictionaries and ranges"),
                );
                return;
            }
        };
        if items.len() != element_types.len() {
            let message = if matches!(iterable, Type::Dict(..)) {
                "a dictionary loop takes one variable (the key) or two (key and value)"
            } else {
                "this loop takes exactly one variable"
            };
            self.error(Diagnostic::error("E_LOOP_BINDINGS", message).primary(span, ""));
            return;
        }
        for (item, element) in items.iter().zip(&element_types) {
            self.unify_at(item, element, span, "loop variable");
        }
    }

    // ----- generalization and finalization ----------------------------------------

    /// Generalizes the signature of a single-function component.
    pub fn generalize(&mut self, id: FuncId) {
        let mut signature = self.functions[id.0 as usize]
            .signature
            .clone()
            .expect("signature");
        let mut free = Vec::new();
        for param in &signature.params {
            self.unifier.free_vars(param, &mut free);
        }
        // A result type that nothing constrains (a function that only fails)
        // is `nothing` rather than a type parameter callers cannot determine.
        if let Type::Var(var) = self.unifier.shallow(&signature.ret)
            && !free.contains(&var)
            && self.unifier.bounds(var) == Bounds::default()
        {
            self.unifier.force_bind(var, Type::Nothing);
        }
        self.unifier.free_vars(&signature.ret, &mut free);
        for var in free {
            let index = signature.generic_count;
            signature.generic_count += 1;
            signature.bounds.push(self.unifier.bounds(var));
            self.unifier.force_bind(var, Type::Param(index));
        }
        signature.params = signature
            .params
            .iter()
            .map(|p| self.unifier.resolve(p))
            .collect();
        signature.ret = self.unifier.resolve(&signature.ret);
        let entry = &mut self.functions[id.0 as usize];
        entry.signature = Some(signature);
        entry.generalized = true;
    }

    /// Defaults literal variables created since `first_var`, then reports any
    /// variable that is still unresolved.
    pub fn default_vars(&mut self, first_var: u32) {
        for index in first_var..self.unifier.var_count() {
            self.unifier.default_var(TyVar(index));
        }
    }

    pub fn finalize_type(&mut self, ty: &Type) -> Type {
        let resolved = self.unifier.resolve(ty);
        if resolved.has_vars() {
            let mut free = Vec::new();
            self.unifier.free_vars(&resolved, &mut free);
            for var in free {
                if self.reported_vars.insert(var) {
                    let span = self.var_spans[var.0 as usize];
                    self.error(
                        Diagnostic::error("E_AMBIGUOUS_TYPE", "cannot infer a type here")
                            .primary(span, "add a type annotation")
                            .help("for example `value: integer = ...` or a parameter annotation `name: string`"),
                    );
                }
                self.unifier.force_bind(var, Type::Unknown);
            }
            return self.unifier.resolve(&resolved);
        }
        resolved
    }

    pub fn finalize_function(&mut self, function: &mut hir::Function) {
        for local in &mut function.locals {
            local.ty = self.finalize_type(&local.ty);
        }
        for param in &mut function.params {
            param.ty = self.finalize_type(&param.ty);
            if let Some(default) = &mut param.default {
                self.finalize_expr(default);
            }
        }
        function.ret = self.finalize_type(&function.ret);
        let mut body = std::mem::take(&mut function.body);
        self.finalize_block(&mut body);
        function.body = body;
    }

    pub fn finalize_block(&mut self, block: &mut hir::Block) {
        for stmt in &mut block.stmts {
            self.finalize_stmt(stmt);
        }
    }

    fn finalize_place(&mut self, place: &mut hir::Place) {
        match place {
            hir::Place::Local(_) => {}
            hir::Place::Index {
                target,
                target_ty,
                index,
            } => {
                self.finalize_place(target);
                *target_ty = self.finalize_type(target_ty);
                self.finalize_expr(index);
            }
            hir::Place::Field {
                target, target_ty, ..
            } => {
                self.finalize_place(target);
                *target_ty = self.finalize_type(target_ty);
            }
            hir::Place::Narrow { target, ty } => {
                self.finalize_place(target);
                *ty = self.finalize_type(ty);
            }
        }
    }

    fn finalize_pattern(&mut self, pattern: &mut hir::Pattern) {
        match pattern {
            hir::Pattern::Wildcard | hir::Pattern::Binding(_) => {}
            hir::Pattern::Literal(value) => self.finalize_expr(value),
            hir::Pattern::Variant { ty, fields, .. } => {
                *ty = self.finalize_type(ty);
                fields.iter_mut().for_each(|f| self.finalize_pattern(f));
            }
            hir::Pattern::Member { member, inner } => {
                *member = self.finalize_type(member);
                self.finalize_pattern(inner);
            }
        }
    }

    fn finalize_stmt(&mut self, stmt: &mut hir::Stmt) {
        use hir::StmtKind::*;
        match &mut stmt.kind {
            Let { value, .. } | Expr(value) => self.finalize_expr(value),
            Assign { place, value } | CompoundAssign { place, value, .. } => {
                self.finalize_place(place);
                self.finalize_expr(value);
            }
            If {
                condition,
                then_block,
                else_block,
            } => {
                self.finalize_expr(condition);
                self.finalize_block(then_block);
                if let Some(block) = else_block {
                    self.finalize_block(block);
                }
            }
            While { condition, body } => {
                self.finalize_expr(condition);
                self.finalize_block(body);
            }
            For { iterable, body, .. } => {
                self.finalize_expr(iterable);
                self.finalize_block(body);
            }
            Return(value) => {
                if let Some(value) = value {
                    self.finalize_expr(value);
                }
            }
            Try {
                body,
                catch,
                finally,
            } => {
                self.finalize_block(body);
                if let Some((_, block)) = catch {
                    self.finalize_block(block);
                }
                if let Some(block) = finally {
                    self.finalize_block(block);
                }
            }
            Block(block) => self.finalize_block(block),
            Match {
                scrutinee, arms, ..
            } => {
                self.finalize_expr(scrutinee);
                for arm in arms {
                    self.finalize_pattern(&mut arm.pattern);
                    if let Some(guard) = &mut arm.guard {
                        self.finalize_expr(guard);
                    }
                    self.finalize_block(&mut arm.body);
                }
            }
            Break | Continue => {}
        }
    }

    pub fn finalize_expr(&mut self, expr: &mut hir::Expr) {
        expr.ty = self.finalize_type(&expr.ty);
        use hir::ExprKind::*;
        match &mut expr.kind {
            Integer(value) => {
                if let Type::Int(int) = &expr.ty
                    && (*value < int.min_value() || *value > int.max_value())
                {
                    self.error(
                        Diagnostic::error(
                            "E_LITERAL_RANGE",
                            format!("{value} does not fit in {}", int.name()),
                        )
                        .primary(
                            expr.span,
                            format!(
                                "{} holds {} to {}",
                                int.name(),
                                int.min_value(),
                                int.max_value()
                            ),
                        ),
                    );
                }
            }
            Float(_) | Boolean(_) | String(_) | Nothing | Local { .. } | Constant(_) => {}
            Call {
                type_args, args, ..
            } => {
                for ty in type_args.iter_mut() {
                    *ty = self.finalize_type(ty);
                }
                for arg in args {
                    self.finalize_expr(&mut arg.expr);
                }
            }
            Builtin { args, .. } => {
                for arg in args {
                    self.finalize_expr(&mut arg.expr);
                }
            }
            Binary { lhs, rhs, .. } => {
                self.finalize_expr(lhs);
                self.finalize_expr(rhs);
            }
            Unary { operand, .. } => self.finalize_expr(operand),
            Array(items) | Variant { args: items, .. } => {
                for item in items {
                    self.finalize_expr(item);
                }
            }
            Record(fields) => {
                for (_, value) in fields {
                    self.finalize_expr(value);
                }
            }
            Field { value, .. } => self.finalize_expr(value),
            Dict(entries) => {
                for (key, value) in entries {
                    self.finalize_expr(key);
                    self.finalize_expr(value);
                }
            }
            Index { target, index } => {
                self.finalize_expr(target);
                self.finalize_expr(index);
            }
            Interpolate(segments) => {
                for segment in segments {
                    if let hir::Segment::Value(value) = segment {
                        self.finalize_expr(value);
                    }
                }
            }
            Inject(inner) | Narrow(inner) | UnionTypeName(inner) => self.finalize_expr(inner),
            IsMember { value, member } => {
                self.finalize_expr(value);
                *member = self.finalize_type(member);
            }
            ErrorField { value, .. } => self.finalize_expr(value),
        }
    }
}

fn conversion_help(expected: &Type, found: &Type) -> Option<String> {
    match (expected, found) {
        (Type::Float(f), Type::Int(_)) => {
            Some(format!("convert explicitly with {}(...)", f.name()))
        }
        (Type::Int(i), Type::Float(_)) | (Type::Int(i), Type::Int(_)) => Some(format!(
            "convert explicitly with {}(...); conversions are checked",
            i.name()
        )),
        (Type::String, Type::Int(_) | Type::Float(_) | Type::Boolean) => {
            Some("convert with tostring(...) or use interpolation \"{value}\"".to_string())
        }
        (Type::Int(_) | Type::Float(_), Type::String) => {
            Some("parse text with tonumber(...)".to_string())
        }
        _ => None,
    }
}

/// Every field type of a structure, or of all variants of an enum.
fn all_field_types(def: &TypeDef) -> Vec<Type> {
    match &def.kind {
        TypeDefKind::Structure(fields) => fields.iter().map(|f| f.ty.clone()).collect(),
        TypeDefKind::Enum(variants) => variants
            .iter()
            .flat_map(|v| v.fields.iter().map(|f| f.ty.clone()))
            .collect(),
    }
}

/// Updates the Copy flag carried by every named type inside `ty`.
fn set_copy_flags(ty: &mut Type, flags: &[bool]) {
    match ty {
        Type::Named(named) => {
            named.copy = flags[named.id.0 as usize];
            named.args.iter_mut().for_each(|a| set_copy_flags(a, flags));
        }
        Type::Array(element) => set_copy_flags(element, flags),
        Type::Dict(key, value) => {
            set_copy_flags(key, flags);
            set_copy_flags(value, flags);
        }
        Type::Union(members) => members.iter_mut().for_each(|m| set_copy_flags(m, flags)),
        _ => {}
    }
}

/// The named types a value of `ty` stores inline: itself, or the members of
/// a union. Collections store their elements elsewhere.
fn inline_named(ty: &Type) -> Vec<&NamedType> {
    match ty {
        Type::Named(named) => vec![named],
        Type::Union(members) => members.iter().flat_map(inline_named).collect(),
        _ => Vec::new(),
    }
}

fn collect_script_names(stmt: &ast::Stmt, names: &mut HashSet<String>) {
    match &stmt.kind {
        StmtKind::Assign { target, .. } => {
            if let ast::ExprKind::Name(name) = &target.kind {
                names.insert(name.name.clone());
            }
        }
        StmtKind::Declare(decl) => {
            names.insert(decl.name.name.clone());
        }
        _ => {}
    }
}

pub(crate) fn collect_function_names(function: &ast::Function, names: &mut Vec<String>) {
    for param in &function.params {
        if let Some(default) = &param.default {
            collect_expr_names(default, names);
        }
    }
    collect_block_names(&function.body, names);
}

fn collect_block_names(block: &ast::Block, names: &mut Vec<String>) {
    for stmt in &block.stmts {
        collect_stmt_names(stmt, names);
    }
}

fn collect_stmt_names(stmt: &ast::Stmt, names: &mut Vec<String>) {
    match &stmt.kind {
        StmtKind::Expr(expr) | StmtKind::Return(Some(expr)) => collect_expr_names(expr, names),
        StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
            collect_expr_names(target, names);
            collect_expr_names(value, names);
        }
        StmtKind::Declare(decl) => collect_expr_names(&decl.value, names),
        StmtKind::If {
            branches,
            else_body,
        } => {
            for branch in branches {
                collect_expr_names(&branch.condition, names);
                collect_block_names(&branch.body, names);
            }
            if let Some(block) = else_body {
                collect_block_names(block, names);
            }
        }
        StmtKind::While { condition, body } => {
            collect_expr_names(condition, names);
            collect_block_names(body, names);
        }
        StmtKind::For { iterable, body, .. } => {
            collect_expr_names(iterable, names);
            collect_block_names(body, names);
        }
        StmtKind::Try {
            body,
            catch,
            finally,
        } => {
            collect_block_names(body, names);
            if let Some(catch) = catch {
                collect_block_names(&catch.body, names);
            }
            if let Some(block) = finally {
                collect_block_names(block, names);
            }
        }
        StmtKind::Function(function) => collect_function_names(function, names),
        StmtKind::Match { scrutinee, arms } => {
            collect_expr_names(scrutinee, names);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_expr_names(guard, names);
                }
                collect_block_names(&arm.body, names);
            }
        }
        _ => {}
    }
}

pub(crate) fn collect_expr_names(expr: &ast::Expr, names: &mut Vec<String>) {
    use ast::ExprKind::*;
    match &expr.kind {
        Name(name) => names.push(name.name.clone()),
        String(segments) => {
            for segment in segments {
                if let ast::StringSegment::Interpolation(inner) = segment {
                    collect_expr_names(inner, names);
                }
            }
        }
        Array(items) => items
            .iter()
            .for_each(|item| collect_expr_names(item, names)),
        Dict(entries) => entries.iter().for_each(|(k, v)| {
            collect_expr_names(k, names);
            collect_expr_names(v, names);
        }),
        Record(fields) => fields
            .iter()
            .for_each(|(_, v)| collect_expr_names(v, names)),
        Call { callee, args, .. } => {
            collect_expr_names(callee, names);
            args.iter().for_each(|arg| collect_expr_names(arg, names));
        }
        Construct { callee, fields, .. } => {
            collect_expr_names(callee, names);
            fields
                .iter()
                .for_each(|(_, value)| collect_expr_names(value, names));
        }
        Index { target, index } => {
            collect_expr_names(target, names);
            collect_expr_names(index, names);
        }
        Member { target, name } => {
            if let Name(alias) = &target.kind {
                names.push(format!("{}.{}", alias.name, name.name));
            }
            collect_expr_names(target, names)
        }
        Unary { operand, .. } => collect_expr_names(operand, names),
        Binary { lhs, rhs, .. } => {
            collect_expr_names(lhs, names);
            collect_expr_names(rhs, names);
        }
        IsError(inner) | TryExpr(inner) => collect_expr_names(inner, names),
        Function(function) => collect_function_names(function, names),
        Integer { .. } | Float(_) | Boolean(_) | Nothing | Import { .. } | Error => {}
    }
}

/// Tarjan's algorithm; components are returned in reverse topological order
/// of the edge relation, so dependencies come first.
fn tarjan(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    struct State<'e> {
        edges: &'e [Vec<usize>],
        index: Vec<Option<usize>>,
        low: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        next: usize,
        components: Vec<Vec<usize>>,
    }
    fn visit(state: &mut State, node: usize) {
        state.index[node] = Some(state.next);
        state.low[node] = state.next;
        state.next += 1;
        state.stack.push(node);
        state.on_stack[node] = true;
        for &target in &state.edges[node] {
            match state.index[target] {
                None => {
                    visit(state, target);
                    state.low[node] = state.low[node].min(state.low[target]);
                }
                Some(target_index) if state.on_stack[target] => {
                    state.low[node] = state.low[node].min(target_index);
                }
                Some(_) => {}
            }
        }
        if Some(state.low[node]) == state.index[node] {
            let mut component = Vec::new();
            loop {
                let member = state.stack.pop().expect("stack");
                state.on_stack[member] = false;
                component.push(member);
                if member == node {
                    break;
                }
            }
            component.sort();
            state.components.push(component);
        }
    }
    let count = edges.len();
    let mut state = State {
        edges,
        index: vec![None; count],
        low: vec![0; count],
        on_stack: vec![false; count],
        stack: Vec::new(),
        next: 0,
        components: Vec::new(),
    };
    for node in 0..count {
        if state.index[node].is_none() {
            visit(&mut state, node);
        }
    }
    state.components
}

#[cfg(test)]
mod tests {
    use super::tarjan;

    #[test]
    fn components_in_dependency_order() {
        // 0 -> 1 -> 2 -> 1, 3 isolated
        let edges = vec![vec![1], vec![2], vec![1], vec![]];
        let components = tarjan(&edges);
        assert_eq!(components, vec![vec![1, 2], vec![0], vec![3]]);
    }
}
