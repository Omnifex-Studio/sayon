#!/usr/bin/env bash
# Builds Sayon in release mode and produces the downloads for this device:
#
#   dist/sayon-<version>-<device>-setup     the installer users download and run
#   dist/sayon-<version>-<device>.tar.gz    the same files as a plain archive
#   dist/SHA256SUMS.txt                     checksums of both
#
# <device> names the operating system and processor, such as linux-x64.
# The archive holds:
#
#   bin/sayon                    the command-line tool
#   lib/libsayon_runtime.a       linked into native builds
#   lib/wasm32-wasip1/           the runtime for WASI, with WASI's C library
#   lib/wasm32-unknown-unknown/  the runtime for web pages
#   lib/x86_64-linux-android/, lib/aarch64-linux-android/  the runtimes for
#                                Android apps (x86-64 for emulators, ARM64 for phones)
#   stdlib/                      the standard library
#   licenses/                    the license texts docs/licenses.md names
#   docs/, examples/, assets/, README.md, LICENSE, INSTALL.txt
#
# The installer carries the files as a zip archive, which Python writes.
# This is the Linux and macOS counterpart of scripts/package.ps1.
#
# Usage: scripts/package.sh [output folder, dist by default]

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
out_dir="${1:-dist}"

version="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)"
[ -n "$version" ] || { echo "cannot find the workspace version in Cargo.toml" >&2; exit 1; }
case "$(uname -s)" in
    Linux) system=linux; system_name=Linux ;;
    Darwin) system=macos; system_name=macOS ;;
    *) echo "unsupported system $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) processor=x64 ;;
    aarch64 | arm64) processor=arm64 ;;
    *) echo "unsupported processor $(uname -m)" >&2; exit 1 ;;
esac
device="$system-$processor"
command -v python3 > /dev/null || { echo "python3 is needed to write the installer's archive" >&2; exit 1; }

cargo build --release -p sayon-cli -p sayon-runtime
cargo build --release -p sayon-runtime --target wasm32-wasip1
cargo build --release -p sayon-runtime --target wasm32-unknown-unknown
android_triples="x86_64-linux-android aarch64-linux-android"
for triple in $android_triples; do
    cargo build --release -p sayon-runtime --target "$triple"
done
# WASI's C library (wasi-libc), as the Rust toolchain ships it.
wasi_libc="$(rustc --print sysroot)/lib/rustlib/wasm32-wasip1/lib/self-contained"

mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"
name="sayon-$version-$device"
stage="$out_dir/$name"
rm -rf "$stage"
mkdir -p "$stage"/{bin,lib,stdlib,licenses,docs,examples,assets}

cp target/release/sayon "$stage/bin/"
cp target/release/libsayon_runtime.a "$stage/lib/"
mkdir -p "$stage/lib/wasm32-wasip1"
cp target/wasm32-wasip1/release/libsayon_runtime.a "$stage/lib/wasm32-wasip1/"
cp "$wasi_libc/crt1-command.o" "$wasi_libc/libc.a" "$stage/lib/wasm32-wasip1/"
cat > "$stage/lib/wasm32-wasip1/NOTICE.txt" <<EOF
crt1-command.o and libc.a are wasi-libc, the C library of WASI programs,
as the Rust toolchain ships it. WebAssembly programs that Sayon builds for
WASI include parts of it. Its license, which allows this, is at
https://github.com/WebAssembly/wasi-libc/blob/main/LICENSE
EOF
mkdir -p "$stage/lib/wasm32-unknown-unknown"
cp target/wasm32-unknown-unknown/release/libsayon_runtime.a "$stage/lib/wasm32-unknown-unknown/"
for triple in $android_triples; do
    mkdir -p "$stage/lib/$triple"
    cp "target/$triple/release/libsayon_runtime.a" "$stage/lib/$triple/"
done
cp stdlib/*.sayon "$stage/stdlib/"
cp docs/*.md "$stage/docs/"
cp examples/*.sayon "$stage/examples/"
for project in examples/*/; do
    cp -R "$project" "$stage/examples/"
done
# The texts of the licenses of what the files hold (see docs/licenses.md),
# as the Rust toolchain ships them.
licenses="$(rustc --print sysroot)/share/doc/rust/licenses"
for text in Apache-2.0.txt MIT.txt Unicode-3.0.txt LLVM-exception.txt; do
    cp "$licenses/$text" "$stage/licenses/"
done
cp assets/logo.png "$stage/assets/"
cp README.md LICENSE "$stage/"

# The newest version of the C library the tool needs, which the systems it
# runs on must have at least.
requirement=""
if [ "$system" = linux ] && command -v objdump > /dev/null; then
    glibc="$(objdump -T "$stage/bin/sayon" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -n 1)"
    [ -z "$glibc" ] || requirement="It runs on systems with version $glibc or later of the GNU C library (glibc)."
fi

cat > "$stage/INSTALL.txt" <<EOF
Sayon $version for $system_name ($processor)

The easiest way to install Sayon is the installer, $name-setup:
make it executable (chmod +x $name-setup), run it, and answer the
question it asks. It installs for your user account (no root needed) in
~/.local/share/sayon, links ~/.local/bin/sayon to it, and leaves your
shell profile alone; when ~/.local/bin is not on your PATH, it says how to
add it. To remove Sayon, run ~/.local/share/sayon/uninstall.

To use this archive instead:

1. Put this folder somewhere permanent.
2. Add its bin folder to your PATH.
3. Run \`sayon doctor\` to check the installation.

\`sayon run\`, \`sayon test\`, \`sayon check\`, and \`sayon fmt\` work right away.
Native builds (\`sayon build\`, \`sayon run --native\`) also need clang from LLVM,
from your system's packages (for example \`sudo apt install clang\`) or from
https://releases.llvm.org; then run \`sayon doctor\` again.
\`sayon build --target wasi\` builds WebAssembly programs that run under a WASI
host such as wasmtime, and \`sayon build --target web\` ones for web pages, with
the JavaScript that runs them and a page; they use the same clang, with its
WebAssembly linker.
TLS (https:// and network.connect_secure) uses the system's OpenSSL 3
(libssl.so.3), which most systems have.
$requirement

To start a project: sayon init hello, then cd hello and sayon run.
The guides are at https://github.com/sayon-lang/docs.sayon-lang.org, and
the VS Code extension at https://github.com/sayon-lang/sayon-vscode.
EOF

archive="$out_dir/$name.tar.gz"
rm -f "$archive"
tar -C "$out_dir" -czf "$archive" "$name"

# The installer carries a zip of the same files inside it.
payload="$out_dir/$name.zip"
rm -f "$payload"
python3 -I - "$out_dir" "$name" "$payload" <<'EOF'
import os, sys, zipfile

base, name, payload = sys.argv[1:]
with zipfile.ZipFile(payload, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
    for folder, folders, files in os.walk(os.path.join(base, name)):
        folders.sort()
        for file in sorted(files):
            path = os.path.join(folder, file)
            archive.write(path, os.path.relpath(path, base).replace(os.sep, "/"))
EOF

SAYON_INSTALLER_PAYLOAD="$payload" cargo build --release -p sayon-installer
rm -f "$payload"
setup="$out_dir/$name-setup"
cp target/release/sayon-setup "$setup"
chmod 755 "$setup"

checksum() {
    if command -v sha256sum > /dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}
(cd "$out_dir" && checksum "$(basename "$setup")" "$(basename "$archive")" > SHA256SUMS.txt)

for file in "$setup" "$archive"; do
    size="$(du -m "$file" | cut -f 1)"
    echo "created $file ($size MB)"
done
