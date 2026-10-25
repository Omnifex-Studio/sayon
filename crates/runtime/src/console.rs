//! The program's standard output and errors, how it ends at once, and the
//! options its host sets: the process's own, or in a web page (a WebAssembly
//! module with no system beneath it) what the page's generated glue gives
//! through the host functions of the import module `sayon`. In an Android
//! app, the output and errors go to Android's log (see `android`).

#[cfg(not(any(
    all(target_family = "wasm", target_os = "unknown"),
    target_os = "android"
)))]
pub use std::io::{stderr, stdout};

/// Ends the program at once with `status`, without cleanup.
#[cfg(not(any(
    all(target_family = "wasm", target_os = "unknown"),
    target_os = "android"
)))]
pub fn exit(status: i32) -> ! {
    std::process::exit(status)
}

#[cfg(target_os = "android")]
pub fn stdout() -> crate::android::Stream {
    crate::android::Stream(1)
}

#[cfg(target_os = "android")]
pub fn stderr() -> crate::android::Stream {
    crate::android::Stream(2)
}

/// Ends the program at once with `status`, without cleanup. In an app, the
/// program's activity finishes, and its thread waits for the process to
/// end with it.
#[cfg(target_os = "android")]
pub fn exit(status: i32) -> ! {
    if !crate::android::in_app() {
        std::process::exit(status)
    }
    crate::android::ended(status);
    loop {
        std::thread::park();
    }
}

/// Whether the host sets the option `name`: an environment variable here.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn option(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub use web::{exit, option, stderr, stdout};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) mod web {
    use std::io::Write;

    #[link(wasm_import_module = "sayon")]
    unsafe extern "C" {
        /// Writes UTF-8 text to the page's output (1) or errors (2).
        fn host_write(stream: i32, text: *const u8, length: usize);
        /// Ends the program: the glue stops running it, with `status`.
        fn host_exit(status: i32) -> !;
        /// Whether the glue was given the option of this name.
        fn host_option(name: *const u8, length: usize) -> i32;
        /// Milliseconds on a clock that never goes back.
        fn host_monotonic() -> f64;
        /// Milliseconds since the start of 1970, in UTC.
        fn host_unix_time() -> f64;
        /// Fills bytes with random ones from the host's secure source.
        fn host_random(bytes: *mut u8, length: usize);
    }

    /// The page's output or errors, unbuffered: the glue buffers lines.
    pub struct HostStream(i32);

    impl HostStream {
        /// Like a locked standard stream: the same stream.
        pub fn lock(&self) -> HostStream {
            HostStream(self.0)
        }
    }

    impl Write for HostStream {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            // SAFETY: the bytes are readable for their length.
            unsafe { host_write(self.0, bytes.as_ptr(), bytes.len()) };
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    pub fn stdout() -> HostStream {
        HostStream(1)
    }

    pub fn stderr() -> HostStream {
        HostStream(2)
    }

    pub fn exit(status: i32) -> ! {
        // SAFETY: the glue stops the program; this never returns.
        unsafe { host_exit(status) }
    }

    pub fn option(name: &str) -> bool {
        // SAFETY: the name is readable for its length.
        unsafe { host_option(name.as_ptr(), name.len()) != 0 }
    }

    pub fn monotonic_milliseconds() -> f64 {
        // SAFETY: the host function only reads its clock.
        unsafe { host_monotonic() }
    }

    pub fn unix_milliseconds() -> f64 {
        // SAFETY: the host function only reads its clock.
        unsafe { host_unix_time() }
    }

    pub fn random_bytes(bytes: &mut [u8]) {
        // SAFETY: the bytes are writable for their length.
        unsafe { host_random(bytes.as_mut_ptr(), bytes.len()) }
    }
}
