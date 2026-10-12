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

Reserved words: `and`, `async`, `await`, `break`, `case`, `catch`, `constant`, `continue`, `else`, `enum`, `false`, `finally`, `for`, `foreign`, `function`, `if`, `import`, `in`, `is`, `match`, `not`, `nothing`, `or`, `own`, `private`, `public`, `return`, `true`, `try`, `type`, `unsafe`, `update`, `variable`, `while`.

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
| Dictionary | `["name" = "Ada", "city" = "London"]` | `Dict<string, T>` |

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

### Arrays and dictionaries

An array literal whose elements have different types gets the union of those types:

```sayon
mixed = [1, "text", true]        # Array<integer or boolean or string>
```

An empty literal `[]` takes its type from an annotation or from later use, such as a `push`. Dictionary keys are strings in this release. Dictionaries keep insertion order. Replacing a key's value keeps the key's position.

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

Compound assignment (`+=`, `-=`, `*=`, `/=`, `%=`) evaluates its target once.

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
| `Dict<string, T>` | An insertion-ordered dictionary |
| `A or B` | A value of one of the member types |
| `Range` | The result of `range(...)` |
| `Error` | A recoverable error value |

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

The recognized tests are `value == nothing`, `value != nothing`, `typeof(value) == "type name"`, and `typeof(value) != "type name"`, combined with `not`, `and`, and `or`. A test followed by a block that always leaves (`return`, `break`, `continue`, or an error) narrows the rest of the enclosing block. Assigning to the variable ends its narrowing.

A union compared with one of its members is equal when it holds that member with an equal value.

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
else if n == 0
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
```

`range(end)`, `range(start, end)`, and `range(start, end, step)` count from `start` (default 0) up to, but not including, `end`. A zero step is an error. A loop over a dictionary visits keys in insertion order, either keys alone (`for key in dict`) or keys with values (`for key, value in dict`). `break` and `continue` apply to the innermost loop.

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

Values of Copy types (numbers, booleans, `nothing`, ranges, and unions of these) are always passed as copies, so modes only matter for strings, arrays, dictionaries, errors, and unions containing them.

An annotated parameter defaults to read; changing or keeping it requires `update` or `own`. An unannotated parameter's mode is inferred from the function's body: the least mode it needs.

## Ownership

Each string, array, dictionary, and error has exactly one owner, and is destroyed when its owner's scope ends. Sayon checks ownership while compiling; there is no garbage collector, and nothing is copied unless you ask.

- Assigning or passing an owned value to an `own` parameter transfers it. The previous owner can no longer use it, until it is assigned a new value.
- `clone(value)` makes an independent copy. `move(value)` makes a transfer explicit.
- Reading a value through a read parameter, in an expression, or in a `for` loop borrows it temporarily.
- A collection cannot change while a `for` loop over it is running.
- A loop variable over a collection of owned values borrows each element: copy it with `clone` to keep it.
- Elements of a collection cannot be moved out; use `clone(...)` or `pop(...)`.
- Module constants cannot be moved or changed.

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
| `byte_length(text)` | Bytes in a string's UTF-8 encoding |
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

### Display

`print` and `tostring` display values as follows. Floats always show a decimal point or an exponent (`3.0`, `0.1`, `1e20`, `inf`, `nan`), and integers never do. Strings appear as text at the top level and quoted inside collections, so collections look like the literals that build them: `[1, "text", true]` and `["name" = "Ada"]`.

## Programs

A file with top-level statements is a script: they run in order. A file with only definitions runs its `main` function, which takes no parameters and returns `nothing` or an `integer`. The integer becomes the exit status. Portable statuses are 0 to 255; other values exit with 255 after a warning.

```sayon
function main() -> integer
    print("done")
    return 0
```
