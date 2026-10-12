//! Semantic analysis for Sayon: name resolution, type inference with
//! flow-sensitive union narrowing, and ownership checking.

pub mod builtins;
mod checker;
mod driver;
mod expr;
pub mod hir;
pub mod infer;
mod narrow;
mod ownership;
mod stmt;
pub mod types;

use checker::Checker;
use sayon_syntax::ast;
use sayon_syntax::{Diagnostics, Span};

pub use checker::CheckOutput;

/// Checks a parsed module and produces the program to execute.
pub fn check_module(module: &ast::Module, span: Span) -> CheckOutput {
    let mut checker = Checker::new();
    let script = checker.collect_items(module);
    let order = checker.dependency_order();
    let mut init_order = Vec::new();
    for component in &order {
        checker.check_component(component, &mut init_order);
    }
    let entry = checker.check_entry(&script, span);

    let mut diagnostics = std::mem::take(&mut checker.diagnostics);
    if diagnostics.has_errors() {
        diagnostics.sort();
        return CheckOutput { program: None, diagnostics };
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
    let mut program = hir::Program { functions, constants, init_order, entry };

    let components = checker.scc_order.clone();
    let ownership = ownership::check_program(&mut program, &components);
    let has_errors = ownership.iter().any(|d| d.is_error());
    diagnostics.extend(ownership);
    diagnostics.sort();
    CheckOutput { program: if has_errors { None } else { Some(program) }, diagnostics }
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
