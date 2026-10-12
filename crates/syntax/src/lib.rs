//! Front end of the Sayon toolchain: source management, diagnostics,
//! tokenization, and parsing.

pub mod ast;
pub mod diagnostic;
pub mod format;
pub mod lexer;
pub mod parser;
pub mod source;
pub mod token;

pub use diagnostic::{Diagnostic, Diagnostics, Severity};
pub use source::{FileId, SourceFile, SourceMap, Span};
