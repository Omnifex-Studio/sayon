//! Embeds the Sayon icon and version information into `sayon.exe` on Windows.
//!
//! The resource compiler is `llvm-rc` (or `rc.exe`), found through
//! `SAYON_RC`, `PATH`, or the default LLVM location. Without one the build
//! continues and the executable simply has no icon.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("..").join("..").join("assets").join("sayon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-env-changed=SAYON_RC");

    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !windows || !msvc {
        return;
    }
    let Some(compiler) = find_resource_compiler() else {
        println!(
            "cargo:warning=no resource compiler (llvm-rc or rc.exe) found; sayon.exe will have no icon"
        );
        return;
    };

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let parts: Vec<&str> = version.split(['.', '-']).take(3).collect();
    let numeric = format!(
        "{},{},{},0",
        parts[0],
        parts.get(1).unwrap_or(&"0"),
        parts.get(2).unwrap_or(&"0")
    );
    let icon_path = icon.display().to_string().replace('\\', "\\\\");
    let script = format!(
        r#"1 ICON "{icon_path}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "The Sayon programming language"
      VALUE "ProductName", "Sayon"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "sayon.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let rc_path = out_dir.join("sayon.rc");
    let res_path = out_dir.join("sayon.res");
    std::fs::write(&rc_path, script).expect("write resource script");
    let status = Command::new(&compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg-bins={}", res_path.display());
        }
        _ => println!("cargo:warning=compiling the icon resource failed; sayon.exe will have no icon"),
    }
}

fn find_resource_compiler() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SAYON_RC") {
        return Some(PathBuf::from(path));
    }
    for name in ["llvm-rc.exe", "rc.exe"] {
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    let default = Path::new(r"C:\Program Files\LLVM\bin\llvm-rc.exe");
    default.is_file().then(|| default.to_path_buf())
}
