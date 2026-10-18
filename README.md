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
- A native backend: programs compile through LLVM to standalone Windows executables that need no runtime DLLs
- Modules: programs can be split across files with `import`
- Standard library modules for files, directories, standard input and output, program arguments, text, and math
- `test` blocks and the `sayon test` runner, and the `sayon fmt` formatter
- Projects with a `sayon.toml` manifest, and the `run`, `build`, `check`, `test`, `fmt`, `init`, `clean`, `doctor`, and `explain` commands

Every example program is checked to behave identically in the interpreter and as a native executable, and to release every allocation it makes. Dependencies and lock files come next. Features planned for later milestones are rejected with a message that names the milestone, rather than half-working.

## Installing

Download the installer for your device from the [releases page](https://github.com/Omnifex-Studio/sayon/releases) and run it. On Windows with an x64 processor (most PCs) that is `sayon-<version>-windows-x64-setup.exe`. It installs Sayon for your user account without administrator rights, makes `sayon` available in new terminals, and can be removed from Settings > Apps. Installers for other devices arrive as Sayon is validated on them; [installing.md](docs/installing.md) lists them and describes every option. No release has been published yet; until then, build the installer from source as described below.

## Documentation

- [Installing](docs/installing.md): installers for each device, what they change, and uninstalling
- [The language](docs/language.md): a reference for everything implemented so far
- [Feature ledger](docs/features.md): what exists, what is planned, and for which milestone
- [Error codes](docs/errors.md): what each diagnostic and runtime error code means, also shown by `sayon explain CODE`
- [Implementation records](docs/implementation.md): grammar, ownership rules, cleanup order, data layouts, and the error ABI

## Building from source

The compiler is written in Rust. The toolchain version is pinned in `rust-toolchain.toml`, and `rustup` installs it automatically.

```sh
cargo build
cargo run -p sayon-cli -- run tests/programs/functions.sayon
cargo run -p sayon-cli -- build tests/programs/functions.sayon -o functions.exe
cargo test
```

`scripts/package.ps1` builds the downloads for the device it runs on: the installer `dist/sayon-<version>-windows-x64-setup.exe`, the same files as `dist/sayon-<version>-windows-x64.zip`, and `dist/SHA256SUMS.txt`.

With `sayon` on your `PATH`, a project looks like this:

```sh
sayon init hello
cd hello
sayon run       # Hello, world!
sayon test
sayon build     # build/debug/hello
```

Native builds need [LLVM](https://releases.llvm.org)'s clang at build time (found on `PATH`, in the default install location, or through `SAYON_CLANG`). The resulting executables have no such dependency.

`examples` holds small tools written in Sayon, such as a word counter that reads standard input and a directory inventory. `tests/programs` holds runnable programs with their expected output, and `tests/errors` holds invalid programs with the diagnostics they produce.

## Repository layout

| Path | Contents |
|---|---|
| `crates/syntax` | Source files, diagnostics, lexer, and parser |
| `crates/check` | Name resolution, type inference, and ownership checking |
| `crates/ir` | Control-flow IR with explicit moves, drops, and error edges |
| `crates/interp` | The interpreter |
| `crates/driver` | Loads a program's modules from disk and runs the compiler phases |
| `crates/codegen` | Native code generation through LLVM IR |
| `crates/runtime` | Runtime library linked into compiled programs |
| `crates/cli` | The `sayon` command |
| `crates/installer` | The installer (`sayon-setup`) |
| `stdlib` | The standard library, written in Sayon |
| `examples` | Small programs written in Sayon |
| `tests` | Example programs and diagnostic snapshots |
| `docs` | Language reference, feature ledger, and implementation records |
| `scripts` | Release packaging: the installer and archive for each device |
| `editors/vscode` | VS Code extension: highlighting and the `.sayon` file icon |
| `assets` | Logo and icons |
