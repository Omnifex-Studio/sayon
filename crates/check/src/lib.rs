//! Semantic analysis for Sayon: name resolution, type inference with
//! flow-sensitive union narrowing, and ownership checking.

pub mod builtins;
mod checker;
mod closures;
mod driver;
mod expr;
pub mod hir;
pub mod infer;
pub mod lint;
mod narrow;
mod ownership;
mod patterns;
mod stmt;
mod structures;
pub mod types;

use checker::{Checker, ModuleInfo};
use sayon_syntax::ast;
use sayon_syntax::{Diagnostics, Span};
use std::collections::HashMap;

pub use checker::{CheckOutput, ModuleId};

/// One parsed source file of a program, with its imports already resolved.
pub struct ModuleSource<'a> {
    pub ast: &'a ast::Module,
    /// The import path or file name used in messages.
    pub name: String,
    /// `(alias, alias span, index of the imported module)` for each import.
    pub imports: Vec<(String, Span, usize)>,
    /// Standard library modules may use host primitives.
    pub standard: bool,
    /// Part of a dependency package, which `sayon lint` leaves alone.
    pub dependency: bool,
}

/// Checks a single-file program.
pub fn check_module(module: &ast::Module, span: Span) -> CheckOutput {
    let source = ModuleSource {
        ast: module,
        name: String::new(),
        imports: Vec::new(),
        standard: false,
        dependency: false,
    };
    check_program(&[source], 0, span)
}

/// Checks a program made of several modules; `entry` is the index of the
/// module that runs.
pub fn check_program(modules: &[ModuleSource], entry: usize, span: Span) -> CheckOutput {
    let mut checker = Checker::new();
    checker.entry_module = ModuleId(entry as u32);
    for (index, module) in modules.iter().enumerate() {
        let imports: HashMap<String, (ModuleId, Span)> = module
            .imports
            .iter()
            .map(|(alias, alias_span, target)| {
                (alias.clone(), (ModuleId(*target as u32), *alias_span))
            })
            .collect();
        checker.modules.push(ModuleInfo {
            name: module.name.clone(),
            items: HashMap::new(),
            imports,
            standard: module.standard,
            entry: index == entry,
        });
    }
    let mut script = Vec::new();
    for (index, module) in modules.iter().enumerate() {
        let statements = checker.collect_items(ModuleId(index as u32), module.ast);
        if index == entry {
            script = statements;
        }
    }
    checker.resolve_type_defs();
    checker.resolve_foreign();
    let order = checker.dependency_order();
    let mut init_order = Vec::new();
    for component in &order {
        checker.check_component(component, &mut init_order);
    }
    let entry = checker.check_entry(&script, span);
    let tests = checker.check_tests();

    let mut diagnostics = std::mem::take(&mut checker.diagnostics);
    if diagnostics.has_errors() {
        diagnostics.sort();
        return CheckOutput {
            program: None,
            diagnostics,
        };
    }

    let functions = checker
        .functions
        .iter_mut()
        .map(|entry| entry.hir.take().expect("checked function"))
        .collect();
    let constants = checker
        .constants
        .iter_mut()
        .map(|entry| hir::Constant {
            name: entry.decl.name.name.clone(),
            span: entry.span,
            ty: entry.ty.clone().unwrap_or(types::Type::Unknown),
            value: entry.hir.take().expect("checked constant"),
            locals: Vec::new(),
        })
        .collect();
    let json = checker.json_support();
    let foreign = checker.foreign_functions();
    let finally = checker.finally_functions();
    let mut program = hir::Program {
        functions,
        constants,
        init_order,
        entry,
        tests,
        types: std::mem::take(&mut checker.type_defs),
        json,
        foreign,
        finally,
    };

    let components = checker.scc_order.clone();
    let ownership = ownership::check_program(&mut program, &components);
    let has_errors = ownership.iter().any(|d| d.is_error());
    diagnostics.extend(ownership);
    diagnostics.sort();
    CheckOutput {
        program: if has_errors { None } else { Some(program) },
        diagnostics,
    }
}

/// Convenience used by tests and tools: parse and check one source text.
pub fn check_source(file: sayon_syntax::FileId, text: &str) -> (Option<hir::Program>, Diagnostics) {
    let parsed = sayon_syntax::parser::parse_source(file, text);
    let mut diagnostics = Diagnostics::new();
    diagnostics.extend(parsed.diagnostics);
    if diagnostics.has_errors() {
        return (None, diagnostics);
    }
    let span = Span::new(file, 0, 0);
    let output = check_module(&parsed.module, span);
    diagnostics.extend(output.diagnostics);
    (output.program, diagnostics)
}
