//! What the names of a program refer to, for editors (`sayon lsp`): each
//! place a name is written, where it is declared and where it is used, with
//! the symbol it means, and what each symbol is. The checker notes each name
//! as it resolves it, so the index holds what it could resolve even when the
//! program has errors.

use crate::checker::{Checker, Item, ModuleId, TypeSource, snake_case};
use crate::hir::LocalKind;
use crate::types::{Type, TypeId};
use sayon_syntax::ast;
use sayon_syntax::{FileId, Span};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Variable,
    Parameter,
    Constant,
    Function,
    Type,
    Field,
    Variant,
    /// An import's alias for a module.
    Module,
}

/// Whose members follow a dot: a type's fields, variants, and methods, or a
/// module's definitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Owner {
    Type(u32),
    Module(u32),
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Where its name is declared.
    pub span: Span,
    /// What it is, as a line of Sayon: a signature, or a name and its type.
    pub detail: String,
    /// For a module's alias, the module's file, which going to its
    /// definition opens.
    pub target: Option<FileId>,
    /// What follows a dot after it: the members of its type, or of the
    /// module it names.
    pub members: Option<Owner>,
    /// Declared by the standard library, so not to be renamed.
    pub standard: bool,
    /// What it is or gives, as Sayon writes the type: a value's type, a
    /// function's result, or the type a structure or variant builds.
    pub ty: Option<String>,
    /// What a call of it takes: a function's parameters, or the fields of
    /// a structure or a variant it builds.
    pub params: Vec<Param>,
}

/// A parameter of a function, or a field a structure or variant is built
/// with.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: String,
    /// Whether a call can leave it out.
    pub default: bool,
    /// `update` or `own`, when written.
    pub mode: Option<&'static str>,
    /// The type whose members a value of it has, for what is passed.
    pub owner: Option<Owner>,
}

/// A name as written: its span, and the symbol it means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Reference {
    pub span: Span,
    pub symbol: usize,
}

/// The local variables of a function body, for completion: `body` is the
/// body's span, or nothing for a script's top level.
#[derive(Clone, Debug)]
pub struct LocalScope {
    pub file: FileId,
    pub body: Option<Span>,
    pub locals: Vec<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Index {
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
    /// The members of each type and module, with the names a dot writes
    /// them by: a method by its name without its type's prefix.
    pub members: HashMap<Owner, Vec<(String, usize)>>,
    /// What each module's own code names without a dot: its definitions
    /// and the aliases of its imports.
    pub scopes: HashMap<FileId, Vec<usize>>,
    pub locals: Vec<LocalScope>,
}

impl Index {
    /// The name written at `offset` of `file`, if any: the reference whose
    /// span holds it, or ends at it.
    pub fn at(&self, file: FileId, offset: u32) -> Option<Reference> {
        self.references
            .iter()
            .filter(|r| r.span.file == file && r.span.start <= offset && offset <= r.span.end)
            .min_by_key(|r| r.span.len())
            .copied()
    }

    /// Every place the symbol's name is written, its declaration first.
    pub fn references_to(&self, symbol: usize) -> Vec<Span> {
        let declared = self.symbols[symbol].span;
        let mut spans: Vec<Span> = self
            .references
            .iter()
            .filter(|r| r.symbol == symbol)
            .map(|r| r.span)
            .collect();
        spans.sort_by_key(|s| (s.file != declared.file, *s != declared, s.file, s.start));
        spans.dedup();
        spans
    }
}

/// What a name the checker resolved refers to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Noted {
    /// A local, by its declaration's span.
    Local(Span),
    Item(Item),
    Field(TypeId, usize),
    Variant(TypeId, u32),
    /// An import alias of a module.
    Alias(ModuleId, String),
}

impl<'a> Checker<'a> {
    /// Remembers what the name at `span` refers to, when indexing.
    pub(crate) fn note(&mut self, span: Span, noted: Noted) {
        if self.indexing {
            self.noted.push((span, noted));
        }
    }

    /// Notes `alias` as a use of an import of `module`, when it is one.
    pub(crate) fn note_alias(&mut self, module: ModuleId, alias: &ast::Ident) {
        if self.indexing && self.import(module, &alias.name).is_some() {
            self.note(alias.span, Noted::Alias(module, alias.name.clone()));
        }
    }

    fn shown(&self, ty: &Type) -> String {
        self.unifier.resolve_for_display(ty).to_string()
    }

    fn function_detail(&self, item: Item) -> String {
        match item {
            Item::Function(id) => {
                let entry = &self.functions[id.0 as usize];
                let Some(signature) = &entry.signature else {
                    return format!("function {}(...)", entry.name);
                };
                let ast = entry.ast;
                let params: Vec<String> = signature
                    .params
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| {
                        let mode = match signature.explicit_modes.get(index).copied().flatten() {
                            Some(ast::ParamMode::Update) => "update ",
                            Some(ast::ParamMode::Own) => "own ",
                            _ => "",
                        };
                        let name = signature
                            .param_names
                            .get(index)
                            .cloned()
                            .unwrap_or_default();
                        let default = ast
                            .and_then(|f| f.params.get(index))
                            .is_some_and(|p| p.default.is_some());
                        format!(
                            "{mode}{name}: {}{}",
                            self.shown(ty),
                            if default { " = …" } else { "" }
                        )
                    })
                    .collect();
                let generics = ast
                    .map(|f| {
                        f.generics
                            .iter()
                            .map(|g| g.name.as_str())
                            .collect::<Vec<_>>()
                    })
                    .filter(|g| !g.is_empty())
                    .map(|g| format!("<{}>", g.join(", ")))
                    .unwrap_or_default();
                let ret = self.unifier.resolve_for_display(&signature.ret);
                let unsafe_word = if ast.is_some_and(|f| f.is_unsafe) {
                    "unsafe "
                } else {
                    ""
                };
                format!(
                    "{unsafe_word}function {}{generics}({}){}",
                    entry.name,
                    params.join(", "),
                    if ret == Type::Nothing {
                        String::new()
                    } else {
                        format!(" -> {ret}")
                    }
                )
            }
            Item::Foreign(id) => {
                let entry = &self.foreign[id.0 as usize];
                let params: Vec<String> = entry
                    .ast
                    .params
                    .iter()
                    .zip(&entry.params)
                    .map(|(param, ty)| format!("{}: {}", param.name.name, self.shown(ty)))
                    .collect();
                let variadic = if entry.ast.variadic { ", ..." } else { "" };
                format!(
                    "foreign \"{}\" function {}({}{variadic}){}",
                    entry.library,
                    entry.ast.name.name,
                    params.join(", "),
                    if entry.ret == Type::Nothing {
                        String::new()
                    } else {
                        format!(" -> {}", self.shown(&entry.ret))
                    }
                )
            }
            Item::Constant(id) => {
                let entry = &self.constants[id.0 as usize];
                match &entry.ty {
                    Some(ty) => format!("constant {}: {}", entry.decl.name.name, self.shown(ty)),
                    None => format!("constant {}", entry.decl.name.name),
                }
            }
            Item::Type(id) => self.type_detail(id),
        }
    }

    fn type_detail(&self, id: TypeId) -> String {
        let entry = &self.types[id.0 as usize];
        let generics = if entry.generics.is_empty() {
            String::new()
        } else {
            format!("<{}>", entry.generics.join(", "))
        };
        let def = &self.type_defs[id.0 as usize];
        let fields = |fields: &[crate::types::FieldDef]| -> String {
            fields
                .iter()
                .map(|f| format!("{}: {}", f.name, f.ty))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if def.is_enum() {
            let variants: Vec<String> = def
                .variants()
                .iter()
                .map(|v| {
                    if v.fields.is_empty() {
                        v.name.clone()
                    } else {
                        format!("{}({})", v.name, fields(&v.fields))
                    }
                })
                .collect();
            format!("enum {}{generics}: {}", entry.name, variants.join(", "))
        } else {
            let foreign = if matches!(entry.source, TypeSource::Foreign(_)) {
                "foreign "
            } else {
                ""
            };
            format!(
                "{foreign}type {}{generics} = [{}]",
                entry.name,
                fields(def.fields())
            )
        }
    }

    fn field_params(&self, fields: &[crate::types::FieldDef]) -> Vec<Param> {
        fields
            .iter()
            .map(|f| Param {
                name: f.name.clone(),
                ty: f.ty.to_string(),
                default: false,
                mode: None,
                owner: self.owner_of(&f.ty),
            })
            .collect()
    }

    /// What a call of a definition takes, and what it is or gives.
    fn item_shape(&self, item: Item) -> (Vec<Param>, Option<String>) {
        match item {
            Item::Function(id) => {
                let entry = &self.functions[id.0 as usize];
                let Some(signature) = &entry.signature else {
                    return (Vec::new(), None);
                };
                let params = signature
                    .params
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| Param {
                        name: signature
                            .param_names
                            .get(index)
                            .cloned()
                            .unwrap_or_default(),
                        ty: self.shown(ty),
                        default: entry
                            .ast
                            .and_then(|f| f.params.get(index))
                            .is_some_and(|p| p.default.is_some()),
                        mode: match signature.explicit_modes.get(index).copied().flatten() {
                            Some(ast::ParamMode::Update) => Some("update"),
                            Some(ast::ParamMode::Own) => Some("own"),
                            _ => None,
                        },
                        owner: self.owner_of(ty),
                    })
                    .collect();
                (params, Some(self.shown(&signature.ret)))
            }
            Item::Foreign(id) => {
                let entry = &self.foreign[id.0 as usize];
                let params = entry
                    .ast
                    .params
                    .iter()
                    .zip(&entry.params)
                    .map(|(param, ty)| Param {
                        name: param.name.name.clone(),
                        ty: self.shown(ty),
                        default: false,
                        mode: None,
                        owner: self.owner_of(ty),
                    })
                    .collect();
                (params, Some(self.shown(&entry.ret)))
            }
            Item::Constant(id) => (
                Vec::new(),
                self.constants[id.0 as usize]
                    .ty
                    .as_ref()
                    .map(|ty| self.shown(ty)),
            ),
            Item::Type(id) => {
                let def = &self.type_defs[id.0 as usize];
                let params = if def.is_enum() {
                    Vec::new()
                } else {
                    self.field_params(def.fields())
                };
                (params, Some(self.types[id.0 as usize].name.clone()))
            }
        }
    }

    fn module_standard(&self, module: ModuleId) -> bool {
        self.modules
            .get(module.0 as usize)
            .is_some_and(|m| m.standard)
    }

    /// Gathers the index from what the checker noted and declared.
    pub(crate) fn build_index(&self) -> Index {
        let mut index = Index::default();
        let mut keys: HashMap<Noted, usize> = HashMap::new();
        fn add(
            index: &mut Index,
            keys: &mut HashMap<Noted, usize>,
            key: Noted,
            symbol: Symbol,
        ) -> usize {
            *keys.entry(key).or_insert_with(|| {
                index.symbols.push(symbol);
                index.symbols.len() - 1
            })
        }
        // The definitions of each module, and their members.
        for (module_index, module) in self.modules.iter().enumerate() {
            let module_id = ModuleId(module_index as u32);
            let mut scope = Vec::new();
            let mut public = Vec::new();
            let mut items: Vec<(&String, &Item)> = module.items.iter().collect();
            items.sort_by_key(|(name, _)| *name);
            for (name, item) in items {
                let (kind, span) = match *item {
                    Item::Function(id) => {
                        (SymbolKind::Function, self.functions[id.0 as usize].span)
                    }
                    Item::Foreign(id) => (
                        SymbolKind::Function,
                        self.foreign[id.0 as usize].ast.name.span,
                    ),
                    Item::Constant(id) => {
                        (SymbolKind::Constant, self.constants[id.0 as usize].span)
                    }
                    Item::Type(id) => (SymbolKind::Type, self.types[id.0 as usize].span),
                };
                let members = match *item {
                    Item::Constant(id) => self.constants[id.0 as usize]
                        .ty
                        .as_ref()
                        .and_then(|ty| self.owner_of(ty)),
                    // An enum's variants follow a dot after it.
                    Item::Type(id) => Some(Owner::Type(id.0)),
                    _ => None,
                };
                let (params, ty) = self.item_shape(*item);
                let symbol = add(
                    &mut index,
                    &mut keys,
                    Noted::Item(*item),
                    Symbol {
                        name: name.clone(),
                        kind,
                        span,
                        detail: self.function_detail(*item),
                        target: None,
                        members,
                        standard: module.standard,
                        ty,
                        params,
                    },
                );
                scope.push(symbol);
                if !self.item_is_private(*item) {
                    public.push((name.clone(), symbol));
                }
            }
            let mut imports: Vec<(&String, &(ModuleId, Span))> = module.imports.iter().collect();
            imports.sort_by_key(|(name, _)| *name);
            for (alias, (target, span)) in imports {
                let target_module = &self.modules[target.0 as usize];
                let symbol = add(
                    &mut index,
                    &mut keys,
                    Noted::Alias(module_id, alias.clone()),
                    Symbol {
                        name: alias.clone(),
                        kind: SymbolKind::Module,
                        span: *span,
                        detail: format!("{alias} = import(\"{}\")", target_module.name),
                        target: Some(target_module.file),
                        members: Some(Owner::Module(target.0)),
                        standard: module.standard,
                        ty: None,
                        params: Vec::new(),
                    },
                );
                scope.push(symbol);
            }
            index.scopes.insert(module.file, scope);
            index
                .members
                .insert(Owner::Module(module_index as u32), public);
        }
        // Each type's fields, variants, and methods.
        for (id, entry) in self.types.iter().enumerate() {
            let type_id = TypeId(id as u32);
            let standard = self.module_standard(entry.module);
            let def = &self.type_defs[id];
            let mut members = Vec::new();
            match entry.source {
                TypeSource::Structure(fields) | TypeSource::Foreign(fields) => {
                    for (field_index, field) in fields.iter().enumerate() {
                        let ty = def.fields().get(field_index).map(|f| f.ty.to_string());
                        let symbol = add(
                            &mut index,
                            &mut keys,
                            Noted::Field(type_id, field_index),
                            Symbol {
                                name: field.name.name.clone(),
                                kind: SymbolKind::Field,
                                span: field.name.span,
                                detail: format!(
                                    "{}.{}: {}",
                                    entry.name,
                                    field.name.name,
                                    ty.unwrap_or_else(|| "?".to_string())
                                ),
                                target: None,
                                members: def
                                    .fields()
                                    .get(field_index)
                                    .and_then(|f| self.owner_of(&f.ty)),
                                standard,
                                ty: def.fields().get(field_index).map(|f| f.ty.to_string()),
                                params: Vec::new(),
                            },
                        );
                        members.push((field.name.name.clone(), symbol));
                    }
                }
                TypeSource::Enum(variants) => {
                    for (variant_index, variant) in variants.iter().enumerate() {
                        let fields = def
                            .variants()
                            .get(variant_index)
                            .map(|v| {
                                v.fields
                                    .iter()
                                    .map(|f| format!("{}: {}", f.name, f.ty))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        let detail = if fields.is_empty() {
                            format!("{}.{}", entry.name, variant.name.name)
                        } else {
                            format!(
                                "{}.{}({})",
                                entry.name,
                                variant.name.name,
                                fields.join(", ")
                            )
                        };
                        let symbol = add(
                            &mut index,
                            &mut keys,
                            Noted::Variant(type_id, variant_index as u32),
                            Symbol {
                                name: variant.name.name.clone(),
                                kind: SymbolKind::Variant,
                                span: variant.name.span,
                                detail,
                                target: None,
                                members: None,
                                standard,
                                ty: Some(entry.name.clone()),
                                params: def
                                    .variants()
                                    .get(variant_index)
                                    .map(|v| self.field_params(&v.fields))
                                    .unwrap_or_default(),
                            },
                        );
                        members.push((variant.name.name.clone(), symbol));
                    }
                }
                TypeSource::Outcome => {}
            }
            // Methods: the functions of the type's module that take it first.
            let prefix = format!("{}_", snake_case(&entry.name));
            if let Some(module) = self.modules.get(entry.module.0 as usize) {
                for (name, item) in &module.items {
                    if let Item::Function(function) = item
                        && self.method_receiver(*function) == Some(type_id)
                        && let Some(&symbol) = keys.get(&Noted::Item(*item))
                    {
                        let written = name.strip_prefix(&prefix).unwrap_or(name);
                        members.push((written.to_string(), symbol));
                    }
                }
            }
            members.sort_by(|a, b| a.0.cmp(&b.0));
            index.members.insert(Owner::Type(id as u32), members);
        }
        // The locals of each function body, the script's among them.
        for entry in &self.functions {
            let Some(function) = &entry.hir else {
                continue;
            };
            let standard = self.module_standard(entry.module);
            let file = self
                .modules
                .get(entry.module.0 as usize)
                .map(|m| m.file)
                .unwrap_or(function.span.file);
            let mut locals = Vec::new();
            for local in &function.locals {
                if local.kind == LocalKind::Hidden || local.name.starts_with('<') {
                    continue;
                }
                let kind = match local.kind {
                    LocalKind::Param => SymbolKind::Parameter,
                    LocalKind::Constant => SymbolKind::Constant,
                    _ => SymbolKind::Variable,
                };
                let symbol = add(
                    &mut index,
                    &mut keys,
                    Noted::Local(local.span),
                    Symbol {
                        name: local.name.clone(),
                        kind,
                        span: local.span,
                        detail: format!("{}: {}", local.name, self.shown(&local.ty)),
                        target: None,
                        members: self.owner_of(&local.ty),
                        standard,
                        ty: Some(self.shown(&local.ty)),
                        params: Vec::new(),
                    },
                );
                locals.push(symbol);
            }
            index.locals.push(LocalScope {
                file,
                body: entry.ast.map(|f| f.body.span),
                locals,
            });
        }
        // Every name as written, declarations among them.
        let mut seen: HashSet<Reference> = HashSet::new();
        for (symbol_index, symbol) in index.symbols.iter().enumerate() {
            seen.insert(Reference {
                span: symbol.span,
                symbol: symbol_index,
            });
        }
        for (span, noted) in &self.noted {
            let key = match noted {
                Noted::Local(declared) => Noted::Local(*declared),
                other => other.clone(),
            };
            if let Some(&symbol) = keys.get(&key) {
                seen.insert(Reference {
                    span: *span,
                    symbol,
                });
            }
        }
        let mut references: Vec<Reference> = seen.into_iter().collect();
        references.sort_by_key(|r| (r.span.file, r.span.start, r.span.end, r.symbol));
        index.references = references;
        index
    }

    /// The type whose members follow a dot after a value of `ty`.
    fn owner_of(&self, ty: &Type) -> Option<Owner> {
        match self.unifier.resolve(ty) {
            Type::Named(named) => Some(Owner::Type(named.id.0)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModuleSource;

    /// The index of a one-file program.
    fn index_of(text: &str) -> Index {
        let parsed = sayon_syntax::parser::parse_source(FileId(0), text);
        let source = ModuleSource {
            ast: &parsed.module,
            name: "test.sayon".to_string(),
            imports: Vec::new(),
            standard: false,
            dependency: false,
        };
        let span = Span::new(FileId(0), 0, 0);
        crate::check_program_indexed(&[source], 0, span, sayon_target::host(), true)
            .index
            .expect("an index")
    }

    /// The symbol written at the `nth` place `needle` is, counting from 0.
    fn symbol<'i>(index: &'i Index, text: &str, needle: &str, nth: usize) -> &'i Symbol {
        let offset = text
            .match_indices(needle)
            .nth(nth)
            .unwrap_or_else(|| panic!("`{needle}` {nth}"))
            .0;
        let reference = index
            .at(FileId(0), offset as u32)
            .unwrap_or_else(|| panic!("nothing at `{needle}` {nth}"));
        &index.symbols[reference.symbol]
    }

    const PROGRAM: &str = "type Point = [x: integer, y: integer]

function norm(p: Point) -> integer
    return p.x * p.x + p.y * p.y

function point_twice(p: Point) -> Point
    return Point(x = p.x * 2, y = p.y * 2)

start = Point(x = 3, y = 4)
print(norm(start), start.twice().x)
missing(start)
";

    #[test]
    fn names_refer_to_their_declarations() {
        let index = index_of(PROGRAM);
        let p = symbol(&index, PROGRAM, "p.x", 0);
        assert_eq!(
            (p.kind, p.detail.as_str()),
            (SymbolKind::Parameter, "p: Point")
        );
        let x = symbol(&index, PROGRAM, "x * p", 0);
        assert_eq!(x.kind, SymbolKind::Field);
        assert_eq!(x.detail, "Point.x: integer");
        // The field as declared, as read, and as structures are built.
        let field = index
            .at(FileId(0), PROGRAM.find("x:").unwrap() as u32)
            .unwrap()
            .symbol;
        assert_eq!(index.references_to(field).len(), 7);
        let norm = symbol(&index, PROGRAM, "norm(start)", 0);
        assert_eq!(norm.detail, "function norm(p: Point) -> integer");
        // A method call names the function it calls.
        let twice = symbol(&index, PROGRAM, "twice()", 0);
        assert_eq!(
            (twice.name.as_str(), twice.kind),
            ("point_twice", SymbolKind::Function)
        );
        let start = symbol(&index, PROGRAM, "start)", 1);
        assert_eq!(start.detail, "start: Point");
        assert_eq!(start.members, Some(Owner::Type(point_type(&index))));
        let members: Vec<&str> = index.members[&start.members.unwrap()]
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        // `norm` takes a `Point` first, so it is a method too.
        assert_eq!(members, vec!["norm", "twice", "x", "y"]);
    }

    /// The type whose members are the fields `x` and `y`.
    fn point_type(index: &Index) -> u32 {
        index
            .members
            .iter()
            .find_map(|(owner, members)| match owner {
                Owner::Type(id) if members.iter().any(|(m, _)| m == "x") => Some(*id),
                _ => None,
            })
            .expect("the type")
    }
}
