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

Text has three kinds of units, and each operation names the one it counts:

| Unit | Is | Counted by | Taken apart by |
|---|---|---|---|
| byte | one byte of the UTF-8 encoding | `bytelength` | `strings.bytes` (a copy), `strings.byte_view` (no copy) |
| character | a Unicode scalar value, such as `e` or the combining accent U+0301 | `length` | `text[i]`, `for`, `strings.characters` |
| grapheme | what a reader sees as one character: a letter with its accents, a whole emoji sequence, a flag | `strings.grapheme_count` | `strings.graphemes` |

```sayon
strings = import("standard/strings.sayon")

word = "e\u{301}t\u{e9}"                       # "été", the first é written as e and an accent
print(bytelength(word), length(word), strings.grapheme_count(word))   # 6 4 3
```

Graphemes follow the extended grapheme clusters of Unicode 17.0 (Unicode Standard Annex #29). Casing and the other operations do not depend on the locale.

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
| `View<T>`, `MutableView<T>` | A borrowed run of an array's elements; see [Views](#views) |
| `A or B` | A value of one of the member types |
| `Range` | The result of `range(...)` |
| `Error` | A recoverable error value |
| `pointer`, `c_string` | A raw address, and an owned copy of text, for C code; see [C functions and unsafe code](#c-functions-and-unsafe-code) |
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

Generic functions can also be written explicitly. Values of an explicit type parameter can be moved, borrowed, cloned, and converted to and from JSON; cloning or converting one makes it a requirement of the parameter, so a call with a type that cannot be cloned, such as an open file, is an error where it is written. Other operations, such as `+` or `==`, need an inferred parameter instead.

```sayon
function wrap<T>(own value: T) -> Array<T>
    return [value]

numbers = wrap<integer>(42)
```

Generic functions with written types can call each other, each with type arguments of its own, as `count_leaves<T>(tree: Tree<T>)` and `count_all<T>(trees: Array<Tree<T>>)` can for a tree. A generic function that calls itself through other functions needs the types of its parameters and its result written (`E_GENERIC_RECURSION`), and what one function of such a cycle requires of its type parameter, the functions calling it with theirs require too. Each set of type arguments makes a version of the function, so one that calls itself with a type argument that grows each time, as `nest([value])` inside `nest<T>(own value: T)` would, is an error (`E_SPECIALIZATION_LIMIT`).

Type arguments may be given in a call, as `wrap<integer>(42)`. A union directly inside them, as in `f<integer or string>(x)`, reads as a comparison, so write such a type deeper inside, as in `f<Array<integer or string>>(x)`, or give it through an annotation: `value: integer or string = f(x)`.

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

The C types, such as `c_int` and `c_size`, convert the same way. Converting a float to an integer rejects infinity and NaN. Converting a float to `float32` rejects finite values too large for it. `tonumber(text)` parses text to `integer or float`, and `tostring(value)` produces display text.

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

A loop also goes through an iterator: a value whose type has a `next` [method](#methods) that returns the next value, or `nothing` at the end. The loop calls `next` before each iteration, and each value belongs to the loop variable, which may keep it.

```sayon
type Countdown = [from: integer]

function next(update countdown: Countdown) -> integer or nothing
    if countdown.from == 0
        return nothing
    countdown.from -= 1
    return countdown.from + 1

for value in Countdown(from = 3)     # 3, 2, 1
    print(value)
```

`next` declares its result as `T or nothing`. A loop over an iterator in a variable changes that variable, and the variable cannot be changed or transferred inside the loop; neither can a variable the iterator was made from with `update`, such as the file of `file.lines()`.

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
| `Enum.variant(...)`, `module.Enum.variant(...)`, `module.Structure(...)` | the same, naming where the variant or structure comes from; `Shape.point` alone is a variant without fields |
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

### Methods

`value.name(arguments)` calls a function with `value` as its first argument. The function is found beside the value's type: in the module that declares the type, a function named `type_name` (`point_move` for `Point`), or else one named `name`, whose first parameter is declared with the type.

```sayon
type Point = [x: float, y: float]

function length_squared(point: Point) -> float
    return point.x * point.x + point.y * point.y

function point_move(update point: Point, dx: float, dy: float)
    point.x += dx
    point.y += dy

p = Point(x = 3.0, y = 4.0)
p.move(1.0, 1.0)
print(p.length_squared())      # 41.0
```

- A method is an ordinary function, also callable as `length_squared(p)`. Its first parameter's mode applies to the value: `point.move(...)` changes `point`, and a method taking `own` transfers it.
- The `type_` prefix lets one module give several types methods of the same name, as `standard/collections` does with `set_add` and `queue_add`.
- A structure field holding a function value is called as before; a field comes before a method of the same name.
- The value's type must be known where the method is called; a parameter used this way needs an annotation (`E_UNKNOWN_METHOD`).
- A private method can be called only inside its module.

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

A function declared inside another function is a closure bound to a variable of that name, so it can use the variables declared before it, as a literal would:

```sayon
function summary(values: Array<integer>) -> string
    scale = 10
    function scaled(value: integer) -> integer
        return value * scale
    total = 0
    for value in values
        total += scaled(value)
    return "total {total}"
```

It cannot call itself, since its name is defined only after its body, and it cannot declare type parameters; a function that needs either belongs at the top level of the module.

## Ownership

Each string, array, dictionary, and error, and each structure or enum value that holds one, has exactly one owner, and is destroyed when its owner's scope ends. Sayon checks ownership while compiling; there is no garbage collector, and nothing is copied unless you ask.

- Assigning or passing an owned value to an `own` parameter transfers it. The previous owner can no longer use it, until it is assigned a new value.
- `clone(value)` makes an independent copy. `move(value)` makes a transfer explicit.
- Reading a value through a read parameter, in an expression, or in a `for` loop borrows it temporarily.
- A collection cannot change while a `for` loop over it is running.
- A loop variable over a collection of owned values borrows each element: copy it with `clone` to keep it.
- Elements of a collection cannot be moved out; use `clone(...)` or `pop(...)`.
- An owned field cannot be moved out of a structure held by a variable, which would leave it incomplete: clone the field, or move the whole structure. A field of a computed value, such as `make_person().name`, can be taken, unless the structure has a [finally function](#finally-functions).
- Module constants cannot be moved or changed.
- A closure takes the owned values it captures, and the variables it changes (see [Function values and closures](#function-values-and-closures)).

```sayon
numbers = [1, 2, 3]
other = numbers
print(numbers)     # error[E_MOVED_VALUE]: `numbers` was transferred to `other`
```

### Views

A view borrows a run of an array's elements without copying them. `view(items, start, end)` shows the elements from `start` up to, but not including, `end`, and `view(items)` all of them; `mutable_view(update items, start, end)` can also change them. The types are `View<T>` and `MutableView<T>`, and a view can be made of another view.

```sayon
function total(values: View<integer>) -> integer
    sum = 0
    for value in values
        sum += value
    return sum

numbers = [5, 3, 8, 1, 9]
print(total(view(numbers, 1, 4)))     # 12

function double(update values: MutableView<integer>)
    for index in range(length(values))
        values[index] = values[index] * 2

double(mutable_view(numbers, 0, 2))
print(numbers)                         # [10, 6, 8, 1, 9]
```

- `length`, `v[i]`, `for` loops, `print`, and `tostring` work on views as on arrays; `v[i] = value` works on a mutable view. Bounds outside the array raise `BoundsError`.
- While a view is still used later in its block, the array it borrows cannot be changed, moved, or replaced, and while a mutable view is, the array cannot be used at all except through the view. After the view's last use, the array is free again. A view made of another view borrows that view in the same way.
- `split_view(update rest, count)` splits the first `count` elements off a view: it returns them as a view of their own and leaves the others in `rest`. The two parts never overlap, so both can be used, and changed, at the same time, while both still borrow the array. Asking for more elements than the view has raises `BoundsError` and leaves the view as it was.

```sayon
function add_into(update target: MutableView<integer>, source: MutableView<integer>)
    for index in range(length(target))
        target[index] += source[index]

pairs = [1, 2, 3, 10, 20, 30]
rest = mutable_view(pairs)
front = split_view(rest, 3)      # front shows 1, 2, 3; rest shows 10, 20, 30
add_into(rest, front)
print(pairs)                     # [1, 2, 3, 11, 22, 33]
```

- `strings.byte_view(text, start, end)` is a `View<byte>` of a string's UTF-8 bytes, without copying them; it borrows the string as a view borrows an array, so the variable holding the string cannot be replaced while the view is used.
- A view lives in variables and parameters. It cannot be stored in a collection, structure, enum, or union, captured by a closure, or held by a constant (`E_VIEW_ESCAPE`), and it must borrow a variable, not a temporary array (`E_VIEW_TEMPORARY`).
- A function can return a view of one of its parameters; the caller's argument then stays borrowed for as long as the result is used. Returning a view of the function's own variable is an error (`E_VIEW_RETURN`).
- `View<T>` values are copied like numbers; a `MutableView<T>` is moved, so only one exists at a time. `clone(view)` makes an array of copies of the elements.

### Shared values

Most values have one owner. When several parts of a program need the same value, `share(value)` makes a `Shared<T>`: a handle to one value that every copy of the handle refers to. `shared.value` reads it in place.

```sayon
type Settings = [name: string, sizes: Array<integer>]

settings = share(Settings(name = "main", sizes = [1, 2, 3]))
other = settings                     # a second handle; both stay usable
print(other.value.name, length(settings.value.sizes))    # main 3
```

- Assigning a handle, passing it, storing it, or capturing it in a closure makes another handle to the same value, so the original stays usable; `move(handle)` hands it over instead. The value is destroyed when its last handle is, so the cost is counting the handles.
- A shared value cannot change (`E_SHARED_IMMUTABLE`), and nothing can be taken out of it; `clone(shared.value)` copies it. A structure holding a handle is still moved like any structure.
- What changes goes in a `Cell<T>`: `cell(value)` makes one, `c.get()` copies its value out, `c.set(value)` replaces it, and `c.replace(value)` replaces it and returns what it held. A cell changes even through a read-only path, so `Shared<Cell<T>>` is state that every handle sees change. Nothing inside a cell can be borrowed, which is what makes this safe; work on a cell's array with `replace` and `set`.
- `shared.weak()` makes a `Weak<T>`, a handle that does not keep the value alive; `weak.upgrade()` gives a `Shared<T>`, or `nothing` once the last `Shared` handle is gone.
- Handles that refer to each other in a circle keep each other alive forever. Point back up a tree, or to anything that owns you, with a `Weak` handle:

```sayon
type Node = [label: string, parent: Cell<Weak<Node> or nothing>, children: Cell<Array<Shared<Node>>>]

function attach(parent: Shared<Node>, child: Shared<Node>)
    child.value.parent.set(parent.weak())
    children = parent.value.children.replace([])
    push(children, child)
    parent.value.children.set(children)
```

- Handles cannot be compared, and a constant cannot hold one or a cell (`E_SHARED_CONSTANT`). They are for one thread; tasks on different threads share a `Mutex` instead (see [Tasks on threads of their own](#tasks-on-threads-of-their-own)).

### Finally functions

A structure that owns something outside the program, such as C memory or a handle of the system, releases it in a `finally function`, which runs when a value of the structure is destroyed:

```sayon
type Buffer = [address: pointer, size: integer]

finally function release(update buffer: Buffer)
    unsafe free(buffer.address)

function scratch()
    buffer = Buffer(address = unsafe malloc(64), size = 64)
    # ...
    # Here `buffer` is destroyed, and `release` frees its memory.
```

- It runs wherever a value is destroyed: at the end of its owner's scope, when a variable or an element is given another value, inside collections, unions, enums, closures, and channels, when the last handle of a `Shared` value or a mutex goes, and for module constants at the end of the program. A value that moves is not destroyed; its new owner destroys it once.
- It takes the value as `update`, and runs before the value's fields are destroyed, so it can still use them. It is declared beside its structure, in the same module, and the structure has no type parameters and is not a `foreign type` (`E_FINALLY_FUNCTION`). Each structure has at most one.
- The program never calls it: a call of it, or its use as a value, is `E_FINALLY_FUNCTION`. To destroy a value early, move it into a function that does nothing with it.
- A value of such a structure is never copied: it is not Copy, cannot be cloned (so `mutex.get()` and `cell.get()` do not take one), and its fields cannot be moved out, not even of a computed value (`E_MOVE_OUT_OF_FIELD`). A `match` lends the fields to its names.
- It cannot wait (`FinallyError`), and cannot fail: when an error escapes it, the program stops at once with a fatal error, which names the function, and status 3. Handle the errors it can meet with `try` inside it.

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

## C functions and unsafe code

A `foreign` block declares functions of a C library, which the program can then call:

```sayon
foreign "c"
    function strlen(text: c_string) -> c_size
    function malloc(size: c_size) -> pointer
    function free(address: pointer)
    function sprintf(buffer: pointer, format: c_string, ...) -> c_int

name = c_string("Sayon")
print(unsafe strlen(name))       # 5
```

- `"c"` names the target's C runtime; another name is one of the system's libraries, such as `"kernel32"` or `"m"`, which a compiled program is linked with. A declaration gives each parameter its C type, and `...` after them takes further arguments.
- C functions take integers, floats, `pointer`, `c_string`, and C structures, and give integers, floats, `pointer`, C structures, or nothing (`E_FOREIGN_TYPE`). The integer types include the target's C types `c_char`, `c_short`, `c_ushort`, `c_int`, `c_uint`, `c_long`, `c_ulong`, `c_longlong`, `c_ulonglong`, `c_size`, and `c_ssize`, each the integer type of its size on the target the program is checked for: `c_long` is `integer32` on Windows and WebAssembly and `integer64` elsewhere, and `c_size` and `c_ssize`, like `pointer`, are as wide as an address, 32 bits on WebAssembly. `c_float` and `c_double` are `float32` and `float`.
- Arguments after the parameters of a variadic function go as C passes them: an integer narrower than `c_int` becomes a `c_int`, a `float32` becomes a `float`, and an integer literal is a `c_int`.
- `c_string(text)` copies text into a buffer that ends with a NUL byte, for C to read; it fails with `ValueError` when the text holds a NUL character, which C would take for its end. The C string owns its buffer, and C reads it during a call that takes it. It shows as its text, and cannot be cloned or compared. Text that C gives back is a `pointer`, which `memory.read_text` reads.
- A `pointer` is a raw address. `null_pointer` points nowhere; pointers can be compared, and one shows as `pointer` or `null_pointer`, never as its address.

The compiler cannot check what C code does, so calling a C function is unsafe code: the call needs `unsafe` before it, or an `unsafe` block around it (`E_UNSAFE`). `unsafe` marks the code the program vouches for, and nothing else changes inside it: types, ownership, and every other check still apply.

```sayon
memory = import("standard/memory.sayon")

unsafe
    buffer = malloc(64)
    if buffer == null_pointer
        error("out of memory")
    sprintf(buffer, c_string("%d items"), 3)
    print(memory.read_text(buffer))   # 3 items
    free(buffer)
```

- `unsafe` before an expression covers one operand, like `try`: `unsafe strlen(name) + 1` adds 1 to the result of the call. An `unsafe` block is a block with variables of its own, like the body of an `if`.
- An `unsafe function` is one whose callers vouch for how they call it: its body is unsafe code, and every call of it needs unsafe code. A function that makes its C calls correct for every argument needs no marking, and its callers need no `unsafe`; that is how a library wraps C code safely. An unsafe function is declared at the top level of a file (`E_UNSAFE_POSITION`).
- A function literal written in unsafe code is unsafe code too. A C function or an unsafe function cannot be used as a value (`E_UNSAFE_VALUE`), since a function value can be called anywhere.
- `W_UNUSED_UNSAFE` warns of an `unsafe` that nothing inside needs, or one inside unsafe code already.

`standard/memory.sayon` reads and writes the memory C code shares with the program. `read<T>(address, offset)` and `write(address, value, offset)` read and store a number, a pointer, or a C structure `offset` bytes after an address; `size<T>()` is how many bytes one takes; `read_bytes` and `write_bytes` copy bytes; `read_text` reads the C string at an address; `offset(address, bytes)` is another address; `address(text)` is where a C string's text is. The functions that touch memory are unsafe, and fail with `NullPointerError` at `null_pointer`.

### C structures

`foreign type` declares a structure laid out as C lays it out, which C functions take and give by value and which `standard/memory` reads and writes:

```sayon
memory = import("standard/memory.sayon")

foreign type Point = [x: c_int, y: c_int]
foreign type Rectangle = [left: c_int, top: c_int, right: c_int, bottom: c_int]

foreign "user32"
    function PtInRect(rectangle: pointer, point: Point) -> c_int

unsafe
    area = malloc(c_size(memory.size<Rectangle>()))
    memory.write(area, Rectangle(left = 0, top = 0, right = 10, bottom = 5))
    print(PtInRect(area, Point(x = 3, y = 4)))     # 1
    free(area)
```

- Its fields are numbers, `pointer`, and other `foreign type` structures, in C's order: each at the next multiple of its alignment, and the whole a multiple of the largest (`E_FOREIGN_TYPE` for anything else, and for type parameters). Text is a `pointer` to its bytes, a boolean a `c_int`, and a structure that points to another of its type does so with a `pointer`.
- Otherwise it is an ordinary structure: built, read, changed, compared, and shown as any other, and copied, since every field is.
- A variadic C function takes a structure by its address, as a `pointer`.
- Structures go to C and come back as the target's C convention passes them; compiled programs and the interpreter pass them as Windows x64 and Linux x86-64 do, compiled programs for WASI as WebAssembly's C convention does and for Android on ARM64 as AAPCS64 does, and compiled programs report `ForeignError` on other targets.

### Functions that C calls back

A parameter of a C function can be a function, which C calls during the call:

```sayon
foreign "c"
    function qsort(base: pointer, count: c_size, size: c_size, compare: function(pointer, pointer) -> c_int)

unsafe
    qsort(numbers, 10, 4, function(a: pointer, b: pointer) -> c_int
        return memory.read<integer32>(a) - memory.read<integer32>(b)
    )
```

- The function takes numbers and `pointer` and gives a number, a `pointer`, or nothing (`E_FOREIGN_TYPE`). Any function value of its type goes: a function literal, with what it captured, or a named function.
- C may call it only during the call it was given to, on the same thread, as `qsort`, `bsearch`, and functions that walk a list with a callback do. A C function that keeps the address to call later would call into nothing; the program stops with a fatal error when that is detected.
- A function that C calls cannot wait (`CallbackError`), since its task cannot leave C in the middle of its call.
- Its failure cannot pass through C. The first one is kept, and C gets 0 (or `null_pointer`) from that call and from every later one, without the function running again; once C returns, the call of the C function fails with the error.
- A function that C calls can call C functions too, which can call back in turn.

Compiled programs call C functions on every target. The interpreter calls them on Windows x64 and Linux x86-64 so far, loading each library and finding each function the first time the program calls one; elsewhere it reports `ForeignError`, and a program built with `sayon build` calls them. The interpreter has room for functions for 64 different parameters of C functions.

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
| `remove(update array, position)` | Removes and returns the element at a position, moving the later ones down; `BoundsError` outside the array |
| `remove(update dict, key)` | Removes a key's entry and returns its value, or `nothing` when the key is absent; the other entries keep their order |
| `swap(update array, first, second)` | Exchanges two elements |
| `share(own value)`, `cell(own value)` | A `Shared<T>` or a `Cell<T>`; see [Shared values](#shared-values) |
| `view(items, start, end)`, `mutable_view(update items, start, end)` | A view of elements; see [Views](#views) |
| `split_view(update view, count)` | Splits the first `count` elements off a view into a view of their own |
| `haskey(dict, key)` | Whether a dictionary has a key |
| `clone(value)` | An independent copy |
| `move(name)` | Transfers a variable's value explicitly |
| `readfile(path)` | A file's whole content as text |
| `writefile(path, text)` | Replaces a file's content |
| `error(message)` | Raises a `UserError` |
| `raise(problem)` | Raises an existing `Error` |
| `c_string(text)` | A copy of text for C code; see [C functions and unsafe code](#c-functions-and-unsafe-code) |

## Standard library

Standard modules are imported like any module, with a `standard/` path. Each module's source in `stdlib/` documents every function.

| Module | Functions |
|---|---|
| `standard/io.sayon` | `read_file`, `write_file`, `append_file`, `read_lines`, `read_file_bytes`, `write_file_bytes`, `read_line` (standard input; `nothing` at the end), `write` (no newline), `write_error`; `open` for a `File`, and `input`, `output`, and `errors` for the standard streams as ones, with the methods `read_line`, `lines`, `read`, `read_bytes`, `read_text` (a part of the text, never cutting a character), `write`, `write_line`, `write_bytes`, `flush`, `position`, `seek`, `close`; `copy` |
| `standard/os.sayon` | `arguments` (the words after `--` with `sayon run`), `environment`, `platform`, `current_directory`, `exit`, `run` (a `Completed` with `status`, `output`, and `errors`), `start` (a `Process` with the files `input`, `output`, and `errors`, and the methods `finish_input`, `wait`, `stop`, `id`) |
| `standard/filesystem.sayon` | `exists`, `is_file`, `is_directory`, `is_link`, `list` (sorted names), `walk` (everything below a directory), `size`, `create_directory`, `remove`, `rename`, `copy`, `join`, `name`, `parent`, `extension` |
| `standard/strings.sayon` | `split`, `lines`, `join`, `trim`, `upper`, `lower`, `replace`, `starts_with`, `ends_with`, `contains`, `find` (`integer or nothing`), `repeat`, `slice`, `characters`, `code` (a character's Unicode code), `character` (the character with a code), `graphemes`, `grapheme_count`, `bytes`, `byte_view`, `from_bytes` (strict UTF-8), `from_bytes_lossy`; other encodings: `encode`, `decode`, `decode_lossy` for UTF-16 (`"utf-16"`, `"utf-16le"`, `"utf-16be"`), Latin-1, ASCII, and Windows-1252 |
| `standard/json.sayon` | the `Json` enum, `parse`, `encode`, `pretty`, `number_value`, `text_value`, `flag_value`, `field`, `item`, `from_integer`; typed values: `decode`, `encode_value`, `from_json`, `to_json`; streams: `items`, `lines`, `write_items` (an `ItemWriter` with `add`, `add_value`, `finish`) |
| `standard/threads.sayon` | `spawn`, `join`, `detach`, `pause`, `new_channel`, `new_unbounded_channel`, `after`, `send`, `recoverable_send` (a `SendResult<T>`), `receive`, `receive_result` (a `ReceiveResult<T>`), `close`, `select`; see [Tasks and channels](#tasks-and-channels) |
| `standard/network.sayon` | `connect` for a `Connection` (`read_line`, `lines`, `read`, `read_bytes`, `write`, `write_line`, `write_bytes`, `flush`, `finish_sending`, `set_timeout`, `local_address`, `peer_address`, `close`), `listen` for a `Listener` (`accept`, `address`, `set_timeout`, `close`), `bind` for a `DatagramSocket` (`send`, `receive`, `address`, `set_timeout`, `close`), `resolve`, `address_text`; HTTP: `get`, `post`, `request`, and `Response` (`text`, `header`, `succeeded`); see [Networking](#networking) |
| `standard/graphics.sayon` | `canvas`, `image`, `decode_image`, and `load_image` for a `Canvas` (`smooth`, `clear`, `fill_rectangle`, `stroke_rectangle`, `fill_circle`, `fill_polygon`, `line`, `draw`, `text`, `write`, `pixel`, `bytes`, `png`, `save_png`), `text_width`, `text_height`, `decode_font`, `load_font`, and `system_font` for a `Font` (`name`, `ascent`, `descent`, `line_height`, `width`, `height`), colors (`Color`, `rgb`, `rgba`, `BLACK`, `WHITE`, `RED`, `GREEN`, `BLUE`, `TRANSPARENT`), `Point`; `open` for a `Window` (`events`, `present`, `width`, `height`, `scale`, `is_open`, `close`) and its `Event`s; see [Graphics](#graphics) |
| `standard/time.sayon` | `Duration` (`nanoseconds`, `microseconds`, `milliseconds`, `seconds`, `minutes`, `hours`, `add`, `subtract`, `scale`, `in_seconds`, `in_milliseconds`, `describe`), the monotonic clock (`start`, `elapsed`, `between`, `sleep`), `deadline` for a `Deadline` (`remaining`, `passed`, `wait`), `ticker` for a `Ticker` (`wait`), and UTC dates (`DateTime`, `now`, `unix_seconds`, `from_unix`, `to_unix`, `shift`, `difference`, `weekday`, `is_leap_year`, `days_in_month`, `format` and `parse` for ISO 8601), time zones (`zone` for a `Zone` with `at`, `moment`, `offset`; `ZonedTime` with `format`, `moment`; `local_zone`, `zones_version`) |
| `standard/collections.sayon` | `sort` and `sort_by` (stable), `reverse`; `Set<T>` with `new_set`, `set_add`, `set_contains`, `set_remove`, `set_size`, `set_values` (as methods: `set.add(value)`); `Queue<T>` with `new_queue`, `queue_add`, `queue_take`, `queue_size` |
| `standard/memory.sayon` | `read`, `write`, `size`, `read_bytes`, `write_bytes`, `read_text`, `offset`, `address`; see [C functions and unsafe code](#c-functions-and-unsafe-code) |
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

`os.run(program, arguments, directory, input, environment, inherit_environment)` runs another program and waits for it, returning its exit status and what it wrote to standard output and error. The arguments are passed exactly as given, with no shell in between, so no character in them has a special meaning. The program reads `input` (nothing unless given), and has this program's environment variables with those of `environment` added, or with `inherit_environment = false` only those. Running something through a shell is explicit, as with `os.run("sh", ["-c", "ls | wc -l"])`. A program that cannot be started raises `IOError`.

`os.start` starts a program without waiting for it, and gives a `Process` whose `input`, `output`, and `errors` are files: write to its input, `finish_input()` when it is done, read its output as it comes, and `wait()` for its exit status, or `stop()` it. Waiting for a program, or for what it writes, lets other tasks run. `io.output()` and `io.errors()` give standard output and error as files, for code that takes a file; what they write comes in order with `print`.

```sayon
os = import("standard/os.sayon")

sorting = os.start("sort", [])
for word in ["pear", "apple"]
    sorting.input.write_line(word)
sorting.finish_input()
print(sorting.output.read())     # apple, then pear
sorting.wait()
```

`os.exit(status)` ends the program with a status from 0 to 255 (another status raises `ValueError`). It ends it in order: `finally` blocks run and values are cleaned up on the way out, so open files are written, but `catch` and `try` do not stop it. In a test, it fails the test.

`filesystem.walk(path)` lists every file and directory below `path`, as paths relative to it joined with `/`. Each directory comes just before what it holds, and each directory's entries are sorted by name. A symbolic link is listed but not followed, so a walk always ends; `filesystem.is_link(path)` tells whether a path is one.

### Files

`io.open(path, mode, encoding)` opens a file to read a part at a time (mode `"r"`, the default), to write from the start (`"w"`, which creates the file or empties it), or to write at its end (`"a"`). Reading and writing go through a buffer. Its text is UTF-8 unless an encoding of `strings.encode` is given, as in `io.open("notes.txt", "r", "utf-16")`; such a file reads and writes text as usual, has no positions to `seek` to, and fails with `EncodingError` for text not in its encoding.

```sayon
io = import("standard/io.sayon")

log = io.open("log.txt", "w")
log.write_line("started")
log.close()

file = io.open("large.txt")
for line in file.lines()
    print(line)
file.close()
```

- `read_line` and `lines` give lines without their endings, accepting LF and CRLF; the last line need not end with one. A line longer than the file's `line_limit` (`io.LINE_LIMIT`, 1 MiB, unless changed with `file.line_limit = ...`) raises `IOError`, and the rest of that line stays unread, so a hostile file cannot fill memory.
- `read` gives the rest of the file as text; `read_bytes(count)` and `write_bytes(bytes)` work with `Array<byte>`, keeping any bytes as they are. `position()` and `seek(position)` count bytes from the start.
- `close()` writes what is still buffered and closes the file, raising `IOError` if writing fails; the file cannot be used afterwards. A file that is not closed is closed when it is no longer used, but without a report, and before the program ends every open file's buffer is written. `flush()` passes buffered output to the system; it does not wait for the disk.
- While a loop over `file.lines()` runs, the file is borrowed: closing it, or reading or writing it another way, is `E_BORROW_CONFLICT`.
- A file has one owner, so `clone(file)` is an error, as is cloning anything holding a file; a generic function that clones values of its type parameter cannot be given one. Lines kept from `file.lines()` after the file is closed raise `IOError`.
- `io.copy(source, target)` copies the rest of one file to another 64 KiB at a time and returns the number of bytes.
- `io.input()` is standard input as a file, read with the same methods and sharing what is read with `io.read_line()`; it cannot `seek`.

### JSON

`json.parse(text)` reads JSON into a `Json` value, an enum with the variants `null`, `flag(value: boolean)`, `number(digits: string)`, `text(value: string)`, `list(items: Array<Json>)`, and `object(fields: Dict<string, Json>)`. `json.encode(value)` writes compact JSON and `json.pretty(value)` indents it by two spaces per level.

```sayon
json = import("standard/json.sayon")

value = json.parse("{{\"name\": \"Ada\", \"born\": 1815}}")
match value
    case json.Json.object(fields)
        print(length(fields))                            # 2
    case _
        print("not an object")
born = json.field(value, "born")                         # Json or nothing
if born != nothing
    print(json.number_value(born))                       # 1815
```

- A number keeps the exact text it was written with, so integers of any size and decimals come back unchanged from `encode`; `number_value` reads it as `integer or float`.
- An object keeps its fields in the order they were written, and `encode` writes them in that order.
- `parse` raises a `ParseError` naming the line and column for text that is not JSON, including anything but whitespace after the value, a field name repeated in one object, an unpaired surrogate escape, a raw control character in a string, and nesting deeper than 512 levels.
- Build values with the variants, written through the module: `json.Json.text("hi")`, `json.Json.list([...])`, or `json.from_integer(3)`.
- Braces in a Sayon string start interpolation, so JSON written as a literal doubles them: `"{{\"a\": 1}}"`.

`json.decode(text)` reads JSON straight into a value of the type it is used as, and `json.encode_value(value)` writes one; `json.from_json` and `json.to_json` do the same between typed values and `Json`.

```sayon
type Point = [x: float, y: float]
type Shape = [name: string, points: Array<Point>, note: string or nothing]

shape: Shape = json.decode("{{\"name\": \"line\", \"points\": [{{\"x\": 0, \"y\": 1.5}}]}}")
print(shape.points[0].y)             # 1.5
print(json.encode_value(shape))      # {"name":"line","points":[{"x":0.0,"y":1.5}],"note":null}
points = json.decode<Array<Point>>("[]")
```

| Type | JSON |
|---|---|
| numbers, `boolean`, `string`, `nothing` | a number, `true` or `false`, text, `null` |
| `Array<T>` | a list |
| `Dict<string, T>`, structures | an object; a structure's field missing from the object is `nothing` when its type allows it, and other members are ignored |
| enums | a variant without fields is its name, `"dot"`; one with fields is an object with one member named after the variant, `{"circle": {"radius": 2.0}}` |
| unions | whichever member reads that kind of value; a number written as an integer reads as the integer member and another as the float member |
| `Json` | any value, unchanged |

- An integer must be written without a fraction or exponent and fit its type; all 64 bits of `unsigned` and `integer` are kept exactly.
- JSON that does not fit raises `DecodeError`, starting with the path to the place: `$.points[0].y: expected float, found text`.
- Types JSON cannot hold, such as function values or dictionaries with integer keys, are compile errors, and so are unions where two members read the same kind of JSON value, such as `Point or Line` (both objects). A generic function that decodes its type parameter passes this requirement on to its callers.

Files too large to read at once are read and written a value at a time. `json.items(file)` reads the items of the array a file holds as a `for` loop goes through them, and `json.lines(file)` the values of a file of JSON Lines, one value on each line; a `ParseError` names the item or the line. `json.write_items(file)` gives an `ItemWriter`, whose `add(value)` and `add_value(value)` write an item each and whose `finish()` ends the array.

```sayon
json = import("standard/json.sayon")
io = import("standard/io.sayon")

file = io.open("orders.json")
total = 0
for order in json.items(file)
    total += 1
file.close()
```
- Writing a float that is infinite or not a number raises `ValueError`, since JSON has no number for it.

### Time

The monotonic clock measures intervals and never goes backwards; the wall clock gives the date and time and can jump when the system clock is set. Dates and times are in UTC, on the Gregorian calendar extended backwards, and time zones give them as a zone's clocks show them. Durations count whole nanoseconds, and their arithmetic is checked like any integer arithmetic.

```sayon
time = import("standard/time.sayon")

start = time.start()
time.sleep(time.milliseconds(50))
print(time.describe(time.elapsed(start)))     # such as 50.12ms
print(time.format(time.now()))                # such as 2026-10-18T09:30:00Z
moment = time.parse("2026-10-18T09:30:00+02:00")
print(time.format(moment), time.weekday(moment))   # 2026-10-18T07:30:00Z 7
```

`time.zone(name)` is a `Zone` of the IANA time zone database, such as `"Europe/Paris"`, and fails with `TimeZoneError` for a name the system does not know; `time.local_zone()` is the system's own. `zone.at(moment)` gives a `ZonedTime`: the `local` date and time the zone's clocks show, their `offset` from UTC, and the zone, whose `format()` writes ISO 8601 with the offset and whose `moment()` is the moment in UTC again. `zone.moment(local)` goes the other way, from a local date and time to its moment: a local time the clocks skip when they go forward falls after the gap, and one they show twice is the earlier. The rules come from the system: on Windows from the ICU library it ships, on Android from the database the phone keeps up to date, with the phone's own zone as the local one, and elsewhere from `/usr/share/zoneinfo`, or the directory `TZDIR` names; `time.zones_version()` tells their version.

```sayon
paris = time.zone("Europe/Paris")
print(paris.at(time.parse("2026-07-15T12:00:00Z")).format())   # 2026-07-15T14:00:00+02:00
print(time.format(paris.moment(time.parse("2026-12-24T18:00:00Z"))))   # 2026-12-24T17:00:00Z
```

`time.deadline(duration)` is a `Deadline` that long from now, whose `remaining()` is the time left (none once it has passed), `passed()` tells whether it has, and `wait()` waits for it. `time.ticker(interval)` is a `Ticker`, whose `wait()` waits for its next beat and gives how many beats have come: the beats keep to the interval however long the work between them takes, and when the work took longer than an interval, the beats missed count too.

### Tasks and channels

`standard/threads.sayon` runs tasks together. A task is a fiber: the tasks take turns on one thread, and one runs until it waits, for a message, for room in a channel, for another task, for time to pass, or in `threads.pause()`. A spawned task starts when the task that spawned it first waits.

```sayon
threads = import("standard/threads.sayon")

handle = threads.spawn(function() -> integer
    return 42
)
print(threads.join(handle))           # 42

channel: Channel<string> = threads.new_channel()
sender = threads.spawn(function()
    threads.send(channel, "message")
)
print(threads.receive(channel))       # message
threads.join(sender)
```

- `spawn(function)` gives a `Task<T>`, and `join(task)` waits for its result, or fails with the task's error. A task owns what its function captured, as any closure does; `Shared` values and channels are handles, so tasks that capture them share them.
- `new_channel(capacity)` makes a `Channel<T>` holding up to `capacity` messages (64 unless given); `send` waits while it is full, and a capacity of 0 makes each `send` wait until its message is received. `receive` waits while it is empty. Messages keep the order they were sent in, and sending transfers a message's ownership. `new_unbounded_channel()` makes a channel whose `send` never waits, at the cost of memory for every message nobody has received yet.
- `close(channel)` stops sends, which then fail with `ChannelClosedError` and destroy their message; receivers get the messages already sent and then `ChannelClosedError`. `receive_result` gives `message(value)` or `closed` instead, and `recoverable_send` gives `sent`, or `unsent(message, problem)` with the message back when the channel is closed.
- `select(channels, handler)` waits until one of the channels has a message or is closed and empty, receives from it, and calls `handler(message, position)`. When several are ready, the choice goes round, starting after the last one the task chose.
- `time.sleep` lets other tasks run while it waits. Waiting when no task could ever go on raises `DeadlockError`.
- `after(delay, message)` is a timer: a channel that receives `message` once `delay` has passed. Selecting it with other channels waits for their messages or for too long, as with `select([events, threads.after(time.seconds(5), timeout)], handler)` for an enum `Event` with a `timeout` variant.
- When the main task finishes, the tasks still running are cancelled: each stops where it waits, running its `finally` blocks and cleanup, which `catch` and `try` do not stop. A task whose handle is dropped without a `join` is cancelled the same way (`W_UNUSED_TASK` warns of a handle dropped at once), and one that has not started never runs; `detach(task)` lets a task run on until the main task ends. A task that fails without being joined is reported and makes the program fail. `os.exit` in any task ends the whole program in order.

Tasks started with `spawn` run on one thread, so values they share through `Shared` and `Cell` never change under them in the middle of an operation. Compiled programs can run tasks on Windows and on Linux x86-64 so far.

### Tasks on threads of their own

`threads.spawn_parallel(function)` starts a task on a thread of its own, which runs at the same time as the program's other tasks, on another processor when there is one. Its handle is a `Task<T>` like any other, which `join` and `detach` take. Tasks on different threads share data through a `Mutex<T>`:

```sayon
threads = import("standard/threads.sayon")

total = mutex(0)
first = threads.spawn_parallel(function()
    total.change(function(update value: integer) value += 1)
)
second = threads.spawn_parallel(function()
    total.change(function(update value: integer) value += 2)
)
threads.join(first)
threads.join(second)
print(total.get())                    # 3
```

- What the function captures moves to the new thread, and the task's result moves back, so both must be *sendable*: they share nothing with the thread they left. Numbers, strings, arrays, dictionaries, structures, enums, errors, cells, channels, and mutexes are sendable when what they hold is; `Shared` and `Weak` values, tasks, views, and function values are not, since their handles are counted without the cost of making that safe across threads (`E_UNSUPPORTED_OPERATION`). The function is written at the call or named, so that its captures can be checked (`E_PARALLEL_FUNCTION`).
- Channels work between threads as they do between the tasks of one thread: a task on any thread can send, receive, `select`, and close, and a rendezvous `send` waits for a receive on another thread. Messages move as they always do, so a channel between threads carries sendable messages, such as a channel to answer on.
- While tasks run on other threads, a task that waits for a channel they may hold waits for them instead of raising `DeadlockError`. A channel counts as one they may hold once a task on another thread was started, or a value went into a mutex or through such a channel, while the channel existed; waiting for a channel made after that, or after the tasks on other threads have finished, is detected as before. Tasks on different threads that wait for each other wait forever.
- `mutex(value)` makes a `Mutex<T>`, a handle like a `Shared` value: copies of it refer to the same mutex, whose value lives until the last handle goes. Its value is used only under its lock, which one task holds at a time: `m.get()` copies the value out, `m.set(value)` replaces it, `m.replace(value)` replaces it and gives what it held, and `m.change(function)` calls the function with the value to change in place, holding the lock until it returns. A task that wants the lock while a task on another thread holds it waits for it; the lock is freed also when the function fails. A mutex's value must be sendable, and a constant cannot hold a mutex (`E_SHARED_CONSTANT`).
- A task cannot wait for anything while it holds a lock: inside `change`, sending, receiving, joining, sleeping, `pause`, and taking the same mutex's lock again fail with `LockError`, and the value is put back as it was left. Keep the work under a lock short, and take the locks of several mutexes in the same order everywhere: two tasks that each hold one lock and want the other's wait forever.
- A task on another thread runs the tasks it spawns on its own thread, and they end with it. When the main task finishes, the tasks on other threads are cancelled where they wait, like any other, and the program waits for their threads to finish; a task that fails there without being joined makes the program fail. `os.exit` in any task, on any thread, ends the whole program.
- `print` and the other output functions write whole lines, so the lines of tasks on different threads do not mix, but their order depends on how the threads run.

### Networking

`standard/network.sayon` makes TCP connections, listens for them, sends and receives UDP datagrams, and makes HTTP requests.

```sayon
network = import("standard/network.sayon")
threads = import("standard/threads.sayon")

listener = network.listen("127.0.0.1", 0)       # 0: any free port
port = listener.address().port
server = threads.spawn(function()
    connection = listener.accept()
    connection.write_line("hello, {connection.read_line()}")
    connection.close()
)

client = network.connect("127.0.0.1", port)
client.write_line("world")
print(client.read_line())                       # hello, world
client.close()
threads.join(server)
```

- A `Connection` reads and writes like a file: `read_line`, `lines`, `read` (everything until the other side finishes), `read_bytes` (as many bytes as have arrived, up to a count), `write`, `write_line`, `write_bytes`, `flush`, `close`. `finish_sending()` ends what this side sends while it can still read, and `local_address()` and `peer_address()` give `Address` values with a `host` and a `port`.
- `network.listen(host, port)` gives a `Listener`, whose `accept()` waits for the next connection; `network.bind(host, port)` gives a `DatagramSocket`, which `send`s datagrams to a host and `receive`s them as a `Datagram` of `bytes` and its `sender`. `network.resolve(host)` gives a host's numeric addresses.
- `network.connect_secure(host, port)` connects and starts TLS, and `connection.secure(name)` starts it on a connection made already, for protocols that begin in plain text. The system's TLS checks the server's certificate against the certificates it trusts, and that it is for the name given, which also goes to the server; a failure is a `TlsError` that says what was wrong, such as an expired certificate or one for another name. What the connection reads and writes after that is encrypted, with every operation the same. TLS comes from the system: SChannel on Windows, OpenSSL's `libssl` on Linux, and in an Android app the platform's own, which trusts what the phone and its user trust; where there is none, as in a program run from `adb shell`, it fails with `TlsError`.
- A host with several addresses is connected on the first that answers: attempts start a quarter of a second apart, between IPv6 and IPv4 by turns, so that a kind of address that does not work on a network costs little.
- Waiting for the network lets other tasks run, so one program can be both a server and its client. `connect` takes a `timeout` (30 seconds unless given), and `set_timeout(duration)` gives a connection, listener, or datagram socket one for each of its operations; an operation that waits longer fails with `TimeoutError`. Other failures are `ConnectionError`, `AddressError`, `TlsError`, and `NetworkError`.
- What a connection writes goes out as the other side takes it; a write waits only when a megabyte is still waiting to go, and `flush` and `close` wait until all of it has gone.

`network.get(url)`, `network.post(url, text)`, and `network.request(method, url, headers, body, timeout, redirects, limit)` make HTTP/1.1 requests and give a `Response`: its `status` and `reason`, its `headers` with lowercase names, its `body` as bytes, and the `url` that answered. `response.text()` reads the body as text, `response.header(name)` finds a header in any letter case, and `response.succeeded()` tells a status from 200 to 299. Requests follow up to 10 redirects and accept bodies up to 16 MiB unless told otherwise; a failing status such as 404 is a response, not an error, while an answer that is not HTTP fails with `HttpError`. Both `http://` and `https://` URLs work, and a redirect may lead from one to the other; an `https://` request goes through TLS.

In a web page the browser makes each request, with its `fetch`, and the other tasks run while one waits for its response. The response is the same, with a few differences that come from the browser:

- The browser follows redirects itself, up to its own limit, so `redirects` only tells whether to follow any; `timeout` is how long the wait for the response, and then for each part of its body, may last.
- A page reads another site's responses only when that site allows it (CORS), and sees only the headers it exposes. The browser sets some headers itself, such as `user-agent`, and leaves out those the program sets for them.
- A URL can be relative to the page. A request the browser could not make fails with `NetworkError`, with only as much of why as the browser tells: it keeps from the page whether the connection was refused or the site does not allow it.

### Graphics

`standard/graphics.sayon` draws on canvases and shows them in windows.

```sayon
graphics = import("standard/graphics.sayon")

window = graphics.open("Hello", 320, 200)
canvas = graphics.canvas(window.width(), window.height())
while window.is_open()
    for event in window.events()
        match event
            case resized(width, height)
                canvas = graphics.canvas(width, height)
            case key_down(key)
                print("pressed", key)
            case _
                continue
    canvas.clear(graphics.BLACK)
    canvas.fill_circle(160, 100, 40, graphics.rgb(255, 160, 0))
    canvas.text("Hello", 20, 20, 16, graphics.WHITE)
    window.present(canvas)
```

- A `Canvas` is pixels to draw on: `graphics.canvas(width, height)` starts transparent, and `graphics.image(width, height, bytes)` takes red, green, blue, and alpha bytes for each pixel, row by row from the top, as `canvas.bytes()` gives them. Drawing gives the same pixels on every system, in the interpreter and compiled, and needs no window, so a program can draw where there is no screen, as under WASI.
- `clear` sets every pixel; `fill_rectangle`, `stroke_rectangle`, `fill_circle`, `fill_polygon`, and `line` (with a `thickness`) blend a `Color` over what is there, as `draw` does another canvas. Coordinates count pixels from the top left corner, rightward and downward. Shapes cover the pixels whose centers they hold; a polygon's points are pixel corners, so a square polygon covers what the rectangle of its size does, and a line runs between the centers of the pixels it names. Colors are `rgb(red, green, blue)` and `rgba(..., alpha)`, each part 0 to 255.
- On a canvas whose `smooth` is true (`canvas.smooth = true`; a canvas starts with it false), `fill_circle`, `fill_polygon`, and `line` are smooth instead: each pixel takes as much of the color as the shape covers of its area, so that edges are anti-aliased. A smooth line is the band as wide as its thickness, at least a pixel, its ends cut square at the centers. Smooth shapes give the same pixels everywhere too.
- `text(text, x, y, size, color)` draws in a built-in font of 8 by 8 pixels, scaled so that each character takes a square cell of `size` pixels; a line break starts a line below, and `text_width` and `text_height` measure text. The font is font8x8, in the public domain, and covers ASCII, the Latin-1 supplement, Greek, box drawing, and block elements; another character shows as a box.
- `write(font, text, x, y, size, color)` draws text in a `Font`, the top left corner of its first line at `x`, `y`, at `size` pixels per em, as a web page sizes text; a line break starts a line `font.line_height(size)` below. `graphics.decode_font(bytes)` and `graphics.load_font(path)` read a TrueType font (a `.ttf` file, or the first font of a `.ttc`), which the runtime draws itself, smooth, so that text in it is the same everywhere; there is no hinting or kerning yet, and fonts of PostScript outlines (most `.otf` files) fail with `FontError`. `graphics.system_font(name)` finds one of the system's fonts by its family's name, such as `"Arial"`, or its full name, such as `"Arial Bold"`; `"sans-serif"` (the default), `"serif"`, and `"monospace"` give the system's usual one of each kind. It looks in the system's font directories: on Windows its `Fonts` and the user's, on Linux `~/.local/share/fonts`, `~/.fonts`, and the `fonts` of `XDG_DATA_DIRS`, on Android `/system/fonts`. In a web page the fonts are the browser's, and the browser draws them, so their pixels are the browser's own. A font's `ascent`, `descent`, and `line_height` at a size, and the `width` and `height` of text in it, are in whole pixels. A character a font lacks shows as the font's glyph for those.
- `graphics.decode_image(bytes)` and `graphics.load_image(path)` read a PNG image into a canvas: any of PNG's kinds of color and depths, palettes, transparency, and interlacing, colors of 16 bits cut to 8. `canvas.png()` gives the canvas as a PNG image of 8-bit red, green, blue, and alpha, and `canvas.save_png(path)` writes it to a file. What is not a PNG image, or is damaged, fails with `ImageError`.
- `graphics.open(title, width, height)` opens a window: on a desktop a window of its own, in a web page a canvas added to the page, and in an Android app the app's screen, whatever its size. `width` and `height` are the system's points, and the window's own `width()` and `height()` are its pixels, which `scale()` relates, such as 2 on a screen of twice the usual density; a canvas of the window's size fills it a pixel for a pixel. `present(canvas)` shows a canvas, over black, its top left corner at the window's, and waits until the next frame is due, about a sixtieth of a second after the last, letting other tasks run.
- `events()` gives what happened since it was last asked: `closed` (the user asked to close it; `is_open()` is then false, and the program decides when to `close()`), `resized(width, height)`, which comes first with the window's size, `key_down(key)` and `key_up(key)`, `pointer_down(x, y, button)`, `pointer_up(...)`, and `pointer_moved(x, y)` for a mouse or a finger (button 1 for the primary or a touch, 2 the secondary, 3 the middle), `focused(focused)`, and `paused` and `resumed` when it is minimized or goes to the background. Keys are named as web pages name them: `"a"` to `"z"` and `"0"` to `"9"` by where the key is, `" "`, `"Enter"`, `"Escape"`, `"ArrowLeft"`, and so on.
- Opening a window where there is no screen fails with `GraphicsError`: under WASI, in Node, from `adb shell`, and on Linux without an X display. A window is used on the thread that opened it.

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

Unknown or misspelled manifest keys are errors. A `[compiler]` table names the project's [targets](#targets).

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

`sayon doctor` reports the Unicode version the toolchain follows and where the standard library, the runtime library, and clang were found. `sayon explain CODE` explains a diagnostic or runtime error code; [errors.md](errors.md) lists them all.

## Targets

A program is checked and built for a target: a platform and a processor. `sayon run`, `sayon test`, and `sayon build` without `--target` use this computer's own; `--target` names others, and can be given more than once:

```text
sayon check --target web          # check the program as a web page would run it
sayon check --target all          # the project's targets, or windows-x64 and linux-x64
sayon build --target wasi         # a WebAssembly program, on any computer
sayon build --target android-x64  # an Android app, with the Android SDK
sayon target list                 # every target, how far it has come, and what it lacks
sayon target setup wasi           # what building for one needs
```

| Target | Platform | Processor | State |
|---|---|---|---|
| `windows-x64` | `windows` | `x64` | built and run |
| `linux-x64` | `linux` | `x64` | built and run |
| `wasi` | `wasi` | `wasm32` | built on any computer, run under wasmtime |
| `web` | `web` | `wasm32` | built on any computer, run in Edge, Chrome, Firefox, and Node |
| `android-x64` | `android` | `x64` | built on any computer with the Android SDK, run on Android's emulator |
| `android-arm64` | `android` | `arm64` | built on any computer with the Android SDK, run on the emulator through Android's translation of ARM code |
| `windows-arm64`, `linux-arm64` | `windows`, `linux` | `arm64` | checked; built from M5 on |
| `macos-arm64`, `macos-x64` | `macos` | `arm64`, `x64` | checked; built from M7 on |
| `ios-arm64`, `ios-simulator-arm64` | `ios` | `arm64` | checked; built from M7 on |

`native` is this computer's target, and the short names `windows`, `linux`, `android`, `macos`, and `ios` are `windows-x64`, `linux-x64`, `android-arm64`, `macos-arm64`, and `ios-arm64`, for good: a later release never moves them to another processor. Each target's LLVM triple, such as `x86_64-unknown-linux-gnu`, names it too. A native program is built on the system it is for; for another native target, `sayon build` checks the program but stops before building it. A WebAssembly program is the same wherever it is built, so `wasi` and `web` build on any computer, and so do Android's programs, wherever the Android SDK is. `sayon build` writes a program for a target other than this computer's under `build/debug/<target>/`, or next to a standalone file as `<name>-<target>`, such as `hello-wasi.wasm`.

### WASI

A program built for `wasi` is a WebAssembly module for WASI preview 1 (`wasm32-wasip1`). It runs under a WASI host, which gives it only the folders it is told to:

```text
sayon build --target wasi hello.sayon      # hello-wasi.wasm
wasmtime run --dir . hello-wasi.wasm       # the current folder, as .
```

Building needs the same clang as native builds, with its WebAssembly linker (`wasm-ld`, part of LLVM), and Sayon's runtime for WASI with WASI's C library, which an installation keeps in `lib/wasm32-wasip1`. Programs behave as in the interpreter: Sayon's tests run each example under wasmtime 50 and compare. Tasks and channels work as everywhere and take turns in the same order: WebAssembly has no separate stacks for them, so the compiler turns each function that can wait into one that can stop and continue later, which costs a little more on each call of such a function. WASI has no sockets, threads, or other programs.

### Web pages

A program built for `web` is a WebAssembly module for browsers (`wasm32-unknown-unknown`), with two files the compiler writes beside it: the glue that runs it (`.js`) and a page that shows its output (`.html`):

```text
sayon build --target web hello.sayon       # hello-web.wasm, hello-web.js, hello-web.html
python -m http.server                      # any server: browsers load modules only over HTTP
```

Opening `hello-web.html` from the server runs the program and shows what it prints, its errors in red. A page of your own runs it through the glue, an ES module that works in browsers and in Node:

```js
import { run } from "./hello-web.js";
const status = await run({ write: (stream, text) => show(stream, text) });
```

`run` loads the module from beside the glue, starts the program, and gives its exit status when it ends; `write` receives what the program writes to its output (1) and errors (2), by default the console a line at a time. The page is never blocked: while every task sleeps or waits for a response, the program returns to the browser, which wakes it with a timer or when the response arrives. Ending the program, with `os.exit` or a fault, stops only the program, not the page.

The module imports nothing but the glue's small set of host functions (output, clocks, random numbers, fetching, and ending); a page has no files, standard input, environment, sockets, threads, other programs, or C functions. Its HTTP requests are the browser's: `network.get`, `post`, and `request` work as elsewhere through `fetch` (see [Networking](#networking)). Sayon's tests run each example in Edge, Chrome, and Firefox, headless, from a server on the same computer, and in Node 24, and compare each with the interpreter.

### Android

A program built for Android is an app: its package (`.apk`) holds the program, compiled for the phone's processor, and Android's own `NativeActivity` starts it, so the app has no Java code.

```text
sayon build --target android-x64 hello.sayon   # hello-android-x64.apk
adb install hello-android-x64.apk
adb shell am start -n sayon.app.hello/android.app.NativeActivity
adb logcat -s sayon sayon-app                  # what it prints
```

`android-arm64` is for phones and `android-x64` for the emulator on a PC. The program runs on a thread of its own when the app starts, so the app keeps answering. What it prints goes to Android's log under the tag `sayon` (its errors at the error level), a line at a time, and when it ends the log says with which status and the app closes. The package is named `sayon.app.` and the program's name, and asks for the internet permission when the program uses the network. Packages run on Android 8.0 (API 26) and newer, are made for Android 16 (API 36), keep the program aligned for phones with 16 KiB pages, and are signed with the debug key Android's tools share, in `.android` in your home folder; a debug build is debuggable. Release signing and store packages come later.

Building needs the Android SDK (`ANDROID_HOME`, or where Android Studio puts it) with its NDK, build tools, and platform `android-36`, and a JDK, 17 or newer, for signing (`JAVA_HOME`); `sayon target setup android-x64` says what is missing. `--executable` builds a program to run from `adb shell` instead, with the shell's own output and errors and an exit status, as Sayon's tests run each example; that shows the runtime works, not that an app does, which is why the tests run some examples as apps too.

An app has the network, threads, C functions, and files of its own: a program starts in the app's private directory, so the files it names by relative paths are the app's. It has no standard input, environment, or other programs. TLS, and so `https://`, is Android's own, which comes from its Java libraries: an app has it, and a program run from `adb shell` does not. Time zones are the phone's, its own zone the local one.

### Platform branches

```sayon
if platform == "web"
    show_in_page(text)
elseif platform == "android"
    show_on_phone(text)
else
    print(text)
```

`platform` is the target's platform: `"windows"`, `"linux"`, `"macos"`, `"android"`, `"ios"`, `"web"`, or `"wasi"`. `processor` is `"x64"`, `"arm64"`, or `"wasm32"`. `supports("name")` is whether the target has a [capability](#capabilities).

An `if` whose condition is made only of these, compared with written names and joined by `and`, `or`, and `not`, is decided by the compiler: only the branch the target selects is checked and compiled. So a branch can call what exists only on its platform, and the others need not even be defined for this target; every branch must still be valid syntax. Checking with `--target` checks the branch of that target, and `--target all` each one. A written name that is no platform, processor, or capability is an error, since its branch would silently never apply.

Anywhere else, `platform` and `processor` are strings and `supports(...)` is a boolean, all fixed when the program is compiled. A variable, function, or import of the program's own with one of these names hides it and is an ordinary value, so `if platform == "web"` after `platform = "web"` is an ordinary condition.

### Capabilities

Each target provides capabilities: what its programs can ask of the system.

| Capability | What it covers | Targets without it |
|---|---|---|
| `console` | `print` and standard output and errors | none |
| `input` | reading standard input | web, Android, iOS |
| `files` | files and directories | web |
| `environment` | environment variables, the current directory, program arguments | web, Android, iOS |
| `processes` | `os.run`, `os.start` | web, WASI, Android, iOS |
| `network` | TCP and UDP sockets, resolving names | web, WASI |
| `http` | the HTTP client | WASI |
| `tls` | `network.connect_secure`, `connection.secure`, `https://` | web, WASI |
| `graphics` | `graphics.open`: windows and the screen (drawing on a canvas needs none) | WASI |
| `tasks` | tasks, channels, `sleep`, `select` | none |
| `threads` | `threads.spawn_parallel` | web, WASI |
| `foreign` | C functions and `standard/memory` | web |
| `time_zones` | IANA time zones | web, WASI |
| `clock`, `random`, `json`, `math`, `text`, `collections` | the rest of the standard library | none |

A program needs a capability when it can reach an operation that uses it: the compiler follows every function the program calls or uses as a value, from its entry through the standard library and its dependencies. Checking or building for a target that lacks one is an error (`E_CAPABILITY`) at the program's own code that leads to the operation. Code in a branch the target does not select needs nothing, so `if supports("files")` keeps file access to the targets with files. A program never meets a capability's absence while it runs.

A project can name its targets and the capabilities it cannot do without:

```toml
[compiler]
targets = ["native", "web"]                    # what --target all means
required_capabilities = ["console", "files"]   # every target must have these
```

Before checking, `check` and `build` stop for any target that lacks a required capability.

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
