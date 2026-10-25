//! Sayon functions that C calls back, during the call of a C function they
//! were given to.
//!
//! Generated code gives a C function, for each of its parameters that is a
//! function, the address of a function made for that parameter, which C
//! calls with C's convention. Before the call, it pushes a frame here with
//! the function values of those parameters; the function made for a
//! parameter finds its value in the newest frame of its C function, so a
//! function literal goes to C with what it captured. A function that C
//! calls cannot wait, since its task cannot be left with C's frames on its
//! stack, and its failure cannot pass through C: the first one is kept, C
//! gets 0 from that call and from every later one, and the call of the C
//! function fails with the error once C returns.

// The functions called by generated code rely on the same conventions as
// those of `ffi`, where their contracts are described.
#![allow(clippy::missing_safety_doc)]

use crate::ffi::{SayonError, sayon_error_drop};
use std::cell::RefCell;

/// The function values of one call of a C function.
struct Frame {
    function: u32,
    /// The address of each parameter's function value, by position; null
    /// for the parameters that are not functions.
    values: *const *const u8,
    /// The first error of a function C called.
    failed: *mut SayonError,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// Starts a call of the C function `function` with the function values at
/// `values`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_callbacks_enter(function: i32, values: *const *const u8) {
    FRAMES.with(|frames| {
        frames.borrow_mut().push(Frame {
            function: function as u32,
            values,
            failed: std::ptr::null_mut(),
        })
    });
    crate::tasks::count_callbacks(1);
}

fn newest<R>(function: i32, f: impl FnOnce(&mut Frame) -> R) -> R {
    FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        let Some(frame) = frames
            .iter_mut()
            .rev()
            .find(|frame| frame.function == function as u32)
        else {
            report!(
                "fatal error: C called a Sayon function after the call it was given to returned, or on another thread"
            );
            std::process::abort();
        };
        f(frame)
    })
}

/// The function value of parameter `param` of the newest call of the C
/// function `function`, or null once one of its functions failed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_callbacks_function(function: i32, param: i32) -> *const u8 {
    newest(function, |frame| {
        if frame.failed.is_null() {
            // SAFETY: the frame's values cover the function's parameters.
            unsafe { *frame.values.add(param as usize) }
        } else {
            std::ptr::null()
        }
    })
}

/// Keeps the error a function that C called failed with.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_callbacks_failed(function: i32, err: *mut *mut SayonError) {
    newest(function, |frame| {
        // SAFETY: the function's error slot.
        unsafe {
            if frame.failed.is_null() {
                frame.failed = *err;
                *err = std::ptr::null_mut();
            } else {
                sayon_error_drop(err);
            }
        }
    })
}

/// Ends the call: 1, with the error moved into the slot, when a function C
/// called failed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_callbacks_leave(err: *mut *mut SayonError) -> i32 {
    let frame = FRAMES
        .with(|frames| frames.borrow_mut().pop())
        .expect("the frame of a call of a C function");
    crate::tasks::count_callbacks(-1);
    if frame.failed.is_null() {
        return 0;
    }
    // SAFETY: the caller's error slot.
    unsafe {
        if !(*err).is_null() {
            sayon_error_drop(err);
        }
        *err = frame.failed;
    }
    1
}
