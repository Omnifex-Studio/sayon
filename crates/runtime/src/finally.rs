//! The calls of `finally function`s, which compiled programs make where a
//! value of their structure is destroyed.

// The functions called by generated code rely on the same conventions as
// those of `ffi`, where their contracts are described.
#![allow(clippy::missing_safety_doc)]

use crate::ffi::SayonError;
use std::ffi::{CStr, c_char};
use std::io::Write;

/// Starts a finally function, which cannot wait.
#[unsafe(no_mangle)]
pub extern "C" fn sayon_finally_enter() {
    crate::tasks::count_finally(1);
}

#[unsafe(no_mangle)]
pub extern "C" fn sayon_finally_leave() {
    crate::tasks::count_finally(-1);
}

/// Ends the program when a finally function fails: the value it was
/// destroying is gone, and nothing is left to handle the error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_finally_failed(err: *mut *mut SayonError, name: *const c_char) {
    let _ = crate::console::stdout().flush();
    // SAFETY: the error the function failed with, and its name as C text.
    unsafe {
        let name = CStr::from_ptr(name).to_string_lossy();
        let error = &**err;
        report!(
            "fatal error: the finally function `{name}` failed: {}: {}",
            error.code,
            error.message
        );
        if let Some(location) = &error.location {
            report!("  --> {location}");
        }
    }
    crate::console::exit(FATAL_STATUS);
}

/// The exit status of a program that cannot go on, as the interpreter
/// gives it.
const FATAL_STATUS: i32 = 3;
