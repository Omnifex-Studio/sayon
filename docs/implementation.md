# Implementation records

These records describe how the current compiler implements the language contracts. They are updated when an implementation decision changes; a change to observable semantics needs a design amendment first.

## Pipeline

```text
.sayon source
  -> lexer (crates/syntax/src/lexer.rs): tokens marked with line starts and indentation
  -> parser (parser.rs): AST with source spans, layout rules applied
  -> checker (crates/check): names, type inference, union narrowing -> typed HIR
  -> ownership (check/src/ownership.rs): copy/move/borrow decisions, parameter modes
  -> lowering (crates/ir/src/lower.rs): monomorphic control-flow IR with drops and error edges
       -> interpreter (crates/interp)
       -> LLVM IR text (crates/codegen) -> clang -> executable linked with crates/runtime
```

Both execution modes run the same IR. The runtime's text rules (number formatting, quoting, number parsing, range length) are shared with the interpreter, and the test suite requires every example program to produce identical output, errors, and exit status in both modes.

## Grammar

Layout is described in [language.md](language.md#source-files): a statement ends at a line start outside delimiters, and a block is the run of lines indented deeper than its header, all at the indentation of the block's first line. `NEWLINE`, `INDENT`, and `DEDENT` below stand for those layout events; the lexer does not produce them as tokens.

```ebnf
module      = { statement } ;
block       = NEWLINE INDENT statement { statement } DEDENT ;

statement   = function | declaration | if | while | for | try
            | "break" | "continue" | "return" [ expression ]
            | type_decl | enum_decl | match
            | expression [ ( "=" | "+=" | "-=" | "*=" | "/=" | "%=" ) expression ] ;

function    = [ "public" | "private" ] "function" NAME [ generics ]
              "(" [ param { "," param } [ "," ] ] ")" [ "->" type ] block ;
generics    = "<" NAME { "," NAME } ">" ;
param       = [ "own" | "update" ] NAME [ ":" type ] [ "=" expression ] ;
declaration = [ "public" | "private" ] ( "variable" | "constant" ) NAME [ ":" type ] "=" expression
            | NAME ":" type "=" expression ;

if          = "if" expression block { "elseif" expression block } [ "else" block ] ;
while       = "while" expression block ;
for         = "for" NAME [ "," NAME ] "in" expression block ;
try         = "try" block [ "catch" NAME block ] [ "finally" block ] ;   (* at least one clause *)

type_decl   = [ "public" | "private" ] "type" NAME [ generics ] "=" "[" field { "," field } [ "," ] "]" ;
field       = NAME ":" type ;
enum_decl   = [ "public" | "private" ] "enum" NAME [ generics ] NEWLINE INDENT variant { variant } DEDENT ;
variant     = NAME [ "(" field { "," field } [ "," ] ")" ] ;
match       = "match" expression NEWLINE INDENT arm { arm } DEDENT ;
arm         = "case" pattern [ "if" expression ] block ;
pattern     = pattern_atom { "or" pattern_atom } ;
pattern_atom = "_" | literal
            | NAME [ "(" pattern { "," pattern } ")" ]
            | NAME "(" NAME "=" pattern { "," NAME "=" pattern } ")" ;

type        = type_atom { "or" type_atom } ;
type_atom   = [ NAME "." ] NAME [ "<" type { "," type } ">" ] | "nothing" | "(" type ")"
            | "function" "(" [ [ "own" | "update" ] type { "," [ "own" | "update" ] type } ] ")" [ "->" type ] ;
            (* the result takes the rest of the type, so a function type with a result is parenthesized inside a union *)

expression  = or ;
or          = and { "or" and } ;
and         = equality { "and" equality } ;
equality    = ordering [ "is" "error" ] [ ( "==" | "!=" ) ordering ] ;     (* not chained *)
ordering    = additive [ ( "<" | ">" | "<=" | ">=" ) additive ] ;          (* not chained *)
additive    = multiplicative { ( "+" | "-" ) multiplicative } ;
multiplicative = unary { ( "*" | "/" | "%" ) unary } ;
unary       = ( "-" | "not" ) unary | postfix ;
postfix     = primary { call | "[" expression "]" | "." NAME } ;
call        = [ "<" type { "," type } ">" ] "(" [ arguments ] ")" ;
arguments   = expression { "," expression } [ "," ]
            | NAME "=" expression { "," NAME "=" expression } [ "," ] ;    (* building a structure *)
primary     = INTEGER | FLOAT | STRING | "true" | "false" | "nothing" | NAME
            | "(" expression ")" | collection | literal_function
            | "try" unary | "import" "(" STRING ")" ;
literal_function = "function" "(" [ param { "," param } ] ")" [ "->" type ]
                   ( block | "return" [ expression ] | expression ) ;   (* a one-line body stays on the header's line *)
collection  = "[" "]"
            | "[" expression { "," expression } [ "," ] "]"
            | "[" expression "=" expression { "," expression "=" expression } [ "," ] "]"
            | "[" NAME "=" expression { "," NAME "=" expression } [ "," ] "]" ;
```

The parser tests (`crates/syntax/src/parser.rs`) and the programs under `tests/` are the executable form of this grammar.

## Ownership rules

A value's type is Copy when it is a number, `boolean`, `nothing`, `Range`, a union of Copy types, or a structure or enum whose fields are all Copy (for a generic one, with Copy type arguments). Every other value, including every function value, has exactly one owner. Generic type parameters are treated as owned, so a generic function is checked for the strictest instantiation, except a parameter that only numbers can fill (one constrained by a numeric literal or by `-`, `*`, `/`, `%`, negation, or a conversion), which is Copy.

For each read of a local, the checker decides the use from the context:

| Context | Owned value | Copy value |
|---|---|---|
| assignment source, `own` argument, returned value, collection element, structure field or variant value, `try` operand, `push` value | move | copy |
| `update` argument | exclusive borrow | exclusive borrow |
| `push`/`pop` target, element or field assignment target | exclusive borrow | copy |
| anything else (operands, read arguments, `print`, conditions) | shared borrow | copy |
| `move(x)` | move | copy |
| captured by a closure that only reads it | move | copy |
| captured by a closure that changes it | move | taken over: later uses are errors |
| callee of a call through a function value | exclusive borrow | exclusive borrow |

The analysis tracks, along every control-flow path, whether each local holds a value. It reports:

| Code | Rule |
|---|---|
| `E_MOVED_VALUE` | A local is used after a move on some path, including an earlier loop iteration. Assigning a new value restores it. Inside `try`, a move in the body is uncertain in `catch` and `finally`. |
| `E_MOVE_BORROWED` | Moving a read or update parameter, a loop variable that borrows an element, or a case binding that borrows part of the matched value. |
| `E_MUTATE_BORROWED` | Changing a read parameter, a borrowed loop variable, or a borrowed case binding. |
| `E_MUTATE_CONSTANT` | Changing a local or module constant. |
| `E_MOVE_CONSTANT` | Moving a module constant. |
| `E_MOVE_OUT_OF_ELEMENT` | Moving an owned element out of an array or dictionary. |
| `E_MOVE_OUT_OF_FIELD` | Moving an owned field out of a structure stored in a variable or constant. Fields of temporaries can be moved; the rest of the temporary is destroyed. |
| `E_BORROW_CONFLICT` | Moving, changing, or replacing a collection while a `for` loop reads it, or a matched variable while a guard or a case with borrowing bindings runs; or passing the same local to one call both exclusively and in any other way. |
| `E_BORROWED_UNION` | A borrowed owned value used where a union is expected, which would need a copy. |
| `E_MOVE_CAPTURED` | Moving a captured value out of the closure that captured it, or a closure inside a closure changing a capture of the outer one. |
| `E_PARAMETER_MODE` | A named function used as a value whose inferred `own` or `update` parameter is not declared, so its function type would not show it. |
| `E_IMMUTABLE_STRING` | Assigning to a position of a string. |
| `E_VIEW_ESCAPE` | A view type inside a collection, structure, enum, union, closure capture, constant, or function result. |
| `E_VIEW_RETURN` | A returned view that does not borrow exactly one parameter. |
| `E_VIEW_TEMPORARY` | A view of an array that is not stored in a variable. |

Each view local records the variables it borrows: the root variable of the array it was made from, the variables its base view borrows, or, for the result of a call, those of the argument the callee's returned views borrow. A function's summary of which parameter its returned views borrow is computed with its parameter modes. A view's borrow lasts through the last statement of its declaring block that mentions it, so a use inside a loop or branch keeps it for that whole statement. While it lasts, moving, replacing, or changing a borrowed variable, and for a mutable view any use of it, is `E_BORROW_CONFLICT`.

In native code a view is `{ ptr first element, i64 length }`, the first two fields of an array, so indexing, `length`, and loops share the array code. The interpreter's view holds a reference to the array and a start and length; places through a view are rewritten to the array's element when they are resolved.

A string position (`text[i]`) produces a new string, so it borrows the text and can be moved freely. A `for` loop over a string reads it in place like a collection, so the string cannot be replaced while the loop runs; each loop variable owns its character.

Calling a function value needs exclusive access to it, because a closure may change what it captured. Calling through a `constant` local or module constant is allowed when its initializer is known to hold only named functions and closures that change none of their captures; such calls only read the constant.

Closure bodies are analyzed after every named function's modes are known. A closure's parameter modes come from its function type, never from inference.

Parameter modes are inferred for unannotated parameters as the least of read, update, and own that the body needs, iterated to a fixed point over mutually recursive functions. Annotated parameters keep their declared mode (default read), and a body that needs more is an error. A field or element assignment needs update mode of the root parameter, except for a Copy parameter, which is the function's own copy.

### Matches

A `match` whose value is a place (a variable, element, field, or constant) reads it in place: case bindings of owned types are references into it, and Copy bindings are copies. A match on a temporary or on `move(x)` owns its value: it is held in a temporary, the bindings of cases without a guard are moved out of it, and what remains is destroyed when the match ends. Bindings of guarded cases always borrow, so a failed guard never leaves a value partly moved. A Copy value is matched on a copy.

An arm with `or` alternatives is lowered once for each combination of alternatives, in order, sharing nothing but its source: each copy tests and binds its own alternative, so a failed guard falls through to the next copy. The checker requires the alternatives to bind the same locals with the same types, so every copy of the body sees the same variables.

Coverage is computed with the usual pattern-matrix usefulness algorithm. A match is exhaustive when a wildcard is not useful after its unguarded cases; a case is unreachable when its pattern is not useful after the unguarded cases above it. Enums, unions, `boolean`, and `nothing` have finite constructor sets, and a structure has exactly one; numbers and strings do not. A row whose first pattern has alternatives counts as one row per alternative. A binding at the top of a case on a union value gets the union of the members still useful there.

## Cleanup and control flow

Every block and every statement is a scope that lists the owned slots it must destroy. On each way out of a scope, the owned values it holds are destroyed in reverse declaration order, innermost scope first:

| Exit | Order |
|---|---|
| end of block | the block's statement temporaries, then its locals |
| `break`, `continue` | scopes inside the loop, innermost first, running each `finally` passed on the way |
| `return` | the result moves into the return slot first; then all scopes, running each `finally`; then the return |
| error | scopes up to the nearest handler; then the `catch` (which receives the error) or the `finally` |

A `finally` is lowered as a separate copy on each path through it (normal end, error, `return`, `break`, `continue`). On the error path the pending error is held in a temporary while the `finally` runs, then propagation continues. If a `finally` fails while a `return` is pending, the return value is destroyed and the new error propagates.

A moved-from place is empty, and destroying an empty place does nothing, so cleanup code destroys every owned slot in scope without per-path bookkeeping. In native code "empty" is all-zero bytes.

## Representations

| Type | Native layout | Size |
|---|---|---|
| integers | `i8`, `i16`, `i32`, `i64` | 1 to 8 |
| `float`, `float32` | `double`, `float` | 8, 4 |
| `boolean` | `i1` | 1 |
| `nothing` | `i8` | 1 |
| `string` | `{ ptr data, i64 length, i64 capacity }`, capacity 0 for static text; a one-character ASCII string taken from another string points into a static table | 24 |
| `Array<T>` | `{ ptr data, i64 length, i64 capacity }`, elements contiguous | 24 |
| `Range` | `{ i64 start, i64 end, i64 step }` | 24 |
| `Dict<K, V>`, `Error` | pointer to a runtime object | 8 |
| `HostStream` | `i64` number of a stream | 8 |
| union | `{ i32 tag, [N x i64] payload }`, payload at offset 8 | 8 + 8N |
| structure | `{ fields... }` in declaration order, each at its natural alignment | sum of fields with padding |
| enum | `{ i32 tag, [N x i64] payload }`; tag is the variant's position plus one; a variant's fields are laid out like a structure starting at offset 8 | 8 + 8N |
| function value | `{ ptr code, ptr environment }` | 16 |

Union tags are assigned per member type across the whole program (the module's union member table), so a value of a larger union is a valid value of every smaller union holding the same member. Narrowing and widening therefore never copy or re-tag. Tag zero means empty.

Structures and enums are stored inline, so a type that contains itself without a collection in between is rejected (`E_RECURSIVE_TYPE`). Destroying a structure destroys its owned fields; destroying an enum destroys the fields of its current variant. Moving one field out zeroes just that field, which keeps the rest destroyable.

A closure's environment is a heap block: a pointer to a function that destroys and frees it, a pointer to a function that copies it, then a structure with one field per captured variable, from offset 16. Inside the closure, each captured variable is a reference to its field, so changes persist between calls. A closure that captures nothing, and a named function used as a value, has a null environment. A named function's value calls a small adapter that accepts the environment pointer and forwards its arguments, so every function value of a type is called the same way.

A dictionary keeps its keys and values in two arrays in insertion order, and finds keys through an open-addressing table of positions keyed by hash. Each key type has a static table of generated helpers the dictionary calls: hash, equality, copy, destroy, and display (for `KeyError` messages). A key's hash combines its parts' hashes in order: a union or enum includes its tag, a string hashes its bytes, and integers hash their values, so values that are equal hash alike.

## Packages

A package is identified by its source: `path:<folder>`, relative to the root package and written with `/`, or `git:<url>#<commit>`. `sayon install` walks the manifests depth first from the root, recording for each package its name, version, and dependency edges (`alias = source`), and fails on a cycle. The lock is written to a temporary file and renamed over `sayon.lock`, so it is never half written; the same graph always renders the same text.

A Git source is cloned into `<cache>/git/<first 20 hex digits of SHA-256(url, NUL, commit)>` through a temporary folder, checked out at the commit, and verified with `git rev-parse HEAD`. Its digest is SHA-256 over the domain prefix `sayon-package-digest-1` and a NUL byte, then, for each file in order of its `/`-separated relative path, the path's length (8 bytes, little-endian), the path, the content's length, and the content. Hidden entries (such as `.git`), and `target` and `sayon.lock` at the package's top, are left out; line endings are not normalized; symbolic links are rejected. The build family recomputes the digest of every Git source it uses. Local folders are not digested, since they are meant to be edited.

The loader resolves `dependencies/<alias>/<path>` against the package of the importing module, using a map from each package's canonical folder to its aliases, built from the lock. Modules from dependencies are named `dependencies/<alias>/<path>` in messages, nested for dependencies of dependencies.

## Methods and iterators

A method call `value.name(...)` is checked as an ordinary call with the value as the first argument, after the checker knows the value's type: it looks in the module declaring the type for a function `type_name`, then `name`, whose first parameter is annotated with that type. Functions are checked in dependency order, and since a method call names no module, a function calling `.name(...)` depends on every method named `name` of any type; a `for` loop depends on every `next`. These edges can only join components in a cycle that already exists through the methods' bodies.

A loop over an iterator keeps the iterator in a hidden local: a reference to the variable it iterates, or the owner of an iterator made for the loop, destroyed when the loop ends. Each iteration calls `next` into a slot of type `T or nothing`, leaves when it holds `nothing`, and otherwise moves the payload into the loop variable, whose scope ends with the iteration. The loop borrows the iterated variable, and for a made iterator, the variables passed to the call that made it in `update` mode.

## Streams

A `File` holds a `HostStream`, a type only the standard library can name: in both execution modes a 64-bit number naming an entry of the runtime's stream table, `0` once moved. An entry counts the values that refer to it. A clone adds one, destroying a value removes one, and the last closes the file, writing its buffer without a report; `close` writes the buffer, reports a failure, and leaves the entry closed for the references that remain. Numbers are never reused, so a stale reference can never reach another file. Readers and writers are buffered; a line read keeps at most the limit and a carriage return, plus one buffer of 8 KiB. When the program ends, every writer still open is flushed. Native programs count each stream reference as an allocation in the `SAYON_DEBUG_ALLOC` report.

## Calls and parameter passing

Every compiled function has the signature `i32 f(ptr result, ptr error, i64 argument_count, parameters...)`. It returns 0 after writing its result through `result`, or 1 after storing an `Error` pointer through `error`. Callers always pass every parameter; a parameter the caller omitted is computed from its default in the callee's prologue, selected by `argument_count`.

| Parameter | Passed as |
|---|---|
| Copy type, read or `own` | the value |
| any type, `update` | a pointer to the caller's place |
| owned type, `own` | the value; the caller's place is cleared |
| owned type, read | a pointer to the caller's place |

A call through a function value loads the code and environment pointers and calls the code with the environment structure's address before the arguments: `i32 code(ptr result, ptr error, i64 argument_count, ptr environment, parameters...)`. How each argument is passed follows the parameter modes of the function type.

## Recoverable errors

An `Error` is a runtime object with a code, a message, and the source location of the operation that raised it. Each function has an error slot. A failing operation stores a new error there and continues at its error edge, which destroys the scopes being left and then jumps to the innermost handler or returns status 1. `catch` moves the error out of the slot into its binding.

A `try` expression evaluates its operand under a handler of its own, as a `try` statement does: the operand's temporaries are a scope that the error edge destroys, and the handler moves the error out of the slot into `failure(problem)`. On success the operand moves into `success(value)`. `Outcome<T>` is the prelude's first type definition, an ordinary enum with those two variants, so it has the enum layout and `match` works on it. Narrowing an `Outcome` by `is error` records the variant as well as the type, so reading the variable reads that variant's field; this keeps `Outcome<Error>` unambiguous.

`os.exit` raises an error with the code `Exit`. Every `catch` and `try` handler first compares the pending error's code with it and, when it matches, passes the error on as if it had no handler, after its `finally` block; so the error reaches the entry boundary, through every cleanup on the way, and the boundary exits with the status it carries instead of reporting it.

The program's entry runs the module constant initializers, then the entry function. An error escaping either is reported as `error: uncaught CODE: message` followed by `  --> file:line:column`, and the program exits with status 1 after releasing module constants.

## Limits

| Limit | Value |
|---|---|
| Interpreter call depth | 100,000 nested calls; exceeding it stops the program with status 3 |
| Integer literals | must fit 64 bits, and their type |
| Native stack | the operating system's default; overflowing it is not recoverable |

Budgets for source size, generic expansion, and IR size are not enforced yet.

## Toolchain

| Component | Version | Notes |
|---|---|---|
| Rust | 1.98.1 | pinned in `rust-toolchain.toml` |
| LLVM / clang | 23.1.3 | validated on Windows x64; found on `PATH`, in the default install location, or through `SAYON_CLANG` |
| C runtime | static (`+crt-static`) on Windows | compiled programs import only system DLLs |

Native builds currently target the host. The runtime library is found next to the `sayon` executable or in `../lib`, or through `SAYON_RUNTIME_LIB`.

## Verification

- `cargo test` runs the lexer, parser, checker, ownership, IR, runtime, and interpreter unit tests, then every program under `tests/programs` in both execution modes, comparing output, errors, and exit status with the recorded expectations, and every program under `tests/errors`, comparing its diagnostics.
- Native runs set `SAYON_DEBUG_ALLOC`, and each program must report zero live runtime allocations at exit, including programs that end with an uncaught error.
- Set `SAYON_BLESS=1` to record new expectations, and review the differences before committing them.
