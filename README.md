<p align="center">
  <img src="assets/logo.png" alt="Sayon logo" width="200">
</p>

<h1 align="center">Sayon</h1>

<p align="center">A readable programming language for scripts, applications, and systems programs.</p>

Sayon means "easy". Ordinary programs should need fewer decisions, less boilerplate, and less setup, while advanced users can still see and control the work their programs do.

```sayon
function total(values: Array<integer>) -> integer
    result = 0
    for value in values
        result = result + value
    return result

numbers = [1, 2, 3]
print("Sum: {total(numbers)}")
print(length(numbers))  # numbers is still owned here
```

- Indentation-based blocks, whole-word keywords, and string interpolation
- Type inference with optional annotations, unions such as `integer or nothing`, and flow-sensitive narrowing
- Compiler-checked ownership with inferred borrowing and deterministic cleanup, with no garbage collector and no hidden copies
- Diagnostics with stable codes that explain the problem and a concrete fix

## Status

Sayon is in early development and not ready for real programs. What works today:

- A front end for the current language subset: lexer, parser, type inference, union narrowing, and ownership checking
- Structures and enums, including generic ones, and `match` with nested and structure patterns, alternatives, guards, and exhaustiveness checking
- Errors captured as values with `try` expressions, `Outcome<T>`, and `is error` narrowing
- Functions as values and closures that own what they capture
- An interpreter that runs checked programs, with checked integer arithmetic and cleanup on every exit path
- A native backend: programs compile through LLVM to standalone Windows executables that need no runtime DLLs, and on any computer to WebAssembly programs for WASI and for web pages, which come with the JavaScript that runs them and a page that shows their output, and to Android apps
- Checking a program for any target, with compile-time platform branches and the capabilities each target provides
- Modules: programs can be split across files with `import`, and projects can depend on other packages from local folders or pinned Git commits, locked by `sayon install`
- Borrowed views of arrays and strings, shared values with weak handles and cells, and finally functions that release what a structure owns when it is destroyed
- Tasks that take turns on a thread, channels and `select`, timers, and tasks on threads of their own that share data through channels and mutexes
- TCP and UDP sockets, TLS from the system, and an HTTP client for `http://` and `https://`, other programs started with pipes to their streams, and C functions: calls, C structures by value, and Sayon functions that C calls back
- Standard library modules for files, directories, streams and text encodings, JSON (typed values and streams), collections, time with time zones, processes, the network, threads, memory, text, and math, and graphics: drawing on canvases, smooth or not, the same pixels everywhere, text in TrueType fonts and the system's, PNG images, shown in windows on Windows and Linux, in web pages, and in Android apps
- `test` blocks and the `sayon test` runner, and the `sayon fmt` formatter
- A language server, `sayon lsp`, and a VS Code extension that starts it: diagnostics as you type, hover with signatures and doc comments, going to definitions, references, renaming, completion, and formatting
- Projects with a `sayon.toml` manifest, and the `run`, `build`, `check`, `test`, `fmt`, `init`, `clean`, `doctor`, `lint`, `lsp`, and `explain` commands

`examples/sketch` is a sketch pad, one program that runs as a window on Windows and Linux, as a web page, and as an Android app:

```text
cd examples/sketch
sayon run                         # a window on this computer
sayon build --target web          # a page: serve build/debug/web and open sketch.html
sayon build --target android-x64  # an app for the emulator: adb install build/debug/android-x64/sketch.apk
```

Every example program is checked to behave identically in the interpreter and as a native executable, and to release every allocation it makes. Linux x86-64 now passes every test in both modes, as Windows does, and each example that WASI, a web page, or Android can run behaves the same as a WebAssembly program under wasmtime, and in Edge, Chrome, Firefox, and Node, and on Android's emulator; macOS and ARM64 desktops come next. Features planned for later milestones are rejected with a message that names the milestone, rather than half-working.

## Installing

Download the installer for your device from the [releases page](https://github.com/Omnifex-Studio/sayon/releases) and run it. On Windows with an x64 processor (most PCs) that is `sayon-<version>-windows-x64-setup.exe`. It installs Sayon for your user account without administrator rights, makes `sayon` available in new terminals, and can be removed from Settings > Apps. On Linux with an x64 processor it is `sayon-<version>-linux-x64-setup`, which installs into `~/.local/share/sayon` and links `~/.local/bin/sayon`. Installers for other devices arrive as Sayon is validated on them; [installing.md](docs/installing.md) lists them and describes every option. No release has been published yet; until then, build the installer from source as described below.

## Documentation

- [Installing](docs/installing.md): installers for each device, what they change, and uninstalling
- [The language](docs/language.md): a reference for everything implemented so far
- [Feature ledger](docs/features.md): what exists, what is planned, and for which milestone
- [Error codes](docs/errors.md): what each diagnostic and runtime error code means, also shown by `sayon explain CODE`
- [Implementation records](docs/implementation.md): grammar, ownership rules, cleanup order, data layouts, and the error ABI
- [Licenses](docs/licenses.md): what is not Sayon's own in its files and in the programs it builds, and under which licenses

## Building from source

The compiler is written in Rust. The toolchain version is pinned in `rust-toolchain.toml`, and `rustup` installs it automatically.

```sh
cargo build
cargo run -p sayon-cli -- run tests/programs/functions.sayon
cargo run -p sayon-cli -- build tests/programs/functions.sayon -o functions.exe
cargo test
```

`scripts/package.ps1` builds the downloads for the device it runs on: the installer `dist/sayon-<version>-windows-x64-setup.exe`, the same files as `dist/sayon-<version>-windows-x64.zip`, and `dist/SHA256SUMS.txt`. On Linux, `scripts/package.sh` builds `dist/sayon-<version>-linux-x64-setup` and `dist/sayon-<version>-linux-x64.tar.gz` the same way.

With `sayon` on your `PATH`, a project looks like this:

```sh
sayon init hello
cd hello
sayon run       # Hello, world!
sayon test
sayon build     # build/debug/hello
```

Native builds need [LLVM](https://releases.llvm.org)'s clang at build time (found on `PATH`, in the default install location, or through `SAYON_CLANG`). The resulting executables have no such dependency.

`examples` holds programs written in Sayon: a word counter that reads standard input, a directory inventory, an HTTP file server with a task for each connection, a word counter that spreads files over threads, and a list of numbers kept in C memory, which C sorts and which frees itself. `tests/programs` holds runnable programs with their expected output, and `tests/errors` holds invalid programs with the diagnostics they produce.

## Repository layout

| Path | Contents |
|---|---|
| `crates/target` | The targets programs are checked and built for, and their capabilities |
| `crates/syntax` | Source files, diagnostics, lexer, and parser |
| `crates/check` | Name resolution, type inference, and ownership checking |
| `crates/ir` | Control-flow IR with explicit moves, drops, and error edges |
| `crates/interp` | The interpreter |
| `crates/driver` | Loads a program's modules from disk and runs the compiler phases |
| `crates/codegen` | Native code generation through LLVM IR |
| `crates/runtime` | Runtime library linked into compiled programs |
| `crates/lsp` | The language server `sayon lsp` runs |
| `crates/cli` | The `sayon` command |
| `crates/installer` | The installer (`sayon-setup`) |
| `stdlib` | The standard library, written in Sayon |
| `examples` | Small programs written in Sayon, and `sketch`, a project that runs on every primary platform |
| `tests` | Example programs and diagnostic snapshots |
| `docs` | Language reference, feature ledger, and implementation records |
| `scripts` | Release packaging: the installer and archive for each device |
| `editors/vscode` | VS Code extension: highlighting, the `.sayon` file icon, and the language server's client |
| `assets` | Logo and icons |

## License

Sayon is licensed under the [Apache License, Version 2.0, with LLVM's exception](LICENSE) (`Apache-2.0 WITH LLVM-exception`): the parts of Sayon that a program built with it holds, such as its runtime, can go with that program without its license or notices. [docs/licenses.md](docs/licenses.md) lists what its files and the programs it builds hold from others, and under which licenses.
