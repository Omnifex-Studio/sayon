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
- The `sayon run` and `sayon check` commands

The native backend, modules, structures and enums, closures, and the standard library modules come next. Features planned for later milestones are rejected with a message that names the milestone, rather than half-working.

## Trying it

The compiler is written in Rust. The toolchain version is pinned in `rust-toolchain.toml`, and `rustup` installs it automatically.

```sh
cargo build
cargo run -p sayon-cli -- run tests/programs/functions.sayon
cargo test
```

`tests/programs` holds runnable examples with their expected output, and `tests/errors` holds invalid programs with the diagnostics they produce.

## Repository layout

| Path | Contents |
|---|---|
| `crates/syntax` | Source files, diagnostics, lexer, and parser |
| `crates/check` | Name resolution, type inference, and ownership checking |
| `crates/ir` | Control-flow IR with explicit moves, drops, and error edges |
| `crates/interp` | The interpreter |
| `crates/cli` | The `sayon` command |
| `tests` | Example programs and diagnostic snapshots |
| `assets` | Logo and icons |
