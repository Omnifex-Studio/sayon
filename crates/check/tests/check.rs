use sayon_check::check_source;
use sayon_check::hir::{ExprKind, StmtKind, Usage};
use sayon_check::types::Type;
use sayon_syntax::ast::ParamMode;
use sayon_syntax::diagnostic::render;
use sayon_syntax::{FileId, SourceMap};

fn errors(source: &str) -> Vec<&'static str> {
    let (_, diagnostics) = check_source(FileId(0), source);
    diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.code)
        .collect()
}

#[track_caller]
fn ok(source: &str) -> sayon_check::hir::Program {
    let (program, diagnostics) = check_source(FileId(0), source);
    if diagnostics.has_errors() || program.is_none() {
        let mut sources = SourceMap::new();
        sources.add("test.sayon", None, source.to_string());
        let rendered: Vec<String> = diagnostics
            .iter()
            .map(|d| render(d, &sources, false))
            .collect();
        panic!("expected success, got:\n{}", rendered.join("\n"));
    }
    program.unwrap()
}

#[track_caller]
fn fails(source: &str, code: &str) {
    let found = errors(source);
    assert!(found.contains(&code), "expected {code}, got {found:?}");
}

fn function<'p>(
    program: &'p sayon_check::hir::Program,
    name: &str,
) -> &'p sayon_check::hir::Function {
    program
        .functions
        .iter()
        .find(|f| f.name == name)
        .expect("function")
}

#[test]
fn literals_and_bindings() {
    ok(
        "count = 42\nmask = 0xFF\nratio = 3.14\npositive: unsigned = 255u\nenabled = true\nmissing = nothing\nnumbers = [1, 2, 3]\nprofile = [\"name\" = \"Alice\", \"city\" = \"Paris\"]\nconstant MAX_SIZE = 100\nvariable total = 0\n",
    );
}

#[test]
fn sized_literals_take_their_context() {
    ok("small: integer8 = -128\nbyte_value: byte = 255\n");
    fails("small: integer8 = 128\n", "E_LITERAL_RANGE");
    fails("x: unsigned = -1\n", "E_LITERAL_RANGE");
    fails("x: float = 1\n", "E_TYPE_MISMATCH");
}

#[test]
fn no_implicit_numeric_conversion() {
    fails("a = 1\nb = 2.5\nc = a + b\n", "E_TYPE_MISMATCH");
    ok("a = 1\nb = 2.5\nc = float(a) + b\n");
    fails("x = \"a\" + 1\n", "E_TYPE_MISMATCH");
    ok("x = \"a\" + \"b\"\n");
}

#[test]
fn inferred_generic_functions() {
    let program = ok(
        "function identity(x)\n    return x\n\nfunction add(a, b)\n    return a + b\n\nprint(identity(42))\nprint(identity(\"text\"))\nprint(add(1, 2))\nprint(add(1.5, 2.5))\nprint(add(\"a\", \"b\"))\n",
    );
    let identity = function(&program, "identity");
    assert_eq!(identity.generic_count, 1);
    assert_eq!(identity.params[0].mode, ParamMode::Own);
    let add = function(&program, "add");
    assert_eq!(add.generic_count, 1);
    fails(
        "function add(a, b)\n    return a + b\n\nprint(add(true, false))\n",
        "E_UNSUPPORTED_OPERATION",
    );
}

#[test]
fn explicit_generics() {
    ok(
        "function wrap<T>(own value: T) -> Array<T>\n    return [value]\n\nnumbers = wrap<integer>(42)\nwords = wrap(\"a\")\n",
    );
    fails(
        "function bad<T>(a: T, b: T) -> T\n    return a + b\n",
        "E_UNSUPPORTED_OPERATION",
    );
}

#[test]
fn recursion() {
    ok(
        "function factorial(n)\n    if n <= 1\n        return 1\n    return n * factorial(n - 1)\n\nprint(factorial(10))\n",
    );
    ok(
        "function is_even(n: integer) -> boolean\n    if n == 0\n        return true\n    return is_odd(n - 1)\n\nfunction is_odd(n: integer) -> boolean\n    if n == 0\n        return false\n    return is_even(n - 1)\n\nprint(is_even(10))\n",
    );
}

#[test]
fn missing_return() {
    fails(
        "function f(x: integer) -> integer\n    if x > 0\n        return 1\n",
        "E_MISSING_RETURN",
    );
    ok(
        "function f(x: integer) -> integer\n    if x > 0\n        return 1\n    else\n        return 2\n",
    );
    ok(
        "function f(x: integer) -> integer\n    if x > 0\n        return 1\n    error(\"negative\")\n",
    );
}

#[test]
fn unions_and_narrowing() {
    ok(
        "function find(id: integer) -> integer or nothing\n    if id == 1\n        return 10\n    return nothing\n\nresult = find(1)\nif result != nothing\n    print(result + 1)\n",
    );
    fails(
        "function find(id: integer) -> integer or nothing\n    return nothing\n\nresult = find(1)\nprint(result + 1)\n",
        "E_TYPE_MISMATCH",
    );
    ok(
        "function get() -> integer or string\n    return 1\n\nvalue = get()\nif typeof(value) == \"integer\"\n    print(value + 10)\nelse\n    print(value + \"text\")\n",
    );
    ok(
        "function get() -> integer or nothing\n    return 1\n\nvalue = get()\nif value == nothing\n    error(\"missing\")\nprint(value * 2)\n",
    );
    ok(
        "function get() -> integer or nothing\n    return 1\n\nvalue = get()\nif value != nothing and value > 3\n    print(value)\n",
    );
}

#[test]
fn narrowing_is_invalidated_by_assignment() {
    fails(
        "function get() -> integer or nothing\n    return 1\n\nvalue = get()\nif value != nothing\n    value = get()\n    print(value + 1)\n",
        "E_TYPE_MISMATCH",
    );
    fails(
        "function get() -> integer or nothing\n    return 1\n\nvalue = get()\nif value != nothing\n    while true\n        print(value + 1)\n        value = get()\n",
        "E_TYPE_MISMATCH",
    );
}

#[test]
fn heterogeneous_arrays() {
    let program = ok("mixed = [1, \"text\", true]\n");
    let entry = &program.functions[program.entry.0 as usize];
    let StmtKind::Let { value, .. } = &entry.body.stmts[0].kind else {
        panic!()
    };
    assert_eq!(value.ty.to_string(), "Array<integer or boolean or string>");
}

#[test]
fn pop_returns_an_optional_value() {
    ok("numbers = [1, 2]\nlast = pop(numbers)\nif last != nothing\n    print(last + 1)\n");
}

#[test]
fn empty_collections_need_a_type() {
    fails("values = []\n", "E_AMBIGUOUS_TYPE");
    ok("values: Array<string> = []\npush(values, \"a\")\n");
    ok("values = []\npush(values, 1)\n");
    ok("counts: Dict<string, integer> = []\ncounts[\"a\"] = 1\n");
}

#[test]
fn undefined_names_suggest_alternatives() {
    let (_, diagnostics) = check_source(FileId(0), "total = 1\nprint(totl)\n");
    let diagnostic = diagnostics
        .iter()
        .find(|d| d.code == "E_UNDEFINED_NAME")
        .expect("error");
    assert!(diagnostic.help.iter().any(|h| h.contains("total")));
}

#[test]
fn functions_cannot_see_script_variables() {
    let (_, diagnostics) = check_source(
        FileId(0),
        "name = \"x\"\nfunction greet()\n    print(name)\n",
    );
    let diagnostic = diagnostics
        .iter()
        .find(|d| d.code == "E_UNDEFINED_NAME")
        .expect("error");
    assert!(!diagnostic.notes.is_empty());
    ok("constant NAME = \"x\"\nfunction greet()\n    print(NAME)\n\ngreet()\n");
}

#[test]
fn scopes() {
    fails("if true\n    inner = 1\nprint(inner)\n", "E_UNDEFINED_NAME");
    ok("total = 0\nif true\n    total = 1\nprint(total)\n");
    fails(
        "variable x = 1\nvariable x = 2\n",
        "E_DUPLICATE_DECLARATION",
    );
    fails("constant LIMIT = 1\nLIMIT = 2\n", "E_ASSIGN_CONSTANT");
    fails("for i in range(3)\n    i = 2\n", "E_ASSIGN_CONSTANT");
}

#[test]
fn loops() {
    ok(
        "for i in range(10)\n    print(i)\nfor name in [\"a\", \"b\"]\n    print(name)\nages = [\"a\" = 1]\nfor key in ages\n    print(key)\nfor key, value in ages\n    print(\"{key}: {value}\")\n",
    );
    fails("break\n", "E_OUTSIDE_LOOP");
    fails("for a, b in [1, 2]\n    print(a)\n", "E_LOOP_BINDINGS");
}

#[test]
fn untyped_parameters_used_as_collections() {
    ok(
        "function total(values)\n    result = 0\n    for value in values\n        result = result + value\n    return result\n\nprint(total([1, 2, 3]))\n",
    );
}

#[test]
fn errors_and_try() {
    ok(
        "function divide(a, b)\n    if b == 0\n        error(\"Division by zero\")\n    return a / b\n\ntry\n    value = divide(10, 0)\ncatch problem\n    print(\"Error: {problem}\")\n    print(problem.message)\nfinally\n    print(\"Operation finished\")\n",
    );
    fails(
        "for i in range(2)\n    try\n        print(i)\n    finally\n        break\n",
        "E_FINALLY_TRANSFER",
    );
}

#[test]
fn unavailable_features_name_their_milestone() {
    fails("async function work()\n    return 1\n", "E_UNAVAILABLE");
    fails("unsafe\n    x = 1\n", "E_UNAVAILABLE");
}

// ----- function values and closures -------------------------------------------

#[test]
fn functions_are_values() {
    ok("function add(a, b)\n    return a + b\n\nf = add\nprint(f(1, 2))\n");
    ok("function apply(f, x)\n    return f(x)\n\nprint(apply(function(x) return x + 1, 2))\n");
    ok("limit = 3\nkeep = function(x: integer) return x < limit\nprint(keep(2), limit)\n");
    fails("n = 5\nprint(n(1))\n", "E_NOT_CALLABLE");
    fails(
        "f = function(x) return x\nprint(f(1, 2))\n",
        "E_ARGUMENT_COUNT",
    );
    fails(
        "function grow(items)\n    push(items, 1)\n\ng = grow\n",
        "E_PARAMETER_MODE",
    );
}

#[test]
fn closures_own_what_they_capture() {
    ok(
        "function counter()\n    count = 0\n    return function()\n        count += 1\n        return count\n\nc = counter()\nprint(c(), c())\n",
    );
    fails(
        "names = [\"a\"]\nf = function() return length(names)\nprint(names)\n",
        "E_MOVED_VALUE",
    );
    fails(
        "total = 0\nadd = function(x: integer)\n    total += x\nadd(1)\nprint(total)\n",
        "E_MOVED_VALUE",
    );
    fails(
        "label = \"a\"\nf = function() return label\n",
        "E_MOVE_CAPTURED",
    );
    fails(
        "function counter()\n    count = 0\n    return function()\n        count += 1\n        return count\n\nconstant c = counter()\nprint(c())\n",
        "E_MUTATE_CONSTANT",
    );
    ok("constant square = function(x: integer) return x * x\nprint(square(3))\n");
}

// ----- structures and enums ---------------------------------------------------

const POINT: &str = "type Point = [x: float, y: float]\n";
const SHAPE: &str = "enum Shape\n    circle(radius: float)\n    point\n\n";

#[test]
fn structures_are_built_with_every_field() {
    ok(&format!(
        "{POINT}p = Point(x = 1.0, y = 2.0)\nq: Point = [x = 0.0, y = p.y]\np.x = 3.0\nprint(p == q)\n"
    ));
    fails(&format!("{POINT}p = Point(x = 1.0)\n"), "E_MISSING_FIELD");
    fails(
        &format!("{POINT}p = Point(x = 1.0, y = 2.0, z = 3.0)\n"),
        "E_UNKNOWN_FIELD",
    );
    fails(
        &format!("{POINT}p = Point(x = 1.0, x = 2.0, y = 0.0)\n"),
        "E_DUPLICATE_FIELD",
    );
    fails(&format!("{POINT}p = Point(1.0, 2.0)\n"), "E_NAMED_FIELDS");
    fails(
        &format!("{POINT}p = Point(x = 1, y = 2.0)\n"),
        "E_TYPE_MISMATCH",
    );
    fails(
        &format!("{POINT}print(Point(x = 1.0, y = 2.0).z)\n"),
        "E_UNKNOWN_FIELD",
    );
    fails("x = [a = 1]\n", "E_RECORD_TYPE");
    fails(&format!("{POINT}print(Point)\n"), "E_TYPE_VALUE");
}

#[test]
fn type_declarations_are_checked() {
    fails("type Empty = []\n", "E_EMPTY_STRUCTURE");
    fails("type Node = [next: Node or nothing]\n", "E_RECURSIVE_TYPE");
    fails(
        "type A = [b: B]\ntype B = [a: A or nothing]\n",
        "E_RECURSIVE_TYPE",
    );
    ok("type Node = [value: integer, children: Array<Node>]\n");
    fails("type P = [x: integer, x: integer]\n", "E_DUPLICATE_FIELD");
    fails("enum E\n    a\n    a\n", "E_DUPLICATE_VARIANT");
    fails("type P = [x: Missing]\n", "E_UNKNOWN_TYPE");
    fails(
        "function f()\n    type Local = [x: integer]\n",
        "E_TYPE_POSITION",
    );
}

#[test]
fn field_access_infers_the_structure() {
    let program = ok(&format!(
        "{POINT}function norm(p)\n    return p.x * p.x + p.y * p.y\n\nprint(norm(Point(x = 3.0, y = 4.0)))\n"
    ));
    assert_eq!(function(&program, "norm").params[0].ty.to_string(), "Point");
}

#[test]
fn structures_of_copy_fields_are_copied() {
    ok(&format!(
        "{POINT}a = Point(x = 1.0, y = 2.0)\nb = a\nprint(a)\n"
    ));
    let person = "type Person = [name: string]\n";
    fails(
        &format!("{person}a = Person(name = \"x\")\nb = a\nprint(a)\n"),
        "E_MOVED_VALUE",
    );
    fails(
        &format!("{person}a = Person(name = \"x\")\nn = a.name\n"),
        "E_MOVE_OUT_OF_FIELD",
    );
    ok(&format!(
        "{person}function make() -> Person\n    return Person(name = \"x\")\n\nn = make().name\n"
    ));
    fails(
        &format!("{person}function rename(p: Person)\n    p.name = \"y\"\n"),
        "E_MUTATE_BORROWED",
    );
    ok(&format!(
        "{person}function rename(update p: Person)\n    p.name = \"y\"\n"
    ));
}

#[test]
fn update_parameters_reach_copy_values() {
    ok("function bump(update n: integer)\n    n += 1\n\nx = 1\nbump(x)\nprint(x)\n");
    fails(
        "constant X = 1\nfunction bump(update n: integer)\n    n += 1\n\nbump(X)\n",
        "E_MUTATE_CONSTANT",
    );
}

#[test]
fn variants_are_built_and_matched() {
    ok(&format!(
        "{SHAPE}s = circle(1.0)\nmatch s\n    case circle(r)\n        print(r)\n    case point\n        print(0)\n"
    ));
    ok(&format!("{SHAPE}s: Shape = point\nt = Shape.circle(2.0)\n"));
    fails(&format!("{SHAPE}s = circle()\n"), "E_ARGUMENT_COUNT");
    fails(&format!("{SHAPE}s = circle\n"), "E_VARIANT_FIELDS");
    fails(
        &format!("{SHAPE}s = Shape.square(1.0)\n"),
        "E_UNKNOWN_VARIANT",
    );
    fails(
        "enum A\n    same\n\nenum B\n    same\n\nx = same\n",
        "E_AMBIGUOUS_VARIANT",
    );
}

#[test]
fn matches_must_be_exhaustive() {
    fails(
        &format!("{SHAPE}s = circle(1.0)\nmatch s\n    case circle(r)\n        print(r)\n"),
        "E_NONEXHAUSTIVE",
    );
    fails(
        &format!(
            "{SHAPE}s = circle(1.0)\nmatch s\n    case circle(r) if r > 1.0\n        print(r)\n    case point\n        print(0)\n"
        ),
        "E_NONEXHAUSTIVE",
    );
    fails(
        "n = 1\nmatch n\n    case 1\n        print(1)\n",
        "E_NONEXHAUSTIVE",
    );
    ok(
        "flag = true\nmatch flag\n    case true\n        print(1)\n    case false\n        print(0)\n",
    );
    fails(
        &format!(
            "{SHAPE}s = circle(1.0)\nmatch s\n    case circle\n        print(1)\n    case _\n        print(0)\n"
        ),
        "E_PATTERN_ARITY",
    );
    fails(
        &format!(
            "{SHAPE}s = circle(1.0)\nmatch s\n    case square(x)\n        print(1)\n    case _\n        print(0)\n"
        ),
        "E_UNKNOWN_VARIANT",
    );
    let (_, diagnostics) = check_source(
        FileId(0),
        "n = 1\nmatch n\n    case _\n        print(1)\n    case 2\n        print(2)\n",
    );
    assert!(diagnostics.iter().any(|d| d.code == "W_UNREACHABLE_CASE"));
}

#[test]
fn try_expressions_capture_errors_as_outcomes() {
    let program = ok(
        "function f(x: integer) -> integer\n    if x == 0\n        error(\"zero\")\n    return x\n\nr = try f(1)\nif r is error\n    print(r.message)\nelse\n    print(r + 1)\n",
    );
    let entry = &program.functions[program.entry.0 as usize];
    let StmtKind::Let { value, .. } = &entry.body.stmts[0].kind else {
        panic!()
    };
    assert_eq!(value.ty.to_string(), "Outcome<integer>");
    ok(
        "r: Outcome<integer8> = try 5\nmatch r\n    case success(n)\n        print(n)\n    case failure(problem)\n        print(problem)\n",
    );
    fails("r = try tonumber(\"1\")\nprint(r + 1)\n", "E_TYPE_MISMATCH");
    fails(
        "r = try tonumber(\"1\")\nprint(r.message)\n",
        "E_UNCHECKED_OUTCOME",
    );
    fails("n = 1\nif n is error\n    print(n)\n", "E_TYPE_MISMATCH");
    ok(
        "values: Array<integer or Error> = [1]\nv = pop(values)\nif v is error\n    print(v.message)\n",
    );
    let (_, diagnostics) = check_source(FileId(0), "try tonumber(\"1\")\n");
    assert!(diagnostics.iter().any(|d| d.code == "W_UNUSED_OUTCOME"));
}

#[test]
fn outcome_payloads_move_out_of_their_local() {
    fails(
        "function f() -> string\n    return \"x\"\n\nr = try f()\nif not (r is error)\n    kept = r\n    print(r)\n",
        "E_MOVED_VALUE",
    );
}

#[test]
fn case_bindings_narrow_unions() {
    ok(
        "function f(v: integer or nothing) -> integer\n    match v\n        case nothing\n            return 0\n        case n\n            return n + 1\n",
    );
}

#[test]
fn matches_borrow_or_take_their_value() {
    let boxed = "enum Box\n    full(text: string)\n    empty\n\n";
    fails(
        &format!(
            "{boxed}b = full(\"x\")\nmatch b\n    case full(text)\n        kept = text\n    case empty\n        print(0)\n"
        ),
        "E_MOVE_BORROWED",
    );
    ok(&format!(
        "{boxed}b = full(\"x\")\nmatch move(b)\n    case full(text)\n        kept = text\n    case empty\n        print(0)\n"
    ));
    fails(
        &format!(
            "{boxed}b = full(\"x\")\nmatch b\n    case full(text)\n        b = empty\n    case empty\n        print(0)\n"
        ),
        "E_BORROW_CONFLICT",
    );
    ok(&format!(
        "{boxed}b = full(\"x\")\nmatch b\n    case empty\n        b = full(\"y\")\n    case _\n        print(0)\n"
    ));
}

#[test]
fn main_function_is_the_entry() {
    let program = ok("function main()\n    print(\"hi\")\n");
    assert_eq!(program.functions[program.entry.0 as usize].name, "main");
    fails("function main(x)\n    print(x)\n", "E_MAIN_SIGNATURE");
}

#[test]
fn constants_are_initialized_in_dependency_order() {
    let program = ok("constant B = A + 1\nconstant A = 41\nprint(B)\n");
    let order: Vec<&str> = program
        .init_order
        .iter()
        .map(|id| program.constants[id.0 as usize].name.as_str())
        .collect();
    assert_eq!(order, vec!["A", "B"]);
    fails("constant A = A + 1\n", "E_INITIALIZATION_CYCLE");
}

// ----- ownership ------------------------------------------------------------

#[test]
fn assignment_moves_owned_values() {
    fails(
        "first = [1, 2, 3]\nsecond = first\nprint(first)\n",
        "E_MOVED_VALUE",
    );
    ok("first = [1, 2, 3]\nsecond = clone(first)\npush(second, 4)\nprint(first)\nprint(second)\n");
    ok("a = 1\nb = a\nprint(a)\n");
}

#[test]
fn moved_value_diagnostic_names_both_sites() {
    let source = "numbers = [1, 2]\nother = numbers\nprint(numbers)\n";
    let (_, diagnostics) = check_source(FileId(0), source);
    let diagnostic = diagnostics
        .iter()
        .find(|d| d.code == "E_MOVED_VALUE")
        .expect("error");
    assert_eq!(diagnostic.labels.len(), 2);
    assert!(diagnostic.help[0].contains("other = clone(numbers)"));
}

#[test]
fn reassignment_reinitializes() {
    ok("a = [1]\nb = a\na = [2]\nprint(a)\nprint(b)\n");
}

#[test]
fn moves_on_some_paths() {
    fails("a = [1]\nif true\n    b = a\nprint(a)\n", "E_MOVED_VALUE");
}

#[test]
fn moves_inside_loops() {
    fails("a = [1]\nfor i in range(3)\n    b = a\n", "E_MOVED_VALUE");
    ok("for i in range(3)\n    a = [i]\n    b = a\n");
}

#[test]
fn calls_borrow_by_default() {
    let program = ok(
        "function total(values: Array<integer>) -> integer\n    result = 0\n    for value in values\n        result = result + value\n    return result\n\nnumbers = [1, 2, 3]\nprint(total(numbers))\nprint(length(numbers))\n",
    );
    let entry = &program.functions[program.entry.0 as usize];
    let StmtKind::Expr(print) = &entry.body.stmts[1].kind else {
        panic!()
    };
    let ExprKind::Builtin { args, .. } = &print.kind else {
        panic!()
    };
    let ExprKind::Call {
        args: call_args, ..
    } = &args[0].expr.kind
    else {
        panic!()
    };
    assert!(matches!(
        call_args[0].expr.kind,
        ExprKind::Local {
            usage: Usage::Borrow,
            ..
        }
    ));
}

#[test]
fn parameter_modes() {
    fails(
        "function f(values: Array<integer>)\n    push(values, 1)\n",
        "E_MUTATE_BORROWED",
    );
    ok(
        "function f(update values: Array<integer>)\n    push(values, 1)\n\nnumbers = [1]\nf(numbers)\nprint(numbers)\n",
    );
    fails(
        "function keep(values: Array<integer>) -> Array<integer>\n    return values\n",
        "E_MOVE_BORROWED",
    );
    ok("function keep(own values: Array<integer>) -> Array<integer>\n    return values\n");
    let program = ok(
        "function add_one(values)\n    push(values, 1)\n\nlist = [0]\nadd_one(list)\nprint(list)\n",
    );
    assert_eq!(
        function(&program, "add_one").params[0].mode,
        ParamMode::Update
    );
}

#[test]
fn transfer_to_owning_parameter() {
    fails(
        "function consume(own values: Array<integer>)\n    print(values)\n\nnumbers = [1]\nconsume(numbers)\nprint(numbers)\n",
        "E_MOVED_VALUE",
    );
}

#[test]
fn loops_borrow_their_collection() {
    fails(
        "numbers = [1, 2]\nfor n in numbers\n    push(numbers, n)\n",
        "E_BORROW_CONFLICT",
    );
    fails(
        "names = [\"a\"]\nfor name in names\n    other = name\n",
        "E_MOVE_BORROWED",
    );
    ok(
        "names = [\"a\"]\ncopies: Array<string> = []\nfor name in names\n    push(copies, clone(name))\nprint(names)\n",
    );
}

#[test]
fn elements_cannot_be_moved_out() {
    fails(
        "names = [\"a\"]\nfirst = names[0]\n",
        "E_MOVE_OUT_OF_ELEMENT",
    );
    ok(
        "names = [\"a\"]\nfirst = clone(names[0])\nprint(names[0])\nnumbers = [1]\nn = numbers[0]\n",
    );
}

#[test]
fn conflicting_arguments() {
    fails("numbers = [1]\npush(numbers, numbers)\n", "E_TYPE_MISMATCH");
    fails("lists = [[1]]\npush(lists, lists)\n", "E_TYPE_MISMATCH");
    ok("numbers = [1]\npush(numbers, numbers[0])\n");
    fails(
        "function both(update a: Array<integer>, b: Array<integer>)\n    print(b)\n\nx = [1]\nboth(x, x)\n",
        "E_BORROW_CONFLICT",
    );
}

#[test]
fn constants_cannot_be_mutated_or_moved() {
    fails(
        "constant NAMES = [\"a\"]\nfunction f()\n    push(NAMES, \"b\")\n",
        "E_MUTATE_CONSTANT",
    );
    fails(
        "constant NAMES = [\"a\"]\nfunction f()\n    taken = NAMES\n",
        "E_MOVE_CONSTANT",
    );
    fails(
        "function f()\n    constant items = [1]\n    push(items, 2)\n",
        "E_MUTATE_CONSTANT",
    );
}

#[test]
fn explicit_move() {
    fails("a = [1]\nb = move(a)\nprint(a)\n", "E_MOVED_VALUE");
}

#[test]
fn try_bodies_make_moves_uncertain() {
    fails(
        "function consume(own values: Array<integer>)\n    print(values)\n\nnumbers = [1]\ntry\n    consume(numbers)\ncatch problem\n    print(numbers)\n",
        "E_MOVED_VALUE",
    );
}

#[test]
fn narrowed_payload_moves_the_union() {
    fails(
        "function get() -> string or nothing\n    return \"x\"\n\nvalue = get()\nif value != nothing\n    other = value\n    print(value)\n",
        "E_MOVED_VALUE",
    );
}

#[test]
fn types_of_locals_are_resolved() {
    let program = ok("x = 1\ny = 2.5\nz = \"s\"\n");
    let entry = &program.functions[program.entry.0 as usize];
    let types: Vec<&Type> = entry.locals.iter().map(|l| &l.ty).collect();
    assert_eq!(types, vec![&Type::INTEGER, &Type::FLOAT, &Type::String]);
}
