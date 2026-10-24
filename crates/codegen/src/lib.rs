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
use sayon_target::Target;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Produces the LLVM IR text for a program.
pub fn emit_llvm(module: &Module, sources: &SourceMap) -> String {
    Codegen::new(module, sources).emit()
}

/// External tools and libraries needed to link a program for a target.
pub struct Toolchain {
    pub clang: PathBuf,
    pub runtime: PathBuf,
    pub target: &'static Target,
}

/// The file name of the runtime library for a target.
fn runtime_file(target: &Target) -> &'static str {
    if target.platform == "windows" {
        "sayon_runtime.lib"
    } else {
        "libsayon_runtime.a"
    }
}

/// The files of WASI's C library that a WASI program links with, kept next
/// to the runtime built for WASI: the startup code of a program that runs
/// once, and the library itself.
pub const WASI_C_LIBRARY: &[&str] = &["crt1-command.o", "libc.a"];

impl Toolchain {
    /// Finds clang and the runtime library for `target`. `exe_dir` is the
    /// directory of the running `sayon` executable. This computer's own
    /// runtime is looked up next to it and in `../lib`, and another
    /// target's in `../lib/<triple>`: an installation's `lib`, or for a
    /// development build in `target/<profile>`, `target/lib`.
    /// `SAYON_CLANG` and, for this computer's target, `SAYON_RUNTIME_LIB`
    /// override the search.
    pub fn locate(exe_dir: Option<&Path>, target: &'static Target) -> Result<Toolchain, String> {
        let clang = match std::env::var_os("SAYON_CLANG") {
            Some(path) => PathBuf::from(path),
            None => find_clang().ok_or_else(|| {
                "clang was not found. Native builds use LLVM's clang to compile and link.\n\
                 Install LLVM (https://releases.llvm.org) and make sure clang is on PATH, \
                 or set SAYON_CLANG to the clang executable."
                    .to_string()
            })?,
        };
        let file = runtime_file(target);
        let runtime = match std::env::var_os("SAYON_RUNTIME_LIB") {
            Some(path) if target.is_host() => PathBuf::from(path),
            _ => {
                let candidates: Vec<PathBuf> = exe_dir
                    .map(|dir| {
                        let lib = dir.parent().map(|parent| parent.join("lib"));
                        if target.is_host() {
                            let mut paths = vec![dir.join(file)];
                            paths.extend(lib.map(|lib| lib.join(file)));
                            paths
                        } else {
                            lib.map(|lib| lib.join(target.triple).join(file))
                                .into_iter()
                                .collect()
                        }
                    })
                    .unwrap_or_default();
                candidates.into_iter().find(|path| path.is_file()).ok_or_else(|| {
                    if target.is_host() {
                        format!(
                            "the Sayon runtime library ({file}) was not found next to the sayon executable \
                             or in ../lib; set SAYON_RUNTIME_LIB to its path"
                        )
                    } else {
                        format!(
                            "the Sayon runtime library for {target} ({file}) was not found in ../lib/{} \
                             next to the sayon executable",
                            target.triple
                        )
                    }
                })?
            }
        };
        if target.platform == "wasi" {
            let directory = runtime.parent().unwrap_or(Path::new("."));
            for name in WASI_C_LIBRARY {
                if !directory.join(name).is_file() {
                    return Err(format!(
                        "WASI's C library ({name}) was not found next to the runtime, in {}",
                        directory.display()
                    ));
                }
            }
        }
        Ok(Toolchain {
            clang,
            runtime,
            target,
        })
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

/// System libraries the runtime needs, per target. A WASI program's C
/// library is linked by name, as the files of [`WASI_C_LIBRARY`].
fn system_libraries(target: &Target) -> &'static [&'static str] {
    match target.platform {
        "windows" => &[
            "kernel32", "ntdll", "userenv", "ws2_32", "dbghelp", "advapi32", "bcrypt",
        ],
        "macos" => &["System"],
        "wasi" => &[],
        _ => &["pthread", "dl", "m"],
    }
}

/// The libraries of the C functions a program calls, as the linker knows
/// them; a library none of whose functions are called, as in a platform
/// branch for another system, is left out. The C runtime is linked already,
/// but on Windows its `printf` and kin are in a library of their own.
pub fn foreign_libraries(module: &Module) -> Vec<String> {
    let mut libraries: Vec<String> = Vec::new();
    for id in module.called_foreign() {
        let function = &module.foreign[id.0 as usize];
        let library = if function.library == "c" {
            if module.target.platform != "windows" {
                continue;
            }
            "legacy_stdio_definitions".to_string()
        } else {
            function.library.clone()
        };
        if !libraries.contains(&library) {
            libraries.push(library);
        }
    }
    libraries
}

/// Compiles LLVM IR and links it with the runtime and `libraries` into
/// `output`.
pub fn link(
    toolchain: &Toolchain,
    ir_path: &Path,
    output: &Path,
    optimize: bool,
    libraries: &[String],
) -> Result<(), String> {
    let target = toolchain.target;
    let mut command = Command::new(&toolchain.clang);
    if !target.is_host() {
        command.arg(format!("--target={}", target.triple));
    }
    command.arg(ir_path).arg(&toolchain.runtime);
    if target.platform == "wasi" {
        // Clang's own WASI setup wants a WASI sysroot and its compiler-rt;
        // the C library lies next to the runtime instead, and the runtime
        // holds the compiler's helper functions.
        let directory = toolchain.runtime.parent().unwrap_or(Path::new("."));
        command.arg("-nostdlib");
        for name in WASI_C_LIBRARY {
            command.arg(directory.join(name));
        }
        // The stack lies in the program's memory: 8 MiB, as on Linux.
        command.arg("-Wl,-z,stack-size=8388608");
    }
    command
        .arg("-o")
        .arg(output)
        .arg(if optimize { "-O2" } else { "-O0" })
        .arg("-Wno-override-module");
    if target.platform == "windows" {
        command.arg("-fuse-ld=lld");
    }
    for library in system_libraries(target) {
        command.arg(format!("-l{library}"));
    }
    for library in libraries {
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
