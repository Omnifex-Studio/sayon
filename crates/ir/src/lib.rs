//! Intermediate representation shared by the interpreter and the native backend.

pub mod ir;
pub mod lower;

pub use ir::*;
pub use lower::{LowerOutput, lower};
