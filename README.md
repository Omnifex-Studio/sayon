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
- An interpreter that runs checked programs, with checked integer arithmetic and cleanup on every exit path
- A native backend: programs compile through LLVM to standalone Windows executables that need no runtime DLLs
- Modules: programs can be split across files with `import`
- The `sayon run`, `sayon run --native`, `sayon build`, and `sayon check` commands

Every example program is checked to behave identically in the interpreter and as a native executable, and to release every allocation it makes. Structures and enums, closures, and the standard library modules come next. Features planned for later milestones are rejected with a message that names the milestone, rather than half-working.

## Documentation

- [The language](docs/language.md): a reference for everything implemented so far
- [Feature ledger](docs/features.md): what exists, what is planned, and for which milestone
- [Implementation records](docs/implementation.md): grammar, ownership rules, cleanup order, data layouts, and the error ABI

## Trying it

The compiler is written in Rust. The toolchain version is pinned in `rust-toolchain.toml`, and `rustup` installs it automatically.

```sh
cargo build
cargo run -p sayon-cli -- run tests/programs/functions.sayon
cargo run -p sayon-cli -- build tests/programs/functions.sayon -o functions.exe
cargo test
```

Native builds need [LLVM](https://releases.llvm.org)'s clang at build time (found on `PATH`, in the default install location, or through `SAYON_CLANG`). The resulting executables have no such dependency.

`tests/programs` holds runnable examples with their expected output, and `tests/errors` holds invalid programs with the diagnostics they produce.

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
| `tests` | Example programs and diagnostic snapshots |
| `docs` | Language reference, feature ledger, and implementation records |
| `editors/vscode` | VS Code extension: highlighting and the `.sayon` file icon |
| `assets` | Logo and icons |
