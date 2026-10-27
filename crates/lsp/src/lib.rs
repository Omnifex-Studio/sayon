//! The Sayon language server: what `sayon lsp` runs for an editor, which
//! talks to it in the language server protocol over its standard input and
//! output. It checks each open document as the entry of a program, with the
//! texts of the open documents in place of their files, and answers from
//! what the checker resolved (`sayon_check::index`): diagnostics as the
//! documents change, hover, going to a definition, references, renaming,
//! completion and signature help (`complete`), and formatting.

mod complete;
pub mod context;
pub use sayon_driver::json;
mod server;
pub mod text;

pub use server::{Configure, serve};
