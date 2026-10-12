# Builds Sayon in release mode and assembles a distributable archive:
#
#   sayon-<version>-x86_64-windows/
#     bin/sayon.exe            the command-line tool
#     lib/sayon_runtime.lib    linked into native builds
#     stdlib/                  the standard library
#     docs/, examples/, editors/vscode/, README.md, INSTALL.txt
#
# Usage: powershell -File scripts/package.ps1 [-OutDir dist]

param([string]$OutDir = "dist")

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$manifest = Get-Content (Join-Path $root "Cargo.toml") -Raw
if ($manifest -notmatch 'version = "([^"]+)"') { throw "cannot find the workspace version in Cargo.toml" }
$version = $Matches[1]

cargo build --release -p sayon-cli -p sayon-runtime
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$name = "sayon-$version-x86_64-windows"
$stage = Join-Path $OutDir $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
foreach ($dir in "bin", "lib", "stdlib", "docs", "examples", "editors", "assets") {
    New-Item -ItemType Directory -Force (Join-Path $stage $dir) | Out-Null
}

Copy-Item "target/release/sayon.exe" (Join-Path $stage "bin")
Copy-Item "target/release/sayon_runtime.lib" (Join-Path $stage "lib")
Copy-Item "stdlib/*.sayon" (Join-Path $stage "stdlib")
Copy-Item "docs/*.md" (Join-Path $stage "docs")
Copy-Item "examples/*.sayon" (Join-Path $stage "examples")
Copy-Item "assets/logo.png" (Join-Path $stage "assets")
Copy-Item -Recurse "editors/vscode" (Join-Path $stage "editors")
Copy-Item "README.md" $stage

$install = @"
Sayon $version for Windows x64

1. Put this folder somewhere permanent, for example C:\Program Files\Sayon.
2. Add its bin folder to your PATH.
3. Run `sayon doctor` to check the installation.

`sayon run`, `sayon test`, `sayon check`, and `sayon fmt` work right away.
Native builds (`sayon build`, `sayon run --native`) also need clang from LLVM:
install it from https://releases.llvm.org, then run `sayon doctor` again.
Programs you build this way run on other Windows machines without Sayon or LLVM.

To start a project: sayon init hello, then cd hello and sayon run.
Editor support for VS Code is in editors/vscode; see its README.
"@
Set-Content -Path (Join-Path $stage "INSTALL.txt") -Value $install -Encoding utf8

$archive = Join-Path $OutDir "$name.zip"
if (Test-Path $archive) { Remove-Item -Force $archive }
Compress-Archive -Path $stage -DestinationPath $archive
$size = [math]::Round((Get-Item $archive).Length / 1MB, 1)
Write-Output "created $archive ($size MB)"
