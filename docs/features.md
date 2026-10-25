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
| Mutually recursive generic functions, with a limit on how deeply type arguments grow | experimental | M3 |
| Methods: `value.name(...)` calls a function declared beside the value's type | experimental | M2 |
| `for` loops over iterators, through a `next` method | experimental | M2 |
| `try` expressions, `is error` narrowing, `Outcome<T>`, unused-outcome warning | experimental | M2 |
| Dictionary keys of any integer, boolean, string, or `nothing` type, and structures, enums, and unions of those | experimental | M2 |
| Borrowed views (`View<T>`, `MutableView<T>`): `view`, `mutable_view`, indexing, loops, returned views of parameters, borrows until last use | experimental | M2 |
| Byte views of strings (`strings.byte_view`) | experimental | M2 |
| Splitting a view into non-overlapping parts (`split_view`) | experimental | M2 |
| String indexing and character iteration (`text[i]`, `for character in text`); slicing with `strings.slice` | experimental | M2 |
| Grapheme clusters (Unicode 17.0), UTF-8 bytes of strings and strings from bytes | experimental | M2 |
| `Shared<T>`, `Weak<T>`, `Cell<T>`: `share`, `cell`, `.value`, `.weak()`, `.upgrade()`, `.get()`, `.set()`, `.replace()` | experimental | M3 |
| `Mutex<T>`: `mutex`, `.get()`, `.set()`, `.replace()`, `.change()`; sendable values checked where a task crosses threads; `LockError` for waiting under a lock | experimental | M3 |
| C functions: `foreign` blocks, variadic calls, the target's C types, `pointer` and `null_pointer`, `c_string`; `unsafe` blocks, expressions, and functions; `standard/memory` | experimental | M3 |
| C functions in the interpreter on Linux x86-64 (System V), with C structures and callbacks | experimental | M3 |
| C functions in the interpreter on macOS and on ARM64 | planned | with each platform's validation |
| C structures (`foreign type`): C layout, by value in C calls, in C memory with `memory.read`, `write`, and `size`; Windows x64 and System V x86-64 (Linux) | experimental | M3 |
| C structures by value in WebAssembly programs (WASI), by its basic C convention | experimental | M4 |
| C structures by value on ARM64 | planned | with each platform's validation |
| C callbacks: Sayon function values, with what they captured, as parameters of C functions that call them during the call; failures raised once C returns; compiled programs on every target, the interpreter on Windows x64 and Linux x86-64 | experimental | M3 |
| C callbacks that C keeps and calls after the call, or on other threads | planned | M4 |
| Finally functions: a structure's own destruction, wherever its values are destroyed; never copied, cannot wait, failures fatal | experimental | M3 |
| Tasks (fibers) and channels: `standard/threads` `spawn`, `join`, `detach`, `pause`, bounded, unbounded, and rendezvous channels, `receive_result`, `recoverable_send`, `select`; cancellation with cleanup, shutdown of tasks at the end of `main`, deadlock detection; native on Windows and Linux x86-64 | experimental | M3 |
| Native tasks on macOS and on ARM64 | planned | with each platform's validation |
| Compile-time platform branches: `platform`, `processor`, `supports("capability")`; only the selected branch is checked and compiled | experimental | M4 |
| Capabilities: each target's are recorded, and a program's needs are found by following its calls through the standard library; a lacking target is an error at the program's own code | experimental | M4 |
| `async`/`await`, hygienic macros | planned | M8 |

## Tools

| Feature | State | Milestone |
|---|---|---|
| `sayon run` (interpreter) | experimental | M1 |
| `sayon check` | experimental | M1 |
| `sayon build`, `sayon run --native` on Windows x64 (M1) and Linux x86-64 (M3) | experimental | M1, M3 |
| Targets: `--target` for `check`, `lint`, and `build` (repeatable, `all`, `native`, short names, triples); `sayon target list` and `setup`; `[compiler]` `targets` and `required_capabilities` in `sayon.toml` | experimental | M4 |
| Building for WASI (`--target wasi`) on any computer: WebAssembly modules for `wasm32-wasip1`, run under wasmtime with the same behavior as the interpreter | experimental | M4 |
| Tasks and channels in WebAssembly programs (WASI): functions that can wait lowered to LLVM coroutines, a scheduler without fibers taking the same turns as the interpreter's | experimental | M4 |
| Building for web pages (`--target web`) on any computer: a `wasm32-unknown-unknown` module with generated JavaScript glue and a page; the program never blocks the page, its tasks waking through the event loop; checked in Edge, Chrome, and Firefox and in Node against the interpreter | experimental | M4 |
| HTTP in web pages: `standard/network`'s `get`, `post`, and `request` through the browser's `fetch`, the task waiting while the others run; checked in Edge, Chrome, Firefox, and Node against the interpreter | experimental | M4 |
| Building on one system for another (Android and the other desktop systems) | planned | M4 to M7, with each target |
| Diagnostics with stable codes and source excerpts | experimental | M1 |
| VS Code syntax highlighting and file icon | experimental | M1 |
| `sayon test` with `test` blocks (interpreter) | experimental | M1 |
| `sayon fmt` (keeps comments and literal spelling; verifies the result means the same) | experimental | M1 |
| Project manifests (`sayon.toml` [package]), `sayon init`, `sayon clean`, `sayon doctor` | experimental | M1 |
| Release archive for Windows x64 (`scripts/package.ps1`) | experimental | M1 |
| Installer for Windows x64 (`sayon-setup`): per-user, `PATH`, Settings > Apps uninstall | experimental | M1 |
| Installer for Linux x64: per user, into `~/.local/share/sayon` with a link in `~/.local/bin`; `scripts/package.sh` | experimental | M3 |
| Installers for Windows and Linux ARM64, and macOS | planned | with each platform's validation (M5, M7) |
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
| Standard output and error as files (`io.output`, `io.errors`); programs started without waiting (`os.start`), streaming their input and output while other tasks run; input and environments for programs | experimental | M3 |
| Text encodings other than UTF-8: `strings.encode` and `decode` for UTF-16, Latin-1, ASCII, and Windows-1252, and files opened with an encoding | experimental | M3 |
| `standard/json`: `Json` values, parse with positions in errors, compact and pretty encoding, exact number text, duplicate-field and depth limits | experimental | M2 |
| `standard/json` typed values: `decode`, `encode_value`, `from_json`, `to_json` for numbers, text, arrays, dictionaries, structures, enums, and unions, with paths in errors | experimental | M2 |
| `standard/json` streams: the items of a large array (`items`), JSON Lines (`lines`), and writing an array an item at a time (`write_items`) | experimental | M3 |
| `standard/collections`: stable sorting, `Set`, `Queue`; `remove` and `swap` built-ins | experimental | M2 |
| `standard/time`: durations, monotonic clock, sleep, UTC dates, ISO 8601 | experimental | M2 |
| `standard/time` deadlines and tickers; timers as channels (`threads.after`) | experimental | M3 |
| `standard/time` time zones of the IANA database, from the system (ICU on Windows, TZif files elsewhere, or `TZDIR`) | experimental | M3 |
| `standard/network`: TCP connections and listeners, UDP datagrams, address resolution, timeouts; waiting for sockets lets other tasks run | experimental | M3 |
| `standard/network` HTTP/1.1 client: `get`, `post`, `request`, chunked and sized bodies, redirects, body size limits | experimental | M3 |
| TLS from the system, with its certificate checks: `network.connect_secure`, `connection.secure`, `https://` in the HTTP client; Windows (SChannel) | experimental | M3 |
| TLS on Linux from OpenSSL (`libssl`, loaded when first needed) | experimental | M3 |
| TLS on macOS | planned | with its validation |
| Connecting on the first of a host's addresses that answers, IPv6 and IPv4 by turns (RFC 8305) | experimental | M3 |
| Streaming HTTP bodies; TLS servers and client certificates | planned | M4 |
| Tasks on threads of their own (`threads.spawn_parallel`), with their own tasks, cancellation, and `os.exit` across threads; channels and `select` between threads; native on Windows and Linux x86-64 | experimental | M3 |
| `standard.graphics` | planned | M4 baseline |

## Platforms

| Target | Specified | Buildable | Executed | Release-qualified |
|---|---|---|---|---|
| Windows x64 (`x86_64-pc-windows-msvc`) | yes | yes | yes, development machine | no |
| Linux x64 (`x86_64-unknown-linux-gnu`) | yes | yes | yes, Ubuntu 26.04 under WSL 2 | no |
| Windows and Linux ARM64 | yes | no | no | no |
| WASI (`wasm32-wasip1`) | yes | yes, from any computer | yes, under wasmtime 50 | no |
| Web pages (`wasm32-unknown-unknown`) | yes | yes, from any computer | yes, in Edge 156, Chrome 156, Firefox 158, and Node 24 | no |
| Android (`android-arm64`, `android-x64`) | yes, checked with `--target` | no | no | no |
| macOS, iOS | yes | no | no | no |
