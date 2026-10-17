//! Embeds the release archive the installer unpacks, named by the
//! `SAYON_INSTALLER_PAYLOAD` environment variable (`scripts/package.ps1`
//! sets it). Without it the installer is built empty and says so when run.
//!
//! On Windows it also embeds a manifest that runs the installer without
//! administrator rights (Windows otherwise asks to elevate any program named
//! "setup"), and, when a resource compiler is available, the Sayon icon and
//! version information.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-env-changed=SAYON_INSTALLER_PAYLOAD");
    let payload = match std::env::var("SAYON_INSTALLER_PAYLOAD") {
        Ok(path) if !path.is_empty() => {
            let path = std::fs::canonicalize(&path)
                .unwrap_or_else(|e| panic!("SAYON_INSTALLER_PAYLOAD `{path}`: {e}"));
            println!("cargo:rerun-if-changed={}", path.display());
            format!("include_bytes!({:?})", path.display().to_string())
        }
        _ => "&[]".to_string(),
    };
    std::fs::write(
        out_dir.join("payload.rs"),
        format!("/// The release archive to install.\npub static PAYLOAD: &[u8] = {payload};\n"),
    )
    .expect("write payload.rs");
    println!(
        "cargo:rustc-env=SAYON_INSTALLER_TARGET={}",
        std::env::var("TARGET").unwrap()
    );

    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !windows || !msvc {
        return;
    }
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='asInvoker'");
    resources(&out_dir);
}

fn resources(out_dir: &Path) {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir
        .join("..")
        .join("..")
        .join("assets")
        .join("sayon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-env-changed=SAYON_RC");
    let Some(compiler) = find_resource_compiler() else {
        println!(
            "cargo:warning=no resource compiler (llvm-rc or rc.exe) found; the installer will have no icon"
        );
        return;
    };
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
      VALUE "FileDescription", "Sayon setup"
      VALUE "ProductName", "Sayon"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "sayon-setup.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let rc_path = out_dir.join("installer.rc");
    let res_path = out_dir.join("installer.res");
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
        _ => println!(
            "cargo:warning=compiling the icon resource failed; the installer will have no icon"
        ),
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
