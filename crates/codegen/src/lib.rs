//! Native code generation.
//!
//! The IR is translated to LLVM IR text, which clang compiles and links with
//! the Sayon runtime library into a standalone executable. Going through
//! textual IR keeps the compiler independent of LLVM's C++ libraries; only a
//! clang installation is needed at build time, and nothing is needed to run
//! the produced program.

mod emit;
mod helpers;
mod layout;

pub use emit::Codegen;

use sayon_ir::Module;
use sayon_syntax::SourceMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Produces the LLVM IR text for a program.
pub fn emit_llvm(module: &Module, sources: &SourceMap) -> String {
    Codegen::new(module, sources).emit()
}

/// External tools and libraries needed to link a program.
pub struct Toolchain {
    pub clang: PathBuf,
    pub runtime: PathBuf,
}

const RUNTIME_FILE: &str = if cfg!(windows) {
    "sayon_runtime.lib"
} else {
    "libsayon_runtime.a"
};

impl Toolchain {
    /// Finds clang and the runtime library. `exe_dir` is the directory of the
    /// running `sayon` executable; the runtime is looked up next to it and in
    /// `../lib`. `SAYON_CLANG` and `SAYON_RUNTIME_LIB` override the search.
    pub fn locate(exe_dir: Option<&Path>) -> Result<Toolchain, String> {
        let clang = match std::env::var_os("SAYON_CLANG") {
            Some(path) => PathBuf::from(path),
            None => find_clang().ok_or_else(|| {
                "clang was not found. Native builds use LLVM's clang to compile and link.\n\
                 Install LLVM (https://releases.llvm.org) and make sure clang is on PATH, \
                 or set SAYON_CLANG to the clang executable."
                    .to_string()
            })?,
        };
        let runtime = match std::env::var_os("SAYON_RUNTIME_LIB") {
            Some(path) => PathBuf::from(path),
            None => {
                let candidates: Vec<PathBuf> = exe_dir
                    .map(|dir| {
                        vec![
                            dir.join(RUNTIME_FILE),
                            dir.join("..").join("lib").join(RUNTIME_FILE),
                        ]
                    })
                    .unwrap_or_default();
                candidates.into_iter().find(|path| path.is_file()).ok_or_else(|| {
                    format!(
                        "the Sayon runtime library ({RUNTIME_FILE}) was not found next to the sayon executable \
                         or in ../lib; set SAYON_RUNTIME_LIB to its path"
                    )
                })?
            }
        };
        Ok(Toolchain { clang, runtime })
    }
}

fn find_clang() -> Option<PathBuf> {
    let name = if cfg!(windows) { "clang.exe" } else { "clang" };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if cfg!(windows) {
        let default = PathBuf::from(r"C:\Program Files\LLVM\bin\clang.exe");
        if default.is_file() {
            return Some(default);
        }
    }
    None
}

/// System libraries the runtime needs, per target.
fn system_libraries() -> &'static [&'static str] {
    if cfg!(windows) {
        &[
            "kernel32", "ntdll", "userenv", "ws2_32", "dbghelp", "advapi32", "bcrypt",
        ]
    } else if cfg!(target_os = "macos") {
        &["System"]
    } else {
        &["pthread", "dl", "m"]
    }
}

/// Compiles LLVM IR and links it with the runtime into `output`.
pub fn link(
    toolchain: &Toolchain,
    ir_path: &Path,
    output: &Path,
    optimize: bool,
) -> Result<(), String> {
    let mut command = Command::new(&toolchain.clang);
    command
        .arg(ir_path)
        .arg(&toolchain.runtime)
        .arg("-o")
        .arg(output)
        .arg(if optimize { "-O2" } else { "-O0" })
        .arg("-Wno-override-module");
    if cfg!(windows) {
        command.arg("-fuse-ld=lld");
    }
    for library in system_libraries() {
        command.arg(format!("-l{library}"));
    }
    let result = command
        .output()
        .map_err(|error| format!("could not start {}: {error}", toolchain.clang.display()))?;
    if result.status.success() {
        Ok(())
    } else {
        Err(format!(
            "linking failed ({}):\n{}{}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        ))
    }
}
