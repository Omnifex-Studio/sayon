# Feature ledger

Every feature has one of these states:

| State | Meaning |
|---|---|
| planned | specified, not implemented; using it reports `E_UNAVAILABLE` with its milestone |
| experimental | implemented and tested, but may still change |
| beta | feature-complete for its milestone, changes need migration notes |
| stable | covered by the compatibility policy |

Nothing is beta or stable yet. Milestones: M1 first usable release, M2 expressive core, M3 systems and concurrency, M4 portable applications, M5 ecosystem, M6 production release, M7 Apple platforms, M8 advanced tooling, M9 self-hosting.

## Language

| Feature | State | Milestone |
|---|---|---|
| UTF-8 source, indentation blocks, tabs or spaces per file | experimental | M1 |
| Line and block comments | experimental | M1 |
| Numeric literals in all bases, separators, `u` suffix | experimental | M1 |
| Strings with escapes and interpolation | experimental | M1 |
| Bindings: implicit, `variable`, `constant`, annotated | experimental | M1 |
| Module constants with dependency-ordered initialization | experimental | M1 |
| All integer widths, `float`, `float32`, `boolean`, `string`, `nothing` | experimental | M1 |
| Arrays, string-keyed dictionaries, heterogeneous arrays | experimental | M1 |
| Operators, precedence, checked integer arithmetic | experimental | M1 |
| Checked numeric conversions | experimental | M1 |
| `if`/`else if`/`else`, `while`, `for` over ranges, arrays, dictionaries | experimental | M1 |
| Functions, recursion, defaults, parameter modes | experimental | M1 |
| Inferred and explicit generic functions | experimental | M2 |
| Unions with `nothing`/`typeof` narrowing | experimental | M2 |
| `error`/`raise`, `try`/`catch`/`finally` | experimental | M1 |
| Ownership: moves, clones, borrows, loop borrows, mode inference | experimental | M1 |
| `main` with integer exit status, scripts | experimental | M1 |
| Static local modules and `import`, `private` | experimental | M1 |
| Structures, enums, `match` with patterns and guards | planned | M2 |
| Function values, closures, captured state | planned | M2 |
| `try` expressions, `is error`, `Outcome<T>` | planned | M2 |
| Dictionary keys other than strings | planned | M2 |
| Borrowed views (`View<T>`, `MutableView<T>`) | planned | M2 |
| String indexing, slicing, character iteration | planned | M2 |
| `Shared`, `Weak`, `Cell`, `Mutex` | planned | M3 |
| `foreign` declarations, `unsafe`, pointers, C interoperability | planned | M3 |
| Fibers, channels, `select` | planned | M3 |
| Compile-time `platform` branches | planned | M4 |
| `async`/`await`, hygienic macros | planned | M8 |

## Tools

| Feature | State | Milestone |
|---|---|---|
| `sayon run` (interpreter) | experimental | M1 |
| `sayon check` | experimental | M1 |
| `sayon build`, `sayon run --native` on Windows x64 | experimental | M1 |
| Diagnostics with stable codes and source excerpts | experimental | M1 |
| VS Code syntax highlighting and file icon | experimental | M1 |
| `sayon test` with `test` blocks (interpreter) | experimental | M1 |
| `sayon fmt` (keeps comments and literal spelling; verifies the result means the same) | experimental | M1 |
| `sayon init`, `sayon doctor`, `sayon clean` | planned | M1 |
| `sayon lint`, `sayon explain`, package installation and lock files | planned | M2 |
| Registry commands: `publish`, `search`, `info`, `login`, `logout` | planned | M5 |
| Language server | planned | before M6 |
| Debugger, REPL, watch mode | planned | M8 |

## Standard library

| Module | State | Milestone |
|---|---|---|
| Prelude: `print`, `length`, `typeof`, `assert`, `range`, `tonumber`, `tostring`, `push`, `pop`, `haskey`, `readfile`, `writefile`, `clone` | experimental | M1 |
| `standard/io`: whole files, lines, standard input and output | experimental | M1 |
| `standard/os`: arguments, environment, platform, current directory | experimental | M1 |
| `standard/filesystem`: listing, metadata, create, remove, rename, copy, paths | experimental | M1 |
| `standard/strings`: split, join, trim, case, replace, search, slicing | experimental | M1 |
| `standard/math`: functions, constants, `random` | experimental | M1 |
| Streams and file handles (`io.open`, `file.lines()`), process spawning, `os.exit`, directory walking | planned | M2 to M3 |
| `standard.json` | planned | M1 to M2 |
| `standard.collections`, `standard.time` | planned | M1 to M3 |
| `standard.network`, `standard.threads` | planned | M3 |
| `standard.graphics` | planned | M4 baseline |

## Platforms

| Target | Specified | Buildable | Executed | Release-qualified |
|---|---|---|---|---|
| Windows x64 (`x86_64-pc-windows-msvc`) | yes | yes | yes, development machine | no |
| Linux x64 | yes | no | no | no |
| Windows and Linux ARM64 | yes | no | no | no |
| Android, browser WebAssembly, WASI | yes | no | no | no |
| macOS, iOS | yes | no | no | no |
