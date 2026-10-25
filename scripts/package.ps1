# Builds Sayon in release mode and produces the downloads for this device:
#
#   dist/sayon-<version>-<device>-setup.exe   the installer users download and run
#   dist/sayon-<version>-<device>.zip         the same files as a plain archive
#   dist/SHA256SUMS.txt                       checksums of both
#
# <device> names the operating system and processor, such as windows-x64.
# The archive holds:
#
#   bin/sayon.exe            the command-line tool
#   lib/sayon_runtime.lib    linked into native builds
#   lib/wasm32-wasip1/       the runtime for WASI, with WASI's C library
#   lib/wasm32-unknown-unknown/  the runtime for web pages
#   stdlib/                  the standard library
#   docs/, examples/, editors/vscode/, README.md, INSTALL.txt
#
# Usage: powershell -File scripts/package.ps1 [-OutDir dist]

param([string]$OutDir = "dist")

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$manifest = Get-Content (Join-Path $root "Cargo.toml") -Raw
if ($manifest -notmatch 'version = "([^"]+)"') { throw "cannot find the workspace version in Cargo.toml" }
$version = $Matches[1]
$processor = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x64" }
    "ARM64" { "arm64" }
    default { throw "unsupported processor $($env:PROCESSOR_ARCHITECTURE)" }
}
$device = "windows-$processor"

cargo build --release -p sayon-cli -p sayon-runtime
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
cargo build --release -p sayon-runtime --target wasm32-wasip1
if ($LASTEXITCODE -ne 0) { throw "building the runtime for WASI failed" }
cargo build --release -p sayon-runtime --target wasm32-unknown-unknown
if ($LASTEXITCODE -ne 0) { throw "building the runtime for web pages failed" }
# WASI's C library (wasi-libc), as the Rust toolchain ships it.
$sysroot = (rustc --print sysroot).Trim()
$wasiLibc = Join-Path $sysroot "lib/rustlib/wasm32-wasip1/lib/self-contained"

New-Item -ItemType Directory -Force $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
$name = "sayon-$version-$device"
$stage = Join-Path $OutDir $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
foreach ($dir in "bin", "lib", "stdlib", "docs", "examples", "editors", "assets") {
    New-Item -ItemType Directory -Force (Join-Path $stage $dir) | Out-Null
}

Copy-Item "target/release/sayon.exe" (Join-Path $stage "bin")
Copy-Item "target/release/sayon_runtime.lib" (Join-Path $stage "lib")
$wasi = Join-Path $stage "lib/wasm32-wasip1"
New-Item -ItemType Directory -Force $wasi | Out-Null
Copy-Item "target/wasm32-wasip1/release/libsayon_runtime.a" $wasi
Copy-Item (Join-Path $wasiLibc "crt1-command.o"), (Join-Path $wasiLibc "libc.a") $wasi
$notice = @"
crt1-command.o and libc.a are wasi-libc, the C library of WASI programs,
as the Rust toolchain ships it. WebAssembly programs that Sayon builds for
WASI include parts of it. Its license, which allows this, is at
https://github.com/WebAssembly/wasi-libc/blob/main/LICENSE
"@
Set-Content -Path (Join-Path $wasi "NOTICE.txt") -Value $notice -Encoding utf8
$web = Join-Path $stage "lib/wasm32-unknown-unknown"
New-Item -ItemType Directory -Force $web | Out-Null
Copy-Item "target/wasm32-unknown-unknown/release/libsayon_runtime.a" $web
Copy-Item "stdlib/*.sayon" (Join-Path $stage "stdlib")
Copy-Item "docs/*.md" (Join-Path $stage "docs")
Copy-Item "examples/*.sayon" (Join-Path $stage "examples")
Copy-Item "assets/logo.png" (Join-Path $stage "assets")
Copy-Item -Recurse "editors/vscode" (Join-Path $stage "editors")
Copy-Item "README.md" $stage

$install = @"
Sayon $version for Windows ($processor)

The easiest way to install Sayon is the installer, $name-setup.exe:
run it and answer the question it asks. It installs for your user account
(no administrator rights needed), makes ``sayon`` available in new
terminals, and can be removed from Settings > Apps.

To use this archive instead:

1. Put this folder somewhere permanent.
2. Add its bin folder to your PATH.
3. Run ``sayon doctor`` to check the installation.

``sayon run``, ``sayon test``, ``sayon check``, and ``sayon fmt`` work right away.
Native builds (``sayon build``, ``sayon run --native``) also need clang from LLVM:
install it from https://releases.llvm.org, then run ``sayon doctor`` again.
Programs you build this way run on other Windows machines without Sayon or LLVM.
``sayon build --target wasi`` builds WebAssembly programs that run under a WASI
host such as wasmtime, and ``sayon build --target web`` ones for web pages, with
the JavaScript that runs them and a page; they use the same clang.

To start a project: sayon init hello, then cd hello and sayon run.
Editor support for VS Code is in editors/vscode; see its README.
"@
Set-Content -Path (Join-Path $stage "INSTALL.txt") -Value $install -Encoding utf8

$archive = Join-Path $OutDir "$name.zip"
if (Test-Path $archive) { Remove-Item -Force $archive }
Compress-Archive -Path $stage -DestinationPath $archive

# The installer carries the archive inside it.
$env:SAYON_INSTALLER_PAYLOAD = $archive
try {
    cargo build --release -p sayon-installer
    if ($LASTEXITCODE -ne 0) { throw "building the installer failed" }
} finally {
    Remove-Item Env:SAYON_INSTALLER_PAYLOAD
}
$setup = Join-Path $OutDir "$name-setup.exe"
Copy-Item "target/release/sayon-setup.exe" $setup -Force

$sums = foreach ($file in $setup, $archive) {
    $hash = (Get-FileHash -Algorithm SHA256 $file).Hash.ToLowerInvariant()
    "$hash  $(Split-Path -Leaf $file)"
}
Set-Content -Path (Join-Path $OutDir "SHA256SUMS.txt") -Value $sums -Encoding ascii

foreach ($file in $setup, $archive) {
    $size = [math]::Round((Get-Item $file).Length / 1MB, 1)
    Write-Output "created $file ($size MB)"
}
