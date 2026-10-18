# Error codes

Every diagnostic has a stable code. `sayon explain CODE` prints the entry for a code; this file is the same text. Codes starting with `E_` are errors, which stop a program from running; codes starting with `W_` are warnings. Errors raised while a program runs have codes such as `KeyError`, listed at the end.

## Source text and layout

### E_INVALID_UTF8

A source file is not valid UTF-8 text; the message gives the offset of the first invalid byte. Save the file with UTF-8 encoding.

### E_UNEXPECTED_CHARACTER

The file contains a character that cannot start any part of a program, such as `$` or `@` outside a string. Remove it, or put it inside a string or a comment.

### E_INVALID_NUMBER

A number is written incorrectly: an underscore that does not separate two digits (`1__000`, `5_`), a prefix without digits (`0x`), a digit its base does not have (`0b102`), a `u` suffix on a float, or an unknown suffix. Prefixes are lowercase: `0xFF`, `0b1010`, `0o755`.

### E_INVALID_ESCAPE

A backslash in a string starts an escape Sayon does not have, or `\u{...}` does not name a Unicode scalar value. The escapes are `\"`, `\\`, `\n`, `\r`, `\t`, `\0`, and `\u{HEX}` with one to six hexadecimal digits. Write `\\` for a backslash.

### E_UNTERMINATED_STRING

A string does not end with `"` on the line where it starts. Strings cannot span lines; join lines with `+`, or use `\n` for a line break inside one.

### E_UNTERMINATED_COMMENT

A block comment that starts with `##` on its own line has no closing `##` line.

### E_EMPTY_INTERPOLATION

A string has `{}` with nothing inside. Put an expression between the braces, or write `{{}}` for literal braces.

### E_SINGLE_BRACE

A string has a `}` that closes no `{`. Write `}}` for a literal closing brace.

### E_LONE_CARRIAGE_RETURN

The file has a carriage return that is not followed by a line feed. Lines end with a line feed or with a carriage return and line feed; save the file with ordinary line endings.

### E_MIXED_INDENTATION

The file indents some lines with tabs and others with spaces, or one line mixes both. A file may use either, but not both; `sayon fmt` rewrites indentation with four spaces per level.

### E_UNEXPECTED_INDENT

A line is indented further than the lines around it, but nothing above it opens a block. Only a header such as `if`, `function`, or `for` is followed by indented lines.

```sayon
print("a")
    print("b")    # error: remove the indentation
```

### E_INCONSISTENT_DEDENT

A line moves left, but not to the indentation of any enclosing block. Line it up with the block it belongs to.

### E_EXPECTED_BLOCK

A header such as `if condition`, `while`, `for`, `function f()`, or `else` must be followed by at least one indented line. For a function literal, a body of one `return` or expression can also stay on the header's line: `function(x) return x * 2`.

### E_SYNTAX

The parser expected something else at this point; the message says what. Typical causes are a missing `)` or `]`, a missing `=`, or two statements on one line.

### E_CHAINED_COMPARISON

Comparisons cannot be chained: `a < b < c` and `a == b == c` are errors. Write `a < b and b < c`.

## Names and declarations

### E_UNDEFINED_NAME

The name is not a variable, function, type, constant, or imported module visible here. Check the spelling, declare it before using it in a script, or import the module that defines it.

### E_DUPLICATE_DECLARATION

A name is declared twice where it must be unique: two parameters of one function, a `variable` declared twice in one block, or a name bound twice in one `case` pattern.

### E_DUPLICATE_DEFINITION

Two functions, types, constants, or imports of one module have the same name. Rename one of them.

### E_UNINITIALIZED

A `variable` or `constant` declaration needs a value: `variable total = 0`.

### E_INVALID_ASSIGNMENT

The left side of `=` is not something that can be assigned: assign to a variable, an element `items[0]`, or a field `point.x`.

### E_ASSIGN_CONSTANT

The assignment would change something that cannot change: a loop variable, a name bound by a `case`, or an imported module's name. Copy the value into a variable first: `copy = item`.

### E_RETURN_OUTSIDE_FUNCTION

`return` appears at the top level of a script. A script ends at its last line; to stop with a status, put the code in `function main() -> integer`.

### E_OUTSIDE_LOOP

`break` or `continue` appears outside any loop.

### E_FINALLY_TRANSFER

A `finally` block runs while the `try` is being left, so it cannot itself leave with `return`, `break`, or `continue`. Move the transfer after the `try`.

### E_MISSING_RETURN

A function returns a value on some paths but can reach its end without returning one. Add a `return` on every path, for example at the end of the function, or annotate it `-> nothing` if it should return nothing.

### E_MAIN_SIGNATURE

`main` takes no parameters and returns `nothing` or an integer, which becomes the exit status. Read arguments with `standard/os.sayon`'s `arguments()`.

### E_TYPE_POSITION

Types and enums are declared at the top level of a file, not inside a function or block.

### E_TEST_POSITION

`test "name"` blocks belong at the top level of a file.

### W_SHADOWED_BUILTIN

A function or variable has the name of a built-in function, such as `print` or `length`, so the built-in cannot be called in this file. Choose another name unless hiding it is intended.

## Types

### E_TYPE_MISMATCH

A value does not have the type its place needs: the message names both types and what required the expected one, such as an argument, a return type, or an annotation. Convert the value (`tostring(n)`, `tonumber(text)`, `float(n)`), or change the annotation.

### E_AMBIGUOUS_TYPE

The compiler cannot determine a type from how a value is used, typically for an empty `[]` that is never filled or a function literal that is never called. Add an annotation: `names: Array<string> = []`.

### E_UNKNOWN_TYPE

An annotation names a type that does not exist. The built-in types are listed in the language reference; declare your own with `type` or `enum`, or import the module that declares it and write `module.Type`.

### E_TYPE_ARGUMENTS

Type arguments are written where they are not accepted, or with the wrong count: `Array` takes one, `Dict` two, a function value none, and a function literal cannot declare type parameters.

### E_UNSUPPORTED_OPERATION

The operation is not defined for the type, such as `+` on booleans, `<` on arrays, or `==` on function values. The message names the type and the operation.

### E_NOT_INDEXABLE

Only arrays, dictionaries, and strings support `value[index]`.

### E_NOT_ITERABLE

`for` loops iterate over arrays, views, dictionaries, strings, ranges, and iterators: values whose type has a `next` method returning `T or nothing`. This error also reports a `next` that takes more than the iterator, returns no `nothing`, or takes the iterator with `own` (a loop calls it again and again, so it needs `update` or a read).

### E_NOT_CALLABLE

The value is not a function, so it cannot be called with `(...)`.

### E_BUILTIN_VALUE

A built-in function such as `print` cannot be used as a value; call it, or wrap it: `show = function(text) print(text)`.

### E_ARGUMENT_COUNT

A call passes more arguments than the function takes, or fewer than its parameters without defaults. A function value always takes all of its parameters.

### E_DEFAULT_ORDER

Parameters with defaults must come after those without, and function literals cannot have defaults.

### E_PARAMETER_MODE

A parameter's mode does not fit how the function is used: a function literal's parameter does not match the `own` or `update` of the function type expected there, or a named function used as a value changes or keeps a parameter that its declaration does not mark `update` or `own`. Write the mode the message suggests.

### E_LITERAL_RANGE

An integer literal does not fit its type, such as `300` for an `integer8` or `-1` for an `unsigned`.

### E_LOOP_BINDINGS

A loop names the wrong number of variables: a loop over an array, string, or range takes one, and a loop over a dictionary takes one (the key) or two (key and value).

### E_KEY_TYPE

A dictionary key type is not usable as a key. Keys must stay equal to themselves: integers, booleans, strings, `nothing`, and structures, enums, and unions of those. Floats (`nan` is not equal to itself), collections, function values, and errors cannot be keys. For a float, use a scaled integer or `tostring(value)` as the key.

### E_DUPLICATE_KEY

A dictionary literal gives the same key twice; the later one would replace the earlier.

### E_UNAVAILABLE

The feature is specified but belongs to a later milestone; the message names it. The feature ledger in `docs/features.md` lists what is planned. Generic functions that call each other wait for M3, which limits how many specializations a program may need, since `f<T>` calling `g<Array<T>>` calling `f<Array<Array<T>>>` would need ever more.

## Unions and errors as values

### E_UNCHECKED_OUTCOME

An `Outcome` from `try` may hold the value or the error, so its contents cannot be used until it is tested. Test it with `if result is error`: inside, it is the `Error`; in the `else` branch, it is the value.

### W_UNUSED_OUTCOME

The result of a `try` expression is never inspected, so an error would go unnoticed. Keep the result and test it with `is error`, or leave out `try` so the error propagates.

### W_UNUSED_TASK

A task is spawned and its handle dropped at once, as in a statement `threads.spawn(...)` on its own. A task whose handle is dropped without a `join` or a `detach` is cancelled, and one that has not started then never runs. Keep the handle and `threads.join` it for the result, or `threads.detach(handle)` to let it run on its own until the main task ends.

### E_BORROWED_UNION

An owned value that is only borrowed here is used where a union is expected, which would need a copy. Pass `clone(value)` for a copy, or `move(value)` to hand it over.

## Structures, enums, and patterns

### E_EMPTY_STRUCTURE

A structure type needs at least one field. For a value with no data, use an enum variant without fields.

### E_RECURSIVE_TYPE

A structure or enum contains itself directly, which would make its values infinitely large. Store the nested values in a collection: `children: Array<Node>`.

### E_DUPLICATE_FIELD

A structure declares a field twice, a construction gives a field twice, or a pattern names a field twice.

### E_DUPLICATE_VARIANT

An enum declares two variants with the same name.

### E_MISSING_FIELD

A structure is built without one of its fields. Every field must be given exactly once.

### E_UNKNOWN_FIELD

The structure or variant has no field of that name. The message lists the fields it has.

### E_UNKNOWN_MEMBER

A module has no function, type, or constant of that name, or it is `private`; or a structure has no member of that name.

### E_UNKNOWN_METHOD

`value.name(...)` names neither a field holding a function value nor a method. A method of a type `Point` is a function declared in the same module, named `point_name` or `name`, whose first parameter is declared as a `Point`. Only structures and enums have methods. The value's type must also be known where the method is called, so a parameter used this way needs an annotation, such as `file: io.File`.

### E_RECORD_TYPE

A field list such as `[x = 1.0, y = 2.0]` is used where the compiler cannot tell which structure it builds. Name the type, `Point(x = 1.0, y = 2.0)`, or annotate the binding.

### E_NAMED_FIELDS

A structure is built with positional arguments where its fields must be named, or with a mix of both. Write `Point(x = 1.0, y = 2.0)`.

### E_NAMED_ARGUMENTS

Named arguments are only for building structures: functions take their arguments by position, and enum variants by position.

### E_VARIANT_FIELDS

A variant is used without the values of its fields, or given values it does not have: `circle(1.0)`, but `point` alone.

### E_AMBIGUOUS_VARIANT

Several enums have a variant of this name, and nothing tells which is meant. Write `Shape.circle(...)`.

### E_UNKNOWN_VARIANT

A pattern or expression names a variant (or structure) that does not exist for the matched type.

### E_TYPE_VALUE

A type or enum name is used as a value. Build a value instead: `Point(x = 0.0, y = 0.0)` or `Shape.point`.

### E_PATTERN_ARITY

A variant or structure pattern gives a different number of fields than the type has. Give a pattern for each field, using `_` for the ones you do not need, or name only the fields you need: `Point(x = 0.0)`.

### E_PATTERN_LITERAL

A pattern can compare only with a fixed value. Bind the value to a name and compare in a guard: `case text if text == expected`.

### E_OR_BINDING

The alternatives of an `or` pattern bind different names. Every alternative must bind the same names, with the same types, so the case's body can use them whichever alternative matched.

### E_NONEXHAUSTIVE

A `match` does not handle every possible value; the message lists what is missing. Add cases for them, or end with `case _`. Cases with an `if` guard do not count, since the guard can fail.

### W_UNREACHABLE_CASE

A case or an `or` alternative can never run, because the cases and alternatives before it already match every value it would.

## Ownership

### E_MOVED_VALUE

A variable is used after its value was transferred to another owner: assigned to another variable, passed to an `own` parameter, returned, stored in a collection or structure, or captured by a closure. The message shows where.

```sayon
numbers = [1, 2, 3]
other = numbers
print(numbers)     # error: `numbers` was transferred to `other`
```

Use the new owner, or write `other = clone(numbers)` when two independent copies are needed. Assigning a new value to the variable makes it usable again. A closure that changes a captured variable takes it over even when its value is Copy; have the closure return what you need.

### E_MOVE_BORROWED

The value is only borrowed here, so it cannot be transferred: a read or update parameter, a loop variable over a collection, or a name bound by a `case` that borrows part of the matched value. Use `clone(...)`, or declare the parameter `own`.

### E_MUTATE_BORROWED

The value is borrowed for reading, so it cannot be changed: a read parameter, a loop variable over a collection, a borrowed `case` binding, or a read parameter holding a function value, which calling would need to change. Declare the parameter `update`, or loop over indexes to change elements.

### E_MUTATE_CONSTANT

A constant cannot be changed: a `constant` local, a module constant, or a constant holding a closure whose call could change what it captured. Declare it with `variable`, or copy it: `items = clone(LIMITS)`.

### E_MOVE_CONSTANT

A module constant cannot be transferred, since it keeps its value for the whole program. Use `clone(NAME)` for an independent copy.

### E_MOVE_OUT_OF_ELEMENT

An owned element cannot be moved out of an array or dictionary, which still holds it. Use `clone(items[0])` for a copy, or `pop(items)` to remove the last element.

### E_MOVE_OUT_OF_FIELD

An owned field cannot be moved out of a structure held by a variable or constant, which would leave the structure incomplete. Clone the field, or move the whole structure. A field of a computed value, such as `make_person().name`, can be taken.

### E_MOVE_CAPTURED

A closure cannot transfer a value it captured out of itself, because it can be called again and would no longer have it. Return `clone(value)` instead.

### E_MOVE_TARGET

`move(...)` takes a variable name: `move(items)`.

### E_BORROW_CONFLICT

One value is used in two conflicting ways at once:

- a collection or string is changed, moved, or replaced while a `for` loop over it is running;
- an array is changed or moved while a view of it is still used later, or used at all while a mutable view of it is;
- a matched variable is changed while a case that borrows its parts runs;
- one call receives the same variable twice, once for changes and once in any other way.

```sayon
for item in items
    push(items, item)    # error: the loop is still reading `items`
```

Collect the changes in another collection and apply them after the loop, or pass a clone.

### E_IMMUTABLE_STRING

Strings cannot be changed in place, so `text[0] = "x"` is an error. Build a new string with `+`, interpolation, or `strings.slice`.

### E_VIEW_ESCAPE

A view is kept somewhere it could outlive the array it borrows: in a collection, structure, enum (including an `Outcome` from `try`), or union, in a closure's captures, in a constant, or as a function value's result. Keep a view in a variable or pass it to a function; for something to keep, `clone(view)` makes an array of its elements.

### E_VIEW_RETURN

A function returns a view that does not borrow exactly one of its parameters: a view of its own variable, which is destroyed when it returns, views of different parameters on different paths, or a view whose source is unclear. Return a view made from one parameter, or `clone(...)` of it.

```sayon
function first_two(items: Array<integer>) -> View<integer>
    return view(items, 0, 2)          # fine: it borrows the parameter
```

### E_VIEW_TEMPORARY

A view is made of an array or string that is not stored anywhere, such as `view([1, 2, 3])`, which is destroyed at the end of the statement. The same holds for a function that returns a view of its argument, such as `first_half(["a", "b"])` or `strings.byte_view("text")`. Store the value in a variable and make the view of that.

### E_SHARED_IMMUTABLE

A shared value is changed, borrowed for changes, or taken out of its handle. Every handle refers to the same value, so none may change it or take it. Keep the parts that change in a `Cell`, as `Shared<Cell<T>>`, and change them with `shared.value.set(...)`; copy the value out with `clone(shared.value)`.

### E_SHARED_CONSTANT

A constant would hold a `Shared`, `Weak`, or `Cell` value. A constant is one value for the whole program and never changes; make the shared value in `main` or a function and pass it where it is needed.

## Modules and projects

### E_IMPORT_NOT_FOUND

The imported file does not exist. Import paths are relative to the program's root, the directory of the file you run (or the project's root), whichever file the import appears in, and include the extension: `import("modules/geometry.sayon")`. Standard modules start with `standard/`.

### E_IMPORT_PATH

An import takes a literal path string naming a `.sayon` file, written with `/` on every system, inside the program's root: `..`, `.`, and absolute paths are rejected.

### E_IMPORT_CASE

The import path's letter case differs from the file's name on disk. Match the spelling exactly, so the program works on every operating system.

### E_IMPORT_POSITION

Imports must be bound to a name at the top of a file: `geometry = import("geometry.sayon")`.

### E_IMPORT_CYCLE

Modules import each other in a cycle. Move the shared parts into a module that both import.

### E_MODULE_STATEMENT

An imported module can only define functions, types, constants, and imports. Statements that would run on import are rejected; move them into a function.

### E_MODULE_VALUE

A module name is used as a value. Use one of its members: `geometry.area(...)`.

### E_PRIVATE

The function, type, or constant is `private` to its module and cannot be used from another one.

### E_INITIALIZATION_CYCLE

A module constant depends on itself, directly or through other constants or functions, so it can never be computed.

### E_MANIFEST

`sayon.toml` is invalid: a key is unknown or misspelled, a value has the wrong type, or a package name or version is malformed. The message points at the problem.

## Lint warnings

`sayon lint` reports these in addition to the compiler's own diagnostics. A comment naming a warning on its line, or on the line above, suppresses it: `# allow(unused_variable)`. The name is the code without `W_`, in lowercase; several names are separated by commas.

### W_UNUSED_VARIABLE

A variable, loop variable, `case` binding, or caught error is never read. Remove it, or start its name with `_` when it is deliberately unused, such as `for _ in range(3)`.

### W_UNUSED_PARAMETER

A parameter is never used in the function's body. Remove it, or start its name with `_` when the function must take it, for example to fit a callback's function type.

### W_UNUSED_IMPORT

A module is imported but none of its members is used. Remove the import.

### W_UNUSED_PRIVATE

A `private` function, type, or constant is never used in its module, and no other module can use it.

### W_NAMING

A name does not follow the conventions: variables, functions, parameters, fields, and enum variants are written in `snake_case`; types and enums in `PascalCase`; module constants in `UPPER_SNAKE_CASE`. The help suggests the conventional spelling.

### W_SHADOWED_VARIABLE

A new variable, parameter, loop variable, or binding has the name of a variable that is still visible, so the outer one cannot be used where the new one is. Choose another name.

### W_SUSPICIOUS_DECLARATION

An assignment inside a block makes a new variable whose name is one letter away from a variable of an enclosing block. This is usually a typo for that variable, which then keeps its old value:

```sayon
total = 0
for price in prices
    totl = total + price    # makes `totl`; `total` stays 0
```

Spell the name exactly to change the variable; write `variable name = ...` when a new variable is intended.

### W_UNKNOWN_LINT

An `# allow(...)` comment names a warning that does not exist, so it suppresses nothing.

## Runtime errors

These are the codes of errors raised while a program runs. `try` and `catch` can handle them; `problem.code` gives the code.

### UserError

Raised by `error(message)` in the program.

### AssertionError

An `assert(condition, message)` found its condition false.

### BoundsError

An array index or string position is outside the collection: valid positions are `0` up to `length(value) - 1`.

### KeyError

A dictionary has no entry for the key being read. Test with `haskey(dict, key)` first, or capture the error with `try`.

### OverflowError

Integer arithmetic produced a result outside its type's range. Use a wider type, such as `integer` instead of `integer32`, or check the operands first.

### DivisionByZeroError

An integer division or remainder had a zero divisor.

### ConversionError

A numeric conversion cannot represent its value, such as a float that is not finite, or a number outside the target type's range.

### ParseError

Text could not be read as what was asked for: `tonumber(text)` was given text that is not a number (the whole text must be a number, without spaces), or `json.parse` was given text that is not JSON. A JSON error names the line and column where reading stopped.

### ChannelClosedError

A `threads.send` on a closed channel, whose message is then destroyed, or a `threads.receive` (or `select`) on a closed channel that has no messages left. `threads.receive_result` tells the end of a channel apart without an error.

### DeadlockError

A task waited for something no task could ever provide: every task was waiting, and none on a timer. For example, a `threads.receive` from a channel nothing else sends on, or a `join` of tasks that wait for each other.

### DecodeError

JSON did not fit the type `json.decode` or `json.from_json` was reading it into: a value of the wrong kind, a structure field that is missing, a name that is not a variant of the enum, or an integer with a fraction or outside its type's range. The message starts with the path to the place in the JSON value, such as `$.points[2].x`, and says what was expected and what was found.

### RangeError

`range(...)` was called with a step of zero.

### ValueError

An argument has a value the operation cannot accept; the message says which.

### IOError

A file or directory operation failed, for example because the file does not exist or cannot be written. The message includes the reason from the operating system.

### EncodingError

A file read as text is not valid UTF-8.

### InternalError

The compiler or runtime reached a state it should never reach. This is a bug in Sayon; please report it with the program that caused it.
