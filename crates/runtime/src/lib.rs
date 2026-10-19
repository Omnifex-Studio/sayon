//! Runtime support for Sayon programs.
//!
//! `text` holds the display and parsing rules shared with the interpreter;
//! `ffi` holds the functions compiled programs call.

pub mod encodings;
pub mod ffi;
pub mod graphemes;
pub mod host;
pub mod network;
pub mod processes;
pub mod streams;
pub mod tasks;
pub mod text;
pub mod unicode_tables;
pub mod zones;
