//! Runtime support for Sayon programs.
//!
//! `text` holds the display and parsing rules shared with the interpreter;
//! `ffi` holds the functions compiled programs call.

pub mod ffi;
pub mod text;
