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
| `if`/`elseif`/`else`, `while`, `for` over ranges, arrays, dictionaries | experimental | M1 |
| Functions, recursion, defaults, parameter modes | experimental | M1 |
| Inferred and explicit generic functions | experimental | M2 |
| Unions with `nothing`/`typeof` narrowing | experimental | M2 |
| `error`/`raise`, `try`/`catch`/`finally` | experimental | M1 |
| Ownership: moves, clones, borrows, loop borrows, mode inference | experimental | M1 |
| `main` with integer exit status, scripts | experimental | M1 |
| Static local modules and `import`, `private` | experimental | M1 |
| Structures and enums, including generic ones; field access and assignment | experimental | M2 |
| `match` with literal, variant, union-member, and nested patterns, guards, exhaustiveness and unreachable-case checks | experimental | M2 |
| Structure patterns (positional and by field name), named variant fields, `or` alternatives | experimental | M2 |
| Function values, closures, captured state, function types | experimental | M2 |
| Functions declared inside functions (closures) | experimental | M2 |
| Mutually recursive generic functions | planned | M3, with specialization budgets |
| Methods: `value.name(...)` calls a function declared beside the value's type | experimental | M2 |
| `for` loops over iterators, through a `next` method | experimental | M2 |
| `try` expressions, `is error` narrowing, `Outcome<T>`, unused-outcome warning | experimental | M2 |
| Dictionary keys of any integer, boolean, string, or `nothing` type, and structures, enums, and unions of those | experimental | M2 |
| Borrowed views (`View<T>`, `MutableView<T>`): `view`, `mutable_view`, indexing, loops, returned views of parameters, borrows until last use | experimental | M2 |
| Byte views of strings (`strings.byte_view`) | experimental | M2 |
| Splitting a view into non-overlapping parts (`split_view`) | experimental | M2 |
| String indexing and character iteration (`text[i]`, `for character in text`); slicing with `strings.slice` | experimental | M2 |
| Grapheme clusters (Unicode 17.0), UTF-8 bytes of strings and strings from bytes | experimental | M2 |
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
| Project manifests (`sayon.toml` [package]), `sayon init`, `sayon clean`, `sayon doctor` | experimental | M1 |
| Release archive for Windows x64 (`scripts/package.ps1`) | experimental | M1 |
| Installer for Windows x64 (`sayon-setup`): per-user, `PATH`, Settings > Apps uninstall | experimental | M1 |
| Installers for Windows ARM64, Linux, and macOS | planned | with each platform's validation (M5, M1 to M3, M7) |
| `sayon explain` for every diagnostic and runtime error code | experimental | M2 |
| `sayon lint`: unused names, naming conventions, shadowing, likely typos; `# allow(...)` suppressions; `--deny warnings` | experimental | M2 |
| Dependencies from local folders and pinned Git commits, `sayon install` (`--locked`, `--offline`), `sayon.lock` with source digests | experimental | M2 |
| Dependencies by version from a registry | planned | M5 |
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
| `standard/strings`: split, join, trim, case, replace, search, slicing, character codes | experimental | M1 |
| `standard/math`: functions, constants, `random` | experimental | M1 |
| `standard/io` files: `io.open`, buffered reading and writing, `file.lines()` with a line limit, bytes, `seek`, `position`, checked `close`, `io.copy` | experimental | M2 |
| `os.exit` (in order: `finally` blocks and cleanup run), `filesystem.walk`, `filesystem.is_link` | experimental | M2 |
| Standard input as a file (`io.input`), running programs and capturing their output (`os.run`) | experimental | M2 |
| Standard output and error as files, other text encodings, streaming to and from running programs, environments for them | planned | M3 |
| `standard/json`: `Json` values, parse with positions in errors, compact and pretty encoding, exact number text, duplicate-field and depth limits | experimental | M2 |
| `standard/json` typed values: `decode`, `encode_value`, `from_json`, `to_json` for numbers, text, arrays, dictionaries, structures, enums, and unions, with paths in errors | experimental | M2 |
| `standard/json` streaming reading and writing | planned | M3 |
| `standard/collections`: stable sorting, `Set`, `Queue`; `remove` and `swap` built-ins | experimental | M2 |
| `standard/time`: durations, monotonic clock, sleep, UTC dates, ISO 8601 | experimental | M2 |
| `standard/time` time zones, timers, deadlines | planned | M3 |
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
