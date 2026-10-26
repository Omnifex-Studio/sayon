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
            | type_decl | enum_decl | match | foreign | "unsafe" block
            | expression [ ( "=" | "+=" | "-=" | "*=" | "/=" | "%=" ) expression ] ;

function    = [ "public" | "private" ] [ "unsafe" ] "function" NAME [ generics ]
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
foreign     = "foreign" STRING NEWLINE INDENT signature { signature } DEDENT ;
signature   = [ "public" | "private" ] "function" NAME
              "(" [ NAME ":" type { "," NAME ":" type } ] [ [ "," ] "..." ] ")" [ "->" type ] ;
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
            | "try" unary | "unsafe" unary | "import" "(" STRING ")" ;
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
| `E_VIEW_TEMPORARY` | A view of an array or string that is not stored in a variable, directly or through a function returning a view of that argument. |

Each view local records the variables it borrows: the root variable of the array it was made from; for a view made of a view, that view's variable and what it borrows; for the part `split_view` splits off, what the split view borrows, but not that view, whose remaining elements it does not overlap; or, for the result of a call, those of the argument the callee's returned views borrow. A returned view's summary leaves out the function's own view variables, which own no elements. A function's summary of which parameter its returned views borrow is computed with its parameter modes. A view's borrow lasts through the last statement of its declaring block that mentions it, so a use inside a loop or branch keeps it for that whole statement. While it lasts, moving, replacing, or changing a borrowed variable, and for a mutable view any use of it, is `E_BORROW_CONFLICT`.

In native code a view is `{ ptr first element, i64 length }`, the first two fields of an array, so indexing, `length`, and loops share the array code. A string starts with the same two fields over its UTF-8 bytes, so `strings.byte_view` makes a `View<byte>` the same way; only the standard library may make a view of a string, since `view(text)` would read as a view of characters. The interpreter's view holds a reference to the array and a start and length; places through a view are rewritten to the array's element when they are resolved.

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
| `Dict<K, V>`, `Error` | pointer to a runtime object | P |
| `HostStream` | `i64` number of a stream | 8 |
| union | `{ i32 tag, [N x i64] payload }`, payload at offset 8 | 8 + 8N |
| structure | `{ fields... }` in declaration order, each at its natural alignment | sum of fields with padding |
| enum | `{ i32 tag, [N x i64] payload }`; tag is the variant's position plus one; a variant's fields are laid out like a structure starting at offset 8 | 8 + 8N |
| function value | `{ ptr code, ptr environment }`, the environment at offset P | 2P |
| `Shared<T>`, `Weak<T>` | pointer to a block: the `Shared` count, the `Weak` count plus one for all the `Shared` handles, the block's size, then the value at offset 24 | P |
| `Cell<T>` | the value itself | as `T` |
| `pointer` | `ptr` | P |
| `c_string` | `ptr` to the text of a heap block: a word holding the block's size, the text, and a NUL byte | P |

P is the size of an address on the target: 8, or 4 on WebAssembly (`wasm32`). Lengths, capacities, counts, and union payloads are 64-bit integers on every target, and LLVM aligns a 64-bit integer to 8 bytes on WebAssembly too, so a string's or an array's length stays at offset 8 and every layout but those holding addresses is the same on all targets. The C types follow the target (`c_long` and `c_size` are 32 bits on WebAssembly), and so do the sizes of C structures.

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

## Shared values

`Shared` and `Weak` are counted handles. The ownership checker treats them, and unions of them with Copy types, as reusable: using one again is a copy, not a move. Lowering turns every owned use of a reusable handle that is not an explicit `move` into a clone, and cloning a handle adds to its count; destroying one subtracts. A parameter of a handle type is still passed by reference for reading, so the rules that treat a Copy parameter as the function's own copy keep applying only to true Copy types.

The value is destroyed when the `Shared` count reaches zero, and the block freed when the `Weak` count does; since the `Shared` handles together hold one `Weak` count, the block is freed exactly once however the counts reach zero. In the interpreter a handle is an `Rc<RefCell<Value>>`; a place through `shared.value` steps into it, and reads and writes walk such steps recursively, borrowing the cell only while they run.

A `Cell<T>` has its value's layout. `get` clones the value, `set` destroys it and stores the new one, and `replace` moves it out first; all three work through a read-only place, which is sound because no place inside a cell can be borrowed, and none of them runs other code while it changes the cell. A cell is stored inline, so a type that holds itself through a cell is `E_RECURSIVE_TYPE`.

## Tasks

A task is a fiber with a call stack of its own. An operation that waits (receiving, sending to a full channel or a rendezvous, joining, sleeping, `pause`, `select`) records what it waits for and switches to the next task that can go on, scanning the tasks round from the one after the running task. A task can go on when what it waits for has come or it is to stop; when none can but some sleep, the scheduler waits for the earliest to wake, and when none can at all, the waiting operation raises `DeadlockError`. A new task starts when another waits.

In the interpreter a task is a frame stack of its own; the running task's stack is the machine's, and switching swaps them. A waiting operation leaves its frame at the operation and runs it again when resumed, which then finds what it waited for; the operations that act and then wait (a rendezvous send, sleeping, `pause`) are told how they were resumed so that they finish instead. In compiled programs a task is a fiber, and a waiting runtime function loops around its wait on the task's own stack. On Windows a fiber is the system's (`CreateFiber` and `SwitchToFiber`); on Linux and Android it is the runtime's own: a stack of 1 MiB mapped with `mmap`, reserved but not committed, with a page below it that faults, and `sayon_fiber_switch`, a few instructions of assembly that push the registers the callee keeps and the floating-point control words, save the stack pointer, and restore the other fiber's: on x86-64, System V's `rbx`, `rbp`, `r12` to `r15`; on ARM64, `x19` to `x30` and `d8` to `d15`, leaving `x18`, which is Android's. A new fiber's stack starts as if it had switched away, so that its first switch returns into `sayon_fiber_entry`, which calls the task's start. A task's block holds the function value it runs, a buffer for its result, its outcome, and the slot its function's error goes to, and is shared by its handle and the scheduler; a channel is a counted handle to a queue of messages, each in a buffer of whole words so that it can be destroyed in place.

WebAssembly has one stack, so compiled programs for it have no fibers; the scheduler is the same, but a task stops by returning. When a program waits anywhere, the code generator (`crates/codegen/src/coroutines.rs`) lowers to an LLVM switched-resume coroutine each function that waits, that calls through a function value, or that is used as a value, and, until none is left, each that calls one of those; LLVM splits each into a function that starts it and one that resumes it, with a heap frame, from the runtime's allocator, holding what lives across a stop. Calls keep their convention and gain the status 2: the callee stopped, leaving its frame in `sayon_frame`. The caller then stops too, and when resumed resumes that frame through `sayon_coroutine_resume`, until it gives a final status; a frame frees itself when its function finishes, leaving the status for its resumer. A waiting runtime operation stops the same way: it records what it waits for, keeps a closure that calls it again with the same arguments, leaves a null frame, and gives 2. The scheduler then picks the next task as the fiber scheduler would, and resumes a task by resuming its first function's frame, down the chain of callers to the operation, which is called again and told what it waited for, like the interpreter's: it finishes, or fails when the task was cancelled or every task waits. When the main task's function stops, `main` hands the scheduler the frame (`sayon_tasks_wait_main`) until the function finishes. Programs that never wait have no coroutines, and the form of `llvm.coro.end`, which gives nothing in newer LLVM (23 here) and a boolean in older (21 here), is found by having the clang in use read a call of the older form, which only older versions accept.

Cancellation sets a flag the task's next wait sees: the wait fails with the internal code `Cancelled`, which, like `Exit`, every `catch` and `try` handler passes on, so the task unwinds through its cleanup. When the main task's function returns or fails, the program cancels every task left, runs each until it has finished, destroys the tasks that never started with what they captured, reports the failures nobody joined, and only then ends; an `Exit` from a task decides the exit status.

## Threads and mutexes

A task started with `spawn_parallel` runs on an operating-system thread with a scheduler of its own, which runs the tasks it spawns; the main thread's scheduler is one of these. Nothing the two threads use is counted without atomics: the checker lets a task's captures and result cross only when their type is `SEND`, which leaves out `Shared`, `Weak`, tasks, views, and function values. In compiled programs every other value owns its parts outright, so the function value and the result move between threads as they are. The interpreter's strings count their references, so what crosses (the captures, the result, the program's constants for the new thread, and a mutex's value at each lock and unlock) is copied in full first. The thread's task block starts with a kind, which tells the task functions of the runtime it from a task of the same thread; it holds the outcome under a lock, and the thread wakes the other schedulers through their waking sockets when it finishes, so that a `join` waits like a wait for a socket.

A mutex is a counted block, with an atomic count, holding its value and which task holds its lock: the thread and the task's place in that thread's scheduler. Taking the lock moves the value out and putting it back moves it in, so `get`, `set`, `replace`, and `change` are a take and a put around code that only works on the value; `change` puts it back on its error edge too. A task that holds a lock counts it, and every wait fails with `LockError` while the count is above zero, so a task never switches away holding a lock: a lock is only ever held by the running task of some thread, and another thread waiting for it blocks on a condition variable until it is put back.

A channel is a block with an atomic count of its handles and its messages under a lock, so the tasks of any thread can use it; `select` takes the message of the channel it chooses under that lock, so that no task on another thread takes it in between. Each thread lists the channels made on it that no other thread can hold yet. A channel's handle can leave its thread only inside a value that does (what a parallel task captures or returns, a mutex's value, or a message on a channel that left already), and the runtime does not look inside those, so each of them marks all of the thread's listed channels as crossed. The interpreter copies the messages waiting in a channel when it crosses, and each message sent on it after that. A change to a crossed channel wakes the schedulers of the other threads; a thread none of whose tasks can go on, but one of which waits for a crossed channel while tasks run on other threads, waits to be woken instead of raising `DeadlockError`.

At the end of the program, after the main thread's own tasks, the program cancels the tasks on other threads (each sees the flag at its next wait, and the threads' schedulers are woken to see it), waits for each thread to finish its own tasks and end, and reports the failures nobody joined. `os.exit` on any thread records the status for the whole program and cancels every task.

## Finally functions

Compiled programs destroy every value through the destroy helper of its type, so a structure with a finally function calls it there, before destroying the fields, between `sayon_finally_enter` and `sayon_finally_leave`, which make its task's waits fail with `FinallyError`; a failure calls `sayon_finally_failed`, which reports it and ends the process with status 3. A moved value leaves its place zeroed, and the helper runs on such places too, so such a structure has a byte after its fields, 1 from when the value is made and 0 once it has moved away or been destroyed, and the helper does nothing when it is 0.

The interpreter destroys values when Rust drops them, wherever that happens, without the machine at hand. So the fields of such a structure's value carry a token, which copies made to read a value do not carry, and dropping them queues the value on its thread. Between instructions, when a value is queued (a global count, cheap to read, tells), the machine takes the queue and runs each value's finally function, with the value held where the function's `update` parameter can reach it, from the frame below; then it destroys the fields, and runs what that queued before the next value, as the destroy helper does. Values moving to another thread keep their token, and copies of constants for a thread's machine have none. At the end of the program, after the tasks, it destroys the constants in the reverse order of their initialization.

## C functions

A compiled program declares each C function of its `foreign` blocks once, by its name, and calls it with the C types of its declaration: `call i32 (ptr, ptr, ...) @sprintf(...)` for a variadic one, which LLVM passes by the target's C convention (on Windows x64, a float in both its integer and its float register). A C string is passed as the address of its text, and a narrow integer with the extension C expects. The libraries of the blocks are linked by name, and `"c"`, the C runtime, is linked already; on Windows, `printf` and its kin come from `legacy_stdio_definitions`. After each call of a C runtime function the program calls `fflush(NULL)`, so that what C writes comes out in order with what the program prints.

The interpreter loads a library the first time one of its functions is called. On Windows it uses `LoadLibraryW` and `GetProcAddress`; `"c"` is `msvcrt.dll`, which has every C function, then `ucrtbase.dll`. On Linux it uses `dlopen` and `dlsym`; `"c"` is `libc.so.6` and `libm.so.6`, and any other name `lib{name}.so`, then `lib{name}.so.6`. On Windows x64 a call goes through one routine for every signature: the arguments come to it as 64-bit words, an integer extended and a float as its bits, and it loads each of the first four into both its integer and its float register and copies the rest to the stack above the 32 bytes reserved for those four, then keeps both result registers; the declared result type picks one. A narrow integer result is extended from its own bits, which C leaves undefined above.

On Linux x86-64 the routine, `sayon_interp_call_sysv`, takes the six integer registers, the eight vector registers, and the words for the stack apart, so the interpreter assigns each argument by System V's rules: an integer or a pointer to the next integer register and a float to the next vector register, each to the stack once its registers are used. It sets `al` to 8, the most vector registers a variadic callee must save. It keeps `rax`, `rdx`, `xmm0`, and `xmm1`, from which a result is read as its type has it come back.

`memory.read` and `memory.write` load and store with alignment 1, after checking for `null_pointer`. The checker allows them only the types that have the `RAW` trait: numbers, pointers, and `foreign type` structures.

A `foreign type` has the layout `sayon_check::types::c_layout` gives it, which both execution modes use: fields in order, each at the next multiple of its alignment, the size rounded up to the largest. It is the layout compiled programs give every structure of those field types, so a C structure is an ordinary LLVM structure. Windows x64 passes a structure of 1, 2, 4, or 8 bytes in its argument's register, as an integer of that size, and any other by the address of a copy the caller makes; it gives back one of those sizes in `RAX`, and any other through an address passed before the arguments, which LLVM's `sret` places. Compiled programs follow that, loading and storing the integer at the structure's address; the interpreter writes each structure into a buffer of words and passes the first word or the buffer's address.

System V on x86-64 (Linux) splits a structure of up to 16 bytes into eightbytes, each classed by the fields in it (`sayon_check::types::sysv_eightbytes`): an integer one if any field in it is an integer or a pointer, a vector one if all are floats. Each eightbyte goes in the next register of its class; when the registers left cannot take them all, the whole structure goes on the stack, as a structure over 16 bytes always does. A result of up to 16 bytes comes back in `rax` and `rdx` or `xmm0` and `xmm1`, an eightbyte in each by its class, and a larger one through an address the caller passes in the first integer register. Compiled programs declare such a C function with an LLVM type for each eightbyte (an integer of its size, `float`, `<2 x float>`, or `double`), which puts it in the register of its class, and with `byval` and `sret` for what goes in memory; a result in two registers comes back as an LLVM structure of the two. The interpreter assigns the eightbytes as it does other arguments, and reads a result's from the registers it kept. WebAssembly's basic C convention passes and gives a structure holding a single number or address, with no padding and however deeply nested, as that value, and any other structure by address: an argument as the address of a copy, which is what C's `byval` comes to there, and a result through an address the caller passes first (`sret`). Other targets' conventions are still to come, so compiled programs there raise `ForeignError` instead of calling a C function that takes or gives a structure.

A function that C calls back goes to C as the address of an entry point for its parameter. A compiled program defines one for each parameter of a C function that is a function, `@sayon.callback.F.P`, with C's convention for the parameter's type; before the call it records the addresses of the call's function values in a frame of the runtime (`sayon_callbacks_enter`), and the entry point finds its value in the newest frame of its C function, so that calls that nest, of the same C function or of others, each find their own. The entry point calls the function value as any call of one does, and on failure keeps the error in the frame and gives C 0; the frame then gives no value, so later calls give 0 without running it, and `sayon_callbacks_leave` raises the error after the call. The interpreter has 64 entry points written in assembly, 16 bytes apart, each putting its number in a register before a routine they share. On Windows x64 the routine stores the integer argument registers in the 32 bytes the caller reserved for them, below the arguments on the stack, so that every argument is in one array of words, and stores the float ones beside it; on Linux x86-64 it stores the six integer and the eight vector argument registers in arrays of their own, beside the address of the arguments on the stack, and the interpreter takes each parameter from them by the same rules as a call; it then calls the interpreter, which runs the function value on the machine making the call, from the frame below the call, and gives its result in both result registers. While a C function that takes functions runs, its task's waits fail with `CallbackError`, as they do with `LockError` while it holds a lock.

## Sockets

Sockets are entries of the stream table, beside files, so a connection is released and shared like a file, and the stream primitives read and write both. Every socket is non-blocking. An operation that cannot finish yet records what it waits for (a socket to read or write, or a connection being made) and returns the internal error `Blocked`; the scheduler then lets the task wait, as for a channel, and runs the operation again when the socket may be ready. So each operation is written to be run again: a read takes data out of the connection's buffer only when it returns it, and a write appends its bytes to the connection's output only once, when less than a megabyte is waiting. The output goes out as the socket takes it, on each operation of the connection and whenever the scheduler waits; what is left when a connection is dropped is sent with a short wait.

When no task can go on but some wait for sockets, the scheduler waits in the system's `poll` (`WSAPoll` on Windows) for any of their sockets, and those with output, until the earliest timer or socket deadline. A socket's timeout counts from when its operation first waited; past it, the operation fails with `TimeoutError`. Making a connection blocks, to find the host's addresses and then to connect, so it runs on a thread of its own, which wakes the scheduler through a loopback datagram socket of the scheduler's poll. In the interpreter a waiting primitive leaves its frame at the operation, as a channel operation does; in compiled programs `sayon_host_call` waits in a loop on the task's own stack.

Making a connection tries the host's addresses in turns of their families, IPv6 and IPv4, starting a new attempt, each on a thread of its own, every quarter of a second until one connects, and taking the first that does; the timeout covers them all.

TLS comes from the system, loaded the first time a connection starts TLS: SChannel on Windows, through `secur32.dll`, and OpenSSL on Linux, through `libssl.so.3`, which the runtime opens with `dlopen`, so that a program that never uses TLS does not need it. Android lets an app load no TLS library of the system's, so there a session is Java's `SSLEngine` from `SSLContext.getDefault()` (Conscrypt, with the system's and the user's trusted certificates), its endpoint identified as for HTTPS, reached through the JNI (`crates/runtime/src/jni.rs`): the activity gives the runtime its Java VM, a thread attaches the first time it calls into Java and detaches as it ends, and each call runs in a frame of local references. The engine wraps and unwraps through direct byte buffers the runtime fills and reads in place, a record at a time, running the tasks it delegates; Conscrypt's exceptions, with their causes, become the messages the other systems give. A program run from `adb shell` has no Java VM, so no TLS. A connection holds its session, which never touches the socket: `receive` adds what arrives to the session's encrypted input, and the session decrypts every whole record into the connection's input, where reading finds it as before; `write` encrypts into records in the connection's output. The handshake runs in `secure`, which gives SChannel what has arrived and sends what it puts out, and like any operation of a connection, returns `Blocked` when it must wait and runs again from where it was. SChannel checks the certificate against the system's store and the name given, which also goes in the handshake. OpenSSL reads and writes only two memory buffers of the session, never the socket; it checks the certificate against the system's default store and, with `SSL_set1_host`, the name given, or the address when the name is one. Its failures are reported with the same messages as SChannel's, from the result of its check of the certificate. A record that starts a handshake again, as a TLS 1.3 server does to send tickets, makes the session go back to the handshake before decrypting more. Closing or finishing sending first sends the message that ends the session.

The HTTP client is Sayon code in `standard/network`, over connections. In a web page, a platform branch of `request` calls `host_fetch` instead (see [Targets](#targets)).

## Graphics

A canvas is an entry of the stream table, like a file, holding red, green, blue, and alpha bytes, so it is released and counted as a stream is, and each drawing operation is a host primitive over it (`crates/runtime/src/graphics.rs`): the interpreter and compiled programs draw with the same code, which uses integers and IEEE arithmetic only and no platform's renderer, so the pixels are the same on every target. Blending is source over, with integer rounding. A circle covers the pixels whose doubled offsets from its center pixel fall within its doubled radius; a polygon is filled by the even-odd rule at pixel centers, its points at pixel corners; a line of width 1 is Bresenham's, a wider one the band around the centers it joins. The built-in font, `crates/runtime/src/font_table.rs`, is generated by `scripts/font_table.py` from font8x8's headers, in the public domain, and text scales its 8 by 8 cells by sampling the nearest bit.

A window (`crates/runtime/src/window.rs`) only puts a canvas on the screen and turns what happens to it into events, which cross to the library as strings. Each frame is the canvas over black, cut or padded to the surface's size, in the byte order the system wants. On Windows a window of the class `SayonWindow`, with the process aware of each monitor's density (per monitor, version 2), keeps the last frame and puts it on the window with GDI's `StretchDIBits`, on `present` and on `WM_PAINT`; its procedure queues events, and `events` and `present` handle the thread's waiting messages. On Linux, Xlib is opened when a window is (`libX11.so.6`), as OpenSSL is, a display for each window; frames go with `XPutImage`, from an `XImage` of the runtime's memory that `XInitImage` completes, and `WM_DELETE_WINDOW` is the close button. In a web page the glue adds a canvas to the page, its backing store the CSS size times `devicePixelRatio`, gathers its pointer, key, focus, and visibility events, and draws a frame with `putImageData`. In an Android app the activity's callbacks keep its native window and its density, and turn its life and its input into events; a frame locks the window's buffer (`ANativeWindow_lock`) and fills it, and opening waits for the window, which Android makes once the app is in front. `present` in the library waits for the next frame with the scheduler's sleep, and in a page always waits at least a millisecond, since the page draws and takes input only while the program waits. A window belongs to the thread that opened it; the others' uses fail.

## Programs

A program started with `os.start` has a pipe to each of its standard streams, and each pipe a thread of the runtime: one reads what the program writes into a buffer, waiting while a megabyte of it is unread, and one writes what is queued for the program's input. Another thread waits for the program to exit and records its status. An operation that must wait for one of them returns `Blocked` as a socket's does, and the threads wake the scheduler through its waking socket when they have done something a task may wait for. Reading all of either output at once lifts the limit on both, so that `os.run`, which reads one and then the other, never leaves the program waiting to write the second. Stopping a program uses its handle (`TerminateProcess`) or its number (`kill`), which stay its own until its status is recorded.

## Time zones

A zone's offset at a moment comes from the system's database. On Windows the runtime loads `icu.dll`, which Windows ships with the IANA database, checks the name with `ucal_getCanonicalTimeZoneID`, and reads the offsets of a calendar of the zone set to the moment. Android keeps the database in one file that Bionic reads, `tzdata`: "tzdata" and its version, the places of an index and of the data, an entry of 52 bytes for each zone (its name in 40, where its data starts, and its length), then each zone's TZif data. The runtime reads the newest, the time zone module's (`/apex/com.android.tzdata/etc/tz/tzdata`, an APEX that updates apart from the system), then one an older update left (`/data/misc/zoneinfo/current`), then the system's, and parses a zone's data as a TZif file; the phone's own zone is the property `persist.sys.timezone`. Elsewhere, or wherever `TZDIR` names a directory, it reads the zone's TZif file once: the transitions it lists, and the POSIX rule after them (such as `CET-1CEST,M3.5.0,M10.5.0/3`), which gives the offsets of the years past the last transition. A local time is turned into a moment in `standard/time` itself, from the offsets a day before and a day after it.

## Typed JSON

`json.from_json<T>` and `json.to_json<T>` contain `typed_decode` and `typed_encode`, built-ins only `standard/json` can call. They require the `JSON` trait, which, like `CLONE`, is recorded on an explicit type parameter, so a call of `json.decode<T>` is checked against its caller's type. The trait holds for numbers, booleans, strings, `nothing`, arrays and string-keyed dictionaries of such types, structures and enums whose fields have it, and unions whose members read disjoint kinds of JSON value (an enum reads text and objects; `Json` reads every kind).

When a generic function is lowered for a type, each built-in becomes a call to a generated function for that type: one reader and one writer per type in the program, made the first time it is needed. A reader takes the `Json` value and its path, handles one level of the type, and calls the readers of the parts, so recursive types need no special case. It calls private functions of `standard/json` to fail with a message, and the host to read numbers exactly: `json_integer` and `json_unsigned` check the digits against the target type's range, so an `unsigned` keeps all 64 bits. Values are built in temporaries of the reader's scopes, so a failure part way destroys what was built.

## Unicode

`crates/runtime/src/unicode_tables.rs` is generated by `scripts/unicode_tables.py` from the Unicode Character Database carried by the Python `regex` package, whose version pins the Unicode version (17.0.0). It holds, for each run of code points with other than the default properties, the Grapheme_Cluster_Break value, Extended_Pictographic, and Indic_Conjunct_Break, searched by binary search. `graphemes.rs` applies the rules GB3 to GB999 of Unicode Standard Annex #29 in one pass, remembering only what rules GB9c, GB11, and GB12 to GB13 look back for. The same script writes `crates/runtime/tests/graphemes.txt`, about 6,000 texts split into clusters by the `regex` package's own implementation; the runtime must split each the same way.

## Streams

A `File` holds a `HostStream`, a type only the standard library can name: in both execution modes a 64-bit number naming an entry of the runtime's stream table, `0` once moved. An entry counts the values that refer to it. `file.lines()` adds one for its iterator, destroying a value removes one, and the last closes the file, writing its buffer without a report; `close` writes the buffer, reports a failure, and leaves the entry closed for the references that remain.

`clone` requires the `CLONE` trait, which every type has but `HostStream` and the types holding it. In an explicit generic function, cloning a value of a type parameter records the trait on that parameter's bounds when the body has been checked, so calls are checked like any bound. A closure's type does not show what it captures, so cloning a closure that captured a file refers to the same stream. Numbers are never reused, so a stale reference can never reach another file. Readers and writers are buffered; a line read keeps at most the limit and a carriage return, plus one buffer of 8 KiB. `io.input()` makes an entry that reads through the process's own standard input buffer, so it and `io.read_line` never lose each other's data; reading it first writes out what the program has printed. When the program ends, every writer still open is flushed. Native programs count each stream reference as an allocation in the `SAYON_DEBUG_ALLOC` report.

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

## Targets

`crates/target` holds the table of targets: each one's name, LLVM triple, platform, processor, address size, how far its support has come, and the capabilities it provides. It depends on nothing, so the checker, the code generator, and the command line share it, and the runtime can later. The short names map to fixed entries. `sayon build` builds for the computer's own target, for WASI and web pages, and for Android; any target can be checked. The lowered module carries its target, so the code generator lays out addresses and C types, picks the C calling convention, and names the entry point for the target, not for the computer it runs on.

A WASI program is the LLVM IR compiled by clang for `wasm32-wasip1` and linked by `wasm-ld` with the runtime built for that target and with wasi-libc's `crt1-command.o` and `libc.a`, given by name (`-nostdlib`), since clang's own WASI setup expects a WASI sysroot and a WebAssembly compiler-rt that LLVM's Windows installer lacks; the runtime's archive holds Rust's `compiler_builtins` instead. wasi-libc's startup calls `__main_argc_argv`, the name C compilers give a `main` with arguments, so that is the entry's name there. The stack lies in linear memory, 8 MiB as on Linux. Its tasks take turns without fibers (see [Tasks](#tasks)).

A web page's program is the IR compiled for `wasm32-unknown-unknown` and linked with the runtime built for it, with no C library and no start (`--no-entry`), exporting its memory, `sayon_run`, and `sayon_wait_milliseconds`. Rust's standard library is there, but its output does nothing and its clocks fail, so the runtime writes, reads clocks, takes random bytes, and ends the program through `console` and `clock`, which elsewhere are the system's and there call the host functions of the import module `sayon` (`host_write`, `host_monotonic`, `host_unix_time`, `host_random`, `host_exit`, `host_option`); a fault inside the runtime is reported through them too. `crates/codegen/src/web/glue.js` is the ES module the compiler writes beside the module: it gives those functions from the page (or Node), calls `sayon_run`, and while it gives -1 waits `sayon_wait_milliseconds` with a timer and calls it again, so the page is never blocked; `host_exit` throws, which stops the program and gives its status, and a trap gives 3. There `main` is `@sayon.main`, a coroutine when the program waits anywhere, which leaves its exit status in a global since its own status says only whether it finished; when every task sleeps the scheduler records how long, `sayon_tasks_wait_main` gives 2, `main` stops, and `sayon_run` gives -1, then, once the timer fires, resumes `main`, which runs the tasks on through `sayon_tasks_go_on`. The allocation report waits until `main` has freed its frame.

HTTP in a page goes through the browser's `fetch`. `host_fetch`, the one primitive that waits for the host, is lowered like a task's waiting operation: `sayon_host_call` gives the request to the glue (`crates/runtime/src/fetch.rs`), which starts fetching it and gives its number, and the task waits for the request to settle, as for a timer. While no task can go on and some wait for responses, the scheduler gives the browser no timer, or the earliest sleeper's (`sayon_wait_milliseconds` gives -1 for none), and the glue's wait also ends when a response arrives. The glue reads the whole body, restarting the timeout with each part and stopping past the limit, and settles the request with its status and its parts as bytes, or with why it failed; the runtime copies them into a stream entry holding the head and the body, which `host_fetch_head` and `host_fetch_body` read, and tells the request's number it is done with. A task cancelled while it waits abandons the request, which the glue aborts.

An Android program is compiled and linked by the NDK's clang, which knows Android's C library (Bionic), for the triple with the oldest API it runs on (`x86_64-linux-android26`), with segments aligned to 16 KiB pages, and with `log`, `android`, `dl`, and `m`. Run from `adb shell` it is an executable whose `main` the C library calls. In an app it is `libmain.so`, linked with `-shared` and keeping the runtime's `ANativeActivity_onCreate` (`crates/runtime/src/android.rs`), which Android's `NativeActivity` calls on the app's main thread: it takes the activity's input events and leaves them to Android, so that the app answers its user, and calls the program's `main` on a thread with an 8 MiB stack. The runtime's console then writes to Android's log, a whole line per message and long lines in parts, and when `main` returns it logs the status and finishes the activity; once the activity is destroyed, the process ends with `_exit`, which unlike `exit` runs no destructors of the libraries Android loaded while their threads go on, so that the next launch runs the program afresh. Packaging (`crates/codegen/src/android.rs`) writes the manifest, has `aapt2` link it against the platform, adds the library to the package as a stored entry aligned to 16 KiB, and every other stored entry to 4 bytes, as `zipalign -P 16` would (`crates/codegen/src/zip.rs`), and signs it with `apksigner`, keeping that alignment, with the shared debug key, which `keytool` makes the first time.

Clang is given `-fno-crash-diagnostics`: should it fail on a program's IR, the report it would write tries to preprocess every input, libraries too, and only takes long.

The checker holds the target. A name `platform` or `processor` that no local, item, or import of the module declares is the target's string, and `supports("name")` called where `supports` is not declared is the target's boolean. When checking an `if`, a condition made only of those, compared with string literals and joined by `and`, `or`, and `not`, is evaluated first: the selected branch is checked as a plain block and the others are skipped, so the program the later phases see has only the selected code. A literal that is no platform, processor, or capability is reported while evaluating, on every side of `and` and `or`.

After checking, `sayon_check::usage::needs` walks the program from its entry, its constants' initializers, and its finally functions, following each call, function value, and closure to the functions they name, breadth first. Each built-in and host primitive maps to a capability, or to none for those every target has; a C call needs `foreign`. A function of the program's own reports its operations where they are; one of the standard library or a dependency reports them where the program's own code first led to it, which the walk carries from caller to callee. The driver reports each capability the target lacks once, at its first such place. Linking passes only the libraries of the C functions the lowered program calls, so a `foreign` block for another platform's library costs nothing when its branch is not selected.

## Toolchain

| Component | Version | Notes |
|---|---|---|
| Rust | 1.98.1 | pinned in `rust-toolchain.toml` |
| LLVM / clang | 23.1.4 on Windows x64, 21.1.8 on Linux x86-64 | found on `PATH`, in the default install location, or through `SAYON_CLANG`; `wasm-ld` beside it links WASI programs |
| C runtime | static (`+crt-static`) on Windows; the system's glibc on Linux (validated with 2.43) | compiled programs import only system DLLs on Windows, and link `libpthread`, `libdl`, and `libm` on Linux |
| OpenSSL | 3 (validated with 3.5.5) | Linux only, for TLS; opened when a program first uses TLS |
| wasi-libc | as Rust 1.98.1's `wasm32-wasip1` target ships it | linked into WASI programs; installations carry it in `lib/wasm32-wasip1` with a notice of its license |
| wasmtime | 50.0.0 | the pinned WASI host the tests run WebAssembly programs under |
| Node | 24.22.1 | runs web programs through their glue in the tests |
| Android SDK | NDK r30 (30.0.16248370, clang 21), build tools 37.0.0, platform `android-36`, platform tools 37.0.1, emulator with the `android-36` Google APIs x86-64 image | builds Android programs, found through `ANDROID_HOME` or where Android Studio puts it (`%LOCALAPPDATA%\Android\Sdk`); the tests run them on the AVD `sayon` |
| JDK | Eclipse Temurin 21.0.12.1 | runs `apksigner` and `keytool`, through `JAVA_HOME` |
| Xlib | `libX11.so.6` (WSLg's X server under WSL 2) | Linux only, for windows; opened when a program first opens one |
| Browsers | Edge 156 (the system's), Chrome for Testing 156.0.8078.47 (unpacked), Firefox 158.0.1 (extracted from its installer, not installed) | the tests run each web program in them, headless; found where they install, in `%LOCALAPPDATA%\Programs\chrome-for-testing` and `firefox\core`, or through `SAYON_BROWSERS` |

Native builds target the host. The runtime library is found next to the `sayon` executable or in `../lib`, or through `SAYON_RUNTIME_LIB`; another target's, with any C library it needs, in `../lib/<triple>`: an installation's `lib`, or `target/lib` for a development build, where the WASI tests put it.

## Verification

- `cargo test` runs the lexer, parser, checker, ownership, IR, runtime, and interpreter unit tests, then every program under `tests/programs` in both execution modes, comparing output, errors, and exit status with the recorded expectations, and every program under `tests/errors`, comparing its diagnostics.
- Native runs set `SAYON_DEBUG_ALLOC`, and each program must report zero live runtime allocations at exit, including programs that end with an uncaught error.
- The WASI check builds the runtime for `wasm32-wasip1` (in `target/wasi-runtime`, so it never waits for the cargo running the tests), puts it with wasi-libc in `target/lib/wasm32-wasip1`, builds every program under `tests/programs` with `--target wasi`, and runs each under wasmtime with the test folder given to it, comparing like the native runs. A program that needs a capability WASI lacks is reported by the build (`E_CAPABILITY`) and listed as left out. `SAYON_SKIP_WASI=1` skips the check, saying so.
- The web checks build every program under `tests/programs` with `--target web`, with the runtime for `wasm32-unknown-unknown` in `target/lib`, and compare each like the native runs: in Node, through `tests/web/node.mjs`, and in each browser found, headless, from an HTTP server the test runs on this computer, where `tests/web/runner.html` runs each program through its glue and posts what it wrote back. `SAYON_SKIP_WEB=1` and `SAYON_SKIP_BROWSERS=1` skip them; with no browser found, the browser check says it skipped.
- `tests/programs/http.sayon` makes its requests to a server the checks run at `127.0.0.1:47613`, the same for every way a program runs, since a page has no other way to learn where it is. It answers pages of any origin (CORS), so the browsers' requests go to it from the runner's page.
- The Android check leaves `tls.sayon` out of its shell runs, since a shell has no TLS there, and runs it as an app; `tests/online/https.sayon`, run as an app on the emulator, gives what it gives on Windows, certificate failures included.
- The Android check builds the runtime for `x86_64-linux-android` and every program under `tests/programs` with `--target android-x64 --executable`, pushes each to the device, and runs it from `adb shell`, comparing like the native runs; `adb reverse` gives the device the HTTP test server at its own loopback address. Then it builds a few as apps, installs and starts each, and compares what they logged and their status. It does the same for `android-arm64` when the device runs ARM code, as the x86-64 emulator does through translation, leaving out the program that passes C structures. It uses the first x86-64 device `adb` lists, or starts the emulator of the AVD `SAYON_AVD` (by default `sayon`), headless, and stops it after; a cold boot on a small computer takes many minutes. `SAYON_SKIP_ANDROID=1` skips it; without an SDK or a device, it says it skipped.
- `tests/programs/graphics.sayon` draws on canvases and prints pixels and digests of them, so every check compares the pixels. The programs under `tests/windows` open a window, show frames, and close it: the window check runs them on this computer's screen, in the interpreter, compiled, and in each browser, when there is a screen (on Linux, an X display, which WSLg gives); the Android check runs them as apps.
- Set `SAYON_BLESS=1` to record new expectations, and review the differences before committing them.
