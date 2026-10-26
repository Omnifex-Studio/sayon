//! Runtime support for Sayon programs.
//!
//! `text` holds the display and parsing rules shared with the interpreter;
//! `ffi` holds the functions compiled programs call. Output, clocks, and
//! ending the program go through `console` and `clock`, which a web page's
//! host provides.

/// Writes a line to standard errors, like `eprintln!`, but through
/// `console`, so that a web page shows it too.
macro_rules! report {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!($crate::console::stderr(), $($arg)*);
    }};
}

#[cfg(target_os = "android")]
pub mod android;
pub mod callbacks;
pub mod clock;
pub mod console;
pub mod encodings;
pub mod fetch;
pub mod ffi;
pub mod finally;
pub mod font;
mod font_table;
pub mod graphemes;
pub mod graphics;
pub mod host;
#[cfg(target_os = "android")]
mod jni;
pub mod network;
pub mod parallel;
pub mod png;
pub mod processes;
pub mod streams;
pub mod tasks;
pub mod text;
pub mod tls;
pub mod unicode_tables;
pub mod window;
pub mod zones;
