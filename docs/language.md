# The Sayon language

This reference describes the language as currently implemented. Features scheduled for later milestones are listed in [features.md](features.md); using one produces an `E_UNAVAILABLE` error that names its milestone.

## Source files

- Source files use the `.sayon` extension and must be valid UTF-8. A leading byte order mark is accepted.
- Lines end with LF or CRLF. A carriage return on its own is an error.
- Indentation defines blocks. Four spaces per level is the convention. Tabs are accepted and count as four columns, but one file must not mix tabs and spaces for indentation.
- Blank lines and comment-only lines do not affect indentation.
- A statement ends at the end of its line, except inside parentheses or brackets, where line breaks are ignored.

```sayon
total = add(1,
            2)
if total > 2
    print("big")
```

### Comments

`#` starts a comment that runs to the end of the line. A line containing only `##` opens a block comment, and the next such line closes it. Block comments do not nest.

```sayon
# A line comment
##
A block comment
##
```

### Names and keywords

Names start with an ASCII letter or underscore and continue with letters, digits, or underscores. By convention, variables and functions use `snake_case`, types use `PascalCase`, and constants use `UPPER_SNAKE_CASE`.

Keywords are always single words, such as `elseif` (never `else if`). Reserved words: `and`, `async`, `await`, `break`, `case`, `catch`, `constant`, `continue`, `else`, `elseif`, `enum`, `false`, `finally`, `for`, `foreign`, `function`, `if`, `import`, `in`, `is`, `match`, `not`, `nothing`, `or`, `own`, `private`, `public`, `return`, `true`, `try`, `type`, `unsafe`, `update`, `variable`, `while`.

## Literals

| Literal | Examples | Type |
|---|---|---|
| Integer | `42`, `1_000_000`, `0xFF`, `0b1010`, `0o755` | `integer` unless the context needs another integer type |
| Unsigned | `255u` | `unsigned` unless the context needs another unsigned type |
| Float | `3.14`, `2.5e-2`, `1e3` | `float` unless the context needs `float32` |
| Boolean | `true`, `false` | `boolean` |
| Nothing | `nothing` | `nothing` |
| String | `"text"` | `string` |
| Array | `[1, 2, 3]`, `[]` | `Array<T>` |
| Dictionary | `["name" = "Ada", "city" = "London"]`, `[1 = "one"]` | `Dict<K, V>` with the keys' and values' types |

Underscores in numbers must sit between digits. Number prefixes are lowercase. An integer literal never becomes a float: `x: float = 1` is an error; write `1.0`.

A literal's value must fit its type, so `small: integer8 = 200` and `positive: unsigned = -1` are compile errors.

### Strings

Strings are immutable UTF-8 text in double quotes and must close on the same line. Escapes: `\"`, `\\`, `\n`, `\r`, `\t`, `\0`, and `\u{HEX}` with one to six hexadecimal digits naming a Unicode scalar value.

Braces interpolate an expression. Doubled braces are literal braces.

```sayon
name = "Sayon"
print("Hello, {name}!")          # Hello, Sayon!
print("Sum: {100 + 20}")         # Sum: 120
print("Braces: {{{name}}}")      # Braces: {Sayon}
```

Positions in a string count Unicode characters (scalar values), as `length` does; `bytelength` counts UTF-8 bytes. `text[i]` is the character at position `i` as a one-character string, and `for character in text` visits each character in order. A position outside the string raises `BoundsError`. Strings cannot be changed in place (`E_IMMUTABLE_STRING`); build a new one with `+`, interpolation, or the `standard/strings.sayon` functions such as `slice`.

```sayon
word = "héllo"
print(word[1], length(word), bytelength(word))   # é 5 6
for character in word
    print(character)
```

Indexing a string walks it from the start, so `text[i]` takes time proportional to `i`; a `for` loop visits every character in one pass.

### Arrays and dictionaries

An array literal whose elements have different types gets the union of those types:

```sayon
mixed = [1, "text", true]        # Array<integer or boolean or string>
```

An empty literal `[]` takes its type from an annotation or from later use, such as a `push`. Dictionaries keep insertion order. Replacing a key's value keeps the key's position.

Dictionary keys are compared by value. A key can be an integer, a boolean, a string, `nothing`, or a structure, enum, or union made only of those; floats, collections, function values, and errors cannot be keys (`E_KEY_TYPE`), since a key must stay equal to itself. A key is copied into the dictionary.

```sayon
type Point = [x: integer, y: integer]

squares = [1 = 1, 2 = 4]
grid: Dict<Point, string> = []
grid[Point(x = 0, y = 0)] = "origin"
print(squares[2], grid)       # 4 [Point(x = 0, y = 0) = "origin"]
```

In `[name = value]` a bare name is a structure field, unless a dictionary is expected there: then the name is evaluated as a key, so `directions: Dict<Direction, integer> = [north = 0, south = 180]` uses the variants `north` and `south`.

## Bindings and scope

```sayon
count = 0                  # introduces count, or updates an existing count
variable total = 0         # always introduces total in this block
constant LIMIT = 10        # cannot be reassigned or changed
ratio: float = 0.5         # introduces ratio with a fixed type
```

- `name = value` updates the nearest visible binding with that name, or introduces a new one in the current block when none exists.
- `variable` and annotated declarations always introduce a new binding. Declaring the same name twice in one block is an error.
- Every binding is initialized where it is declared, and becomes visible after its initializer.
- Blocks (`if`, `while`, `for`, `try`, and so on) have their own scope.
- `constant` at the top level of a file declares a module constant, which every function can read. Module constants are initialized before the program starts, in dependency order. A constant cannot depend on itself.
- Other top-level bindings belong to the script and are not visible inside functions; pass them as arguments.

Compound assignment (`+=`, `-=`, `*=`, `/=`, `%=`) evaluates its target once. In an assignment to an element or field, the value is computed before the place it is stored in.

## Types

| Type | Meaning |
|---|---|
| `integer` (`integer64`) | Signed 64-bit integer |
| `integer8`, `integer16`, `integer32` | Smaller signed integers |
| `unsigned` (`unsigned64`) | Unsigned 64-bit integer |
| `unsigned8` (`byte`), `unsigned16`, `unsigned32` | Smaller unsigned integers |
| `float` (`float64`), `float32` | IEEE floating point |
| `boolean` | `true` or `false`; nothing else is treated as true or false |
| `string` | Immutable UTF-8 text |
| `nothing` | The absence of a value |
| `Array<T>` | A growable sequence |
| `Dict<K, V>` | An insertion-ordered dictionary from keys of type `K` to values of type `V` |
| `A or B` | A value of one of the member types |
| `Range` | The result of `range(...)` |
| `Error` | A recoverable error value |
| `Outcome<T>` | The result of a `try` expression: `success(value)` or `failure(problem)` |
| `function(A, B) -> R` | A function value taking `A` and `B` and returning `R`; parameters may be marked `own` or `update` |
| structures | Declared with `type`: named fields |
| enums | Declared with `enum`: one of several variants, each with its own fields |

### Type inference

Annotations are optional. The type of every binding and the parameter and result types of every function are inferred from how they are used. Functions are generic when their code works for more than one type:

```sayon
function identity(x)
    return x

function add(a, b)
    return a + b

print(identity(42), identity("text"))   # 42 text
print(add(1, 2), add("con", "cat"))     # 3 concat
```

Generic functions can also be written explicitly. Explicit type parameters support only moving and borrowing their values:

```sayon
function wrap<T>(own value: T) -> Array<T>
    return [value]

numbers = wrap<integer>(42)
```

When a value's type cannot be determined, for example an empty `[]` that is never used, the compiler asks for an annotation.

### Unions and narrowing

A union holds one value of one of its member types. Unions are flattened and normalized, so `(integer or nothing) or nothing` is `integer or nothing`. To use a union value as one of its members, test it first; inside the test the value has the narrower type:

```sayon
function find(id: integer) -> string or nothing
    if id == 1
        return "ada"
    return nothing

name = find(1)
if name != nothing
    print(length(name))

value: integer or string = 5
if typeof(value) == "integer"
    print(value + 10)
else
    print(value + "text")
```

The recognized tests are `value == nothing`, `value != nothing`, `typeof(value) == "type name"`, `typeof(value) != "type name"`, and `value is error` (see [Capturing errors](#capturing-errors)), combined with `not`, `and`, and `or`. A test followed by a block that always leaves (`return`, `break`, `continue`, or an error) narrows the rest of the enclosing block. Assigning to the variable ends its narrowing.

A union compared with one of its members is equal when it holds that member with an equal value.

### Structures

A structure type has named fields. Build a value by naming every field, either with the type's name or, where the type is known, with a field list:

```sayon
type Point = [
    x: float,
    y: float
]

origin: Point = [x = 0.0, y = 0.0]
corner = Point(x = 3.0, y = 4.0)
corner.x = 5.0
print(corner.x + origin.y)        # 5.0
```

- Every field must be given exactly once; missing, repeated, and unknown fields are errors. Fields are evaluated in the order written.
- A field list without a known type builds the one structure of the module with exactly those fields.
- Fields are read with `value.field` and changed with `value.field = ...` or `value.field += ...`, through a variable, an element, or an `update` parameter.
- A function can use a field of a parameter with no annotation; the parameter's type is the one structure of the module with that field.
- Structures are generic when they declare type parameters: `type Pair<T> = [first: T, second: T]`. The arguments are inferred, as in `Pair(first = 1, second = 2)`, or written: `Pair<string>(first = "a", second = "b")`.
- A structure cannot contain itself directly; store nested values in a collection, such as `children: Array<Node>`.
- Types are declared at the top level of a file and are exported unless marked `private`. Another module names them with its import alias: `geometry.Point`.

Two structures are equal when their fields are equal.

### Enums

An enum value is one of several variants. A variant can carry fields:

```sayon
enum Shape
    circle(radius: float)
    rectangle(width: float, height: float)
    point

shapes = [circle(1.0), Shape.rectangle(2.0, 3.0), point]
```

- A variant is built by passing its fields in order, or named alone when it has none.
- A variant can be written without its enum's name when the expected type is that enum, or when only one enum of the module has a variant of that name. `Shape.circle(...)` always works.
- A variant of the expected enum takes precedence over a built-in function of the same name, so `enum Result<T>` may have a variant `error(message: string)` and a function returning `Result<integer>` can `return error("...")`.
- Enums can be generic and may contain themselves through a collection.

Two enum values are equal when they are the same variant with equal fields.

## Operators

From highest to lowest precedence:

| Operators | Notes |
|---|---|
| calls `f(x)`, indexing `a[i]`, members `x.field` | |
| `-x`, `not x` | |
| `*`, `/`, `%` | |
| `+`, `-` | `+` also joins strings |
| `<`, `>`, `<=`, `>=` | numbers and strings |
| `==`, `!=` | |
| `and` | short-circuits |
| `or` | short-circuits |

Comparisons cannot be chained: write `a < b and b < c`. Both operands of an arithmetic or comparison operator must have the same type; Sayon never converts numbers implicitly.

Integer arithmetic is checked: a result outside the type's range raises an `OverflowError`, and division by zero raises a `DivisionByZeroError`. Integer division truncates toward zero, and `%` takes the sign of the dividend. Float arithmetic follows IEEE rules, so `1.0 / 0.0` is `inf`.

### Conversions

Each numeric type name converts a number to that type, with checks:

```sayon
integer(3.99)      # 3, truncated toward zero
float(7)           # 7.0
integer8(300)      # ConversionError: 300 does not fit in integer8
```

Converting a float to an integer rejects infinity and NaN. Converting a float to `float32` rejects finite values too large for it. `tonumber(text)` parses text to `integer or float`, and `tostring(value)` produces display text.

## Control flow

```sayon
if n < 0
    print("negative")
elseif n == 0
    print("zero")
else
    print("positive")

while count < 3
    count += 1

for i in range(5)             # 0, 1, 2, 3, 4
    print(i)
for i in range(10, 0, -3)     # 10, 7, 4, 1
    print(i)
for name in ["ada", "grace"]
    print(name)
for key, value in ["a" = 1]
    print("{key}: {value}")
for character in "abc"
    print(character)
```

`range(end)`, `range(start, end)`, and `range(start, end, step)` count from `start` (default 0) up to, but not including, `end`. A zero step is an error. A loop over a dictionary visits keys in insertion order, either keys alone (`for key in dict`) or keys with values (`for key, value in dict`). `break` and `continue` apply to the innermost loop.

### Matching

`match` runs the first `case` whose pattern matches the value, and whose guard, if it has one, is true:

```sayon
function area(shape: Shape) -> float
    match shape
        case circle(radius)
            return 3.0 * radius * radius
        case rectangle(width, height) if width == height
            return width * width
        case rectangle(width, height)
            return width * height
        case point
            return 0.0
```

| Pattern | Matches |
|---|---|
| `_` | anything |
| a name | anything, binding it to the name (unless it names a variant without fields) |
| a literal: `0`, `-1`, `2.5`, `"text"`, `true`, `nothing` | an equal value |
| `variant(pattern, ...)` | that variant, with each field matching its pattern |
| `Structure(pattern, ...)` | a structure whose fields, in declaration order, match the patterns |
| `Name(field = pattern, ...)` | a variant or structure whose named fields match; fields left out match anything |
| `pattern or pattern` | a value either alternative matches |

Patterns nest: `case some(circle(r))`. On a union, a literal or variant pattern selects the member it belongs to, and a name after cases that handled some members holds only the remaining ones:

```sayon
function describe(value: integer or string or nothing) -> string
    match value
        case nothing
            return "nothing"
        case 0
            return "zero"
        case other
            return "something: {other}"     # other is integer or string
```

Alternatives share one body. Every alternative must bind the same names with the same types, and when a guard fails the next alternative is tried:

```sayon
type Point = [x: float, y: float]

function where(p: Point) -> string
    match p
        case Point(x = 0.0, y = 0.0)
            return "origin"
        case Point(x = 0.0, y = y) or Point(x = y, y = 0.0)
            return "on an axis at {y}"
        case _
            return "elsewhere"

function size(shape: Shape) -> float
    match shape
        case circle(s) or square(s)
            return s
        case _
            return 0.0
```

- A `match` must handle every possible value; the error lists what is missing. End with `case _` to handle everything else. Cases with a guard do not count toward this, since the guard can fail.
- A case that can never run, because the cases above it match everything it would, is reported as a warning, as is an alternative that the cases and alternatives before it already cover.
- Matching a variable borrows its value, and names bound by a case borrow its parts: copy them with `clone`, and do not change the variable while such a case runs. Matching `move(variable)` or a computed value gives the match ownership, so the names bound by cases without a guard own their parts.

## Operators

## Functions

```sayon
function greet(name, greeting = "Hello")
    print("{greeting}, {name}!")

function append_score(update scores: Array<integer>, score: integer)
    push(scores, score)

function keep(own items: Array<string>) -> Array<string>
    return items
```

- Parameters without defaults come first. Defaults are evaluated on each call that omits them, and may use earlier parameters.
- A function without a result annotation returns `nothing` when it reaches its end. A function that returns a value must return one on every path.
- Functions may call each other in any order, including recursively.

### Parameter modes

How a function uses a parameter decides what the call does with the argument:

| Mode | Written | The function may | The caller's value |
|---|---|---|---|
| read | (default) | read it | stays with the caller |
| update | `update name` | change it in place | stays with the caller, changed |
| own | `own name` | keep, return, or store it | is transferred to the function |

Values of Copy types (numbers, booleans, `nothing`, ranges, structures and enums whose fields are all Copy, and unions of these) are passed to read and own parameters as copies; a function may change its copy without affecting the caller. An `update` parameter always receives the caller's variable, so `function translate(update p: Point)` moves the caller's point.

An annotated parameter defaults to read; changing or keeping it requires `update` or `own`. An unannotated parameter's mode is inferred from the function's body: the least mode it needs. A parameter whose type holds a function value has its mode inferred even when annotated, because calling a function value borrows it for changes (see below).

### Function values and closures

A named function is a value, and `function(...)` in an expression makes a closure: a function that captures the variables it uses from where it is written.

```sayon
function apply(func, x, y)
    return func(x, y)

function add(a, b)
    return a + b

print(apply(add, 10, 20))                          # 30
print(apply(function(a, b) return a * b, 6, 7))    # 42

function make_counter()
    count = 0
    return function()
        count = count + 1
        return count

counter = make_counter()
print(counter())  # 1
print(counter())  # 2
```

- A function literal's body is an indented block, or a single `return` or expression on the header's line: `function(x) return x * 2`.
- A function value is called with every argument; defaults apply only to direct calls of a named function. A literal cannot have defaults or type parameters; its types are inferred where it is used, or written as annotations.
- A literal's parameters are read borrows unless they are marked `own` or `update`, or the function type expected where it is written says otherwise. A named function used as a value must declare `own` or `update` on parameters it takes over or changes (`E_PARAMETER_MODE`).
- Function types are written `function(integer, own string) -> boolean`; `-> R` may be left out when the function returns nothing. Inside a union, parenthesize them: `(function(string) -> nothing) or nothing`.
- Function values can be stored in variables, collections, structure fields, and unions, passed, returned, and copied with `clone`. They cannot be compared.

What a closure captures:

- A Copy value the closure only reads is copied when the closure is made.
- An owned value moves into the closure. The enclosing code can no longer use it, as after any other transfer; give the closure a copy with `clone` to keep one.
- A variable the closure changes belongs to the closure from then on, even when its value is Copy, so `make_counter`'s calls keep counting. The enclosing code can no longer use it.
- A closure cannot move a captured value out of itself (`E_MOVE_CAPTURED`), because it can be called again: return `clone(value)` instead.
- A closure made in a loop captures that iteration's value.
- Calling a function value borrows it for changes, since the call may change what it captured. A read-only parameter holding a function value cannot be called; an unannotated one is inferred to be `update`. A `constant` can hold named functions and closures that only read what they capture, and those can be called.

## Ownership

Each string, array, dictionary, and error, and each structure or enum value that holds one, has exactly one owner, and is destroyed when its owner's scope ends. Sayon checks ownership while compiling; there is no garbage collector, and nothing is copied unless you ask.

- Assigning or passing an owned value to an `own` parameter transfers it. The previous owner can no longer use it, until it is assigned a new value.
- `clone(value)` makes an independent copy. `move(value)` makes a transfer explicit.
- Reading a value through a read parameter, in an expression, or in a `for` loop borrows it temporarily.
- A collection cannot change while a `for` loop over it is running.
- A loop variable over a collection of owned values borrows each element: copy it with `clone` to keep it.
- Elements of a collection cannot be moved out; use `clone(...)` or `pop(...)`.
- An owned field cannot be moved out of a structure held by a variable, which would leave it incomplete: clone the field, or move the whole structure. A field of a computed value, such as `make_person().name`, can be taken.
- Module constants cannot be moved or changed.
- A closure takes the owned values it captures, and the variables it changes (see [Function values and closures](#function-values-and-closures)).

```sayon
numbers = [1, 2, 3]
other = numbers
print(numbers)     # error[E_MOVED_VALUE]: `numbers` was transferred to `other`
```

## Errors

`error(message)` raises an error; `raise(problem)` raises an existing `Error` value. An error travels up through calls until a `try` catches it. Every local value is destroyed on the way.

```sayon
try
    value = divide(10, 0)
catch problem
    print("Error: {problem}")         # the message
    print(problem.code)               # UserError
finally
    print("Operation finished")
```

`finally` runs once whichever way the `try` ends: normally, by `return`, `break`, or `continue`, or by an error. A `finally` block cannot itself leave with `return`, `break`, or `continue`. An error that nothing catches ends the program with exit status 1 and a message naming the error and where it happened.

### Capturing errors

`try` before an expression captures an error raised while the expression is evaluated, instead of letting it travel up. The result is an `Outcome<T>`, either `success(value)` or `failure(problem)`:

```sayon
io = import("standard/io.sayon")

result = try io.read_file("data.txt")
if result is error
    print("Read failed: {result.message}")
else
    print(result)
```

- `value is error` is true when an `Outcome` holds a failure (or a union holds an `Error`). Like the union tests, it narrows the variable: where it is true the variable is the `Error`, and where it is false it is the successful value.
- An `Outcome` must be tested before its contents are used. It can also be matched: `case success(value)` and `case failure(problem)`.
- A `try` whose result is thrown away, such as the statement `try save(notes)`, is reported as a warning, since an error would go unnoticed.
- `try` applies to the expression that follows it, as `not` does: `try parse(text)` captures errors from the call and from evaluating its arguments, and an arithmetic expression needs parentheses, as in `try (total / count)`.
- Errors that are not recoverable, such as running out of stack, are not captured.

Error codes raised by the language and its built-in functions:

| Code | Raised by |
|---|---|
| `UserError` | `error(message)` |
| `OverflowError` | integer arithmetic outside the type's range |
| `DivisionByZeroError` | integer `/` or `%` by zero |
| `BoundsError` | an array index outside the array |
| `KeyError` | reading a dictionary key that is not present |
| `ConversionError` | a numeric conversion that does not fit |
| `ParseError` | `tonumber` on text that is not a number |
| `RangeError` | `range` with a step of zero |
| `AssertionError` | a failed `assert` |
| `IOError`, `EncodingError` | file and output failures, and files that are not UTF-8 |

## Built-in functions

| Function | Description |
|---|---|
| `print(values...)` | Writes the values separated by spaces, then a newline |
| `length(value)` | Characters in a string, elements in an array, entries in a dictionary |
| `bytelength(text)` | Bytes in a string's UTF-8 encoding |
| `typeof(value)` | The name of the value's type, or of the member a union holds |
| `tostring(value)` | Display text for any value |
| `tonumber(text)` | Parses `integer or float`; numbers are returned unchanged |
| `assert(condition, message)` | Raises an `AssertionError` when the condition is false; the message is optional |
| `range(start, end, step)` | A range of integers; see above |
| `push(update array, own value)` | Adds a value to the end of an array |
| `pop(update array)` | Removes and returns the last element, or `nothing` when empty |
| `haskey(dict, key)` | Whether a dictionary has a key |
| `clone(value)` | An independent copy |
| `move(name)` | Transfers a variable's value explicitly |
| `readfile(path)` | A file's whole content as text |
| `writefile(path, text)` | Replaces a file's content |
| `error(message)` | Raises a `UserError` |
| `raise(problem)` | Raises an existing `Error` |

## Standard library

Standard modules are imported like any module, with a `standard/` path. Each module's source in `stdlib/` documents every function.

| Module | Functions |
|---|---|
| `standard/io.sayon` | `read_file`, `write_file`, `append_file`, `read_lines`, `read_line` (standard input; `nothing` at the end), `write` (no newline), `write_error` |
| `standard/os.sayon` | `arguments` (the words after `--` with `sayon run`), `environment`, `platform`, `current_directory` |
| `standard/filesystem.sayon` | `exists`, `is_file`, `is_directory`, `list` (sorted names), `size`, `create_directory`, `remove`, `rename`, `copy`, `join`, `name`, `parent`, `extension` |
| `standard/strings.sayon` | `split`, `lines`, `join`, `trim`, `upper`, `lower`, `replace`, `starts_with`, `ends_with`, `contains`, `find` (`integer or nothing`), `repeat`, `slice`, `characters` |
| `standard/math.sayon` | `PI`, `E`, `TAU`, `sqrt`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `exp`, `log`, `log10`, `log2`, `pow`, `floor`, `ceil`, `round`, `trunc`, `abs`, `min`, `max`, `clamp`, `random` |

```sayon
io = import("standard/io.sayon")
strings = import("standard/strings.sayon")

while true
    line = io.read_line()
    if line == nothing
        break
    print(strings.upper(line))
```

String positions count Unicode characters. File and directory failures raise `IOError`; text that is not UTF-8 raises `EncodingError`. `math.random()` is not suitable for security.

### Display

`print` and `tostring` display values as follows. Floats always show a decimal point or an exponent (`3.0`, `0.1`, `1e20`, `inf`, `nan`), and integers never do. Strings appear as text at the top level and quoted inside collections, so collections look like the literals that build them: `[1, "text", true]` and `["name" = "Ada"]`. Structures and enum values look like the calls that build them: `Point(x = 1.0, y = 2.0)`, `circle(5.0)`, `point`. `typeof` names a structure or enum by its type name, such as `Point`.

## Modules

Each file is a module. A program imports another file by binding it to a name at the top of a file:

```sayon
geometry = import("modules/geometry.sayon")

print(geometry.area(3, 4))
print(geometry.UNIT)
```

- Import paths are relative to the program's root, the directory of the file you run, whichever file the import appears in. They use forward slashes, name a `.sayon` file, and cannot leave the root (`..` and absolute paths are rejected).
- Spelling must match the file on disk exactly, on every system, so a program behaves the same on Windows and Linux.
- `standard/...` paths name modules of the standard library.
- A module's functions and constants are exported unless marked `private`. Imported modules can only define things: statements that would run on import are rejected.
- Each module is loaded once, however many modules import it. Imports cannot form a cycle.

## Projects

A single file runs on its own, with imports relative to its directory. A project groups files under a root directory that holds a `sayon.toml` manifest:

```toml
[package]
name = "hello"                 # lowercase letters, digits, and underscores
version = "0.1.0"              # major.minor.patch
entry = "src/main.sayon"       # optional; this is the default
authors = ["Ada"]              # optional
description = "Says hello"     # optional
```

`sayon init hello` creates a project with a manifest, a program, a module with a test, and ignore rules. Inside a project (in its root or any directory below it), `sayon run`, `check`, `build`, and `test` need no file name: they use the manifest's entry file. Imports in a project's files resolve from the project root, also when a file is named on the command line. `sayon build` writes `build/debug/<name>` (`build/release/<name>` with `--release`), and `sayon clean` removes the `build` directory. Without file names, `sayon test` runs the tests in `src` and `tests`.

Unknown or misspelled manifest keys are errors.

### Dependencies

A project can use other Sayon packages: folders with their own `sayon.toml`. Each dependency has an alias, and comes from a local folder or from one exact commit of a Git repository:

```toml
[dependencies]
utilities = { path = "../utilities" }
codec = { git = "https://example.com/team/codec.git", revision = "<full commit id>" }

[dev_dependencies]
checks = { path = "../checks" }    # only for this project's tests
```

A module imports a dependency's files through its alias, with paths inside the dependency's folder:

```sayon
utilities = import("dependencies/utilities/src/library.sayon")
```

- `sayon install` reads the manifests of the project and of every dependency, fetches Git sources into a cache, and writes `sayon.lock`, which records each package's source, version, and dependencies, and a digest of each Git source's files. Commit `sayon.lock` with the project.
- `run`, `check`, `build`, `test`, and `lint` read `sayon.lock`. They never fetch anything or change the lock: when the manifests and the lock disagree, or a cached Git source no longer matches its digest, they stop and ask for `sayon install`.
- `sayon install --locked` fails instead of changing the lock, for automated builds; `sayon install --offline` uses only sources already in the cache.
- A dependency's own imports resolve inside its folder, and its `dependencies/...` imports through its own manifest. Packages cannot depend on each other in a cycle. Warnings about a dependency's code are not shown, and `sayon lint` does not lint it.
- A local folder is used as it is, so changes to it take effect at once. A Git dependency names a full commit (`git rev-parse HEAD` prints one); branches and tags would let the code change under the lock.
- Fetched sources are kept in `SAYON_CACHE`, or the user's cache folder (`%LOCALAPPDATA%\Sayon\cache` on Windows). Git must be installed to fetch them.
- Dependencies by version from a package registry arrive with the registry (milestone M5).

`sayon doctor` reports where the standard library, the runtime library, and clang were found. `sayon explain CODE` explains a diagnostic or runtime error code; [errors.md](errors.md) lists them all.

## Tests

A `test` block at the top level of a file describes a check:

```sayon
function add(a, b)
    return a + b

test "adds integers"
    assert(add(2, 3) == 5)
```

`sayon test file.sayon` runs the file's test blocks; with no files, it runs every `.sayon` file under `tests/`. Each test runs with freshly initialized constants. A test passes when it finishes and fails when an error escapes it; a failing test's printed output is shown with the error. The exit status is 1 when a test fails and 2 when a file does not compile.

`sayon run` and `sayon check` check test blocks but do not run them, and test blocks in imported modules are ignored. Outside a test block, `test` is an ordinary name.

## Formatting

`sayon fmt` rewrites files in the canonical style: four-space indentation, single spaces around operators and after commas, no space inside brackets, one blank line at most between statements, LF line endings, and a final newline. Comments, blank-line separation, and the spelling of literals are kept. A statement written across several lines, or with a comment inside it, keeps its own layout and is only re-indented. Files with syntax errors are left unchanged, and the formatter refuses to write a result whose structure differs from the original.

`sayon fmt --check` lists the files that are not formatted and exits with status 1 if there are any. With no paths, both forms cover every `.sayon` file under the current directory.

## Linting

`sayon lint` checks a program like `sayon check` and also warns about code that is valid but probably not meant: unused variables, parameters, imports, and private items; names that do not follow the conventions; variables that hide others; and an assignment that makes a new variable whose name is one letter away from an existing one. The standard library is not linted. [errors.md](errors.md#lint-warnings) describes each warning.

```sayon
total = 0
for price in prices
    totl = total + price    # warning[W_SUSPICIOUS_DECLARATION]
```

A comment on the warning's line, or the line above, suppresses it by name: `# allow(unused_variable)`. Names that start with `_` are never reported as unused. `sayon lint --deny warnings` fails with status 2 when there are warnings, for use in automated checks.

## Programs

A file with top-level statements is a script: they run in order. A file with only definitions runs its `main` function, which takes no parameters and returns `nothing` or an `integer`. The integer becomes the exit status. Portable statuses are 0 to 255; other values exit with 255 after a warning.

```sayon
function main() -> integer
    print("done")
    return 0
```
