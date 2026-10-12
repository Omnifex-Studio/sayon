//! Interpreter for Sayon programs.
//!
//! It runs the same checked, ownership-annotated IR the native backend
//! compiles, so moves, borrows, checked arithmetic, and cleanup behave the
//! same way in both execution modes.

mod machine;
pub mod value;

pub use machine::{MAX_CALL_DEPTH, Machine, RunError};
pub use value::{ErrorValue, Value};
