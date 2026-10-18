//! Runtime values and their display, equality, and ordering rules.

use sayon_check::types::{TypeDef, TypeId};
use sayon_ir::FunctionId;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

/// A runtime value. Owned values form a tree: moving a value moves the whole
/// tree, and dropping it releases everything it owns.
#[derive(Clone, Debug)]
pub enum Value {
    /// Every integer width; the static type decides the valid range.
    Int(i128),
    F64(f64),
    F32(f32),
    Bool(bool),
    Nothing,
    Str(Rc<str>),
    Array(Vec<Value>),
    Dict(Box<Dict>),
    /// A union value: member tag (starting at 1) and payload.
    Union(u32, Box<Value>),
    /// A structure value: its type and fields in declaration order.
    Record(TypeId, Vec<Value>),
    /// An enum value: its type, variant index, and the variant's fields.
    Variant(TypeId, u32, Vec<Value>),
    Error(Box<ErrorValue>),
    Range {
        start: i64,
        end: i64,
        step: i64,
    },
    /// A function value: the code to call and its environment, a structure
    /// of captured values (`Nothing` when it captures none).
    Function(FunctionId, Box<Value>),
    /// A view of `length` elements of the array at a reference, from
    /// `start`.
    View(Box<ViewValue>),
    Stream(StreamRef),
    /// A `Shared` handle; cloning the value retains it.
    Shared(Rc<RefCell<Value>>),
    Weak(std::rc::Weak<RefCell<Value>>),
    Task(TaskHandle),
    /// A channel handle; cloning the value retains it.
    Channel(Rc<RefCell<ChannelState>>),
}

/// What the scheduler and a task's handle know of the task.
#[derive(Debug, Default)]
pub struct TaskState {
    /// The task's result once it has finished.
    pub result: Option<Result<Value, ErrorValue>>,
    /// Set when the task is to stop at its next wait.
    pub cancel: bool,
    pub joined: bool,
    pub detached: bool,
}

/// The handle of a task. The scheduler holds the task's state too, so when
/// the last handle goes without a join or a detach, the task is cancelled.
#[derive(Debug, Clone)]
pub struct TaskHandle(pub Rc<RefCell<TaskState>>);

impl Drop for TaskHandle {
    fn drop(&mut self) {
        if Rc::strong_count(&self.0) == 2 {
            let mut state = self.0.borrow_mut();
            if !state.joined && !state.detached {
                state.cancel = true;
            }
        }
    }
}

/// A channel's messages, in the order they were sent.
#[derive(Debug)]
pub struct ChannelState {
    pub queue: std::collections::VecDeque<Value>,
    /// 0 for a rendezvous: each message waits until it is received.
    pub capacity: usize,
    pub closed: bool,
    pub sent: u64,
    pub received: u64,
}

impl ChannelState {
    /// Whether a receive would not wait: a message is there, or none will come.
    pub fn receivable(&self) -> bool {
        !self.queue.is_empty() || self.closed
    }

    /// Whether a send would not wait for room.
    pub fn has_room(&self) -> bool {
        self.closed || self.queue.len() < self.capacity.max(1)
    }
}

/// A reference to an open stream of the runtime. A clone refers to the
/// same file, and the file is closed when the last reference is dropped.
#[derive(Debug)]
pub struct StreamRef(i64);

impl StreamRef {
    /// Takes over the reference a new stream comes with.
    pub fn adopt(id: i64) -> StreamRef {
        StreamRef(id)
    }

    pub fn id(&self) -> i64 {
        self.0
    }
}

impl Clone for StreamRef {
    fn clone(&self) -> StreamRef {
        sayon_runtime::streams::share(self.0);
        StreamRef(self.0)
    }
}

impl Drop for StreamRef {
    fn drop(&mut self) {
        sayon_runtime::streams::release(self.0);
    }
}

/// Where a value is stored: a frame's slot, or a module constant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Root {
    Slot { frame: usize, slot: usize },
    Global(usize),
}

/// One step from a value into a part of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Index(usize),
    DictValue(usize),
    DictKey(usize),
    Payload,
    /// A field of a structure, or of an enum value's variant.
    Field(usize),
    /// The environment of a function value.
    Env,
    /// The value a `Shared` handle refers to.
    Deref,
}

/// A stored value, or a part of one.
#[derive(Clone, Debug, PartialEq)]
pub struct Ref {
    pub root: Root,
    pub path: Vec<Step>,
}

#[derive(Clone, Debug)]
pub struct ViewValue {
    /// The array the view shows.
    pub array: Ref,
    pub start: usize,
    pub length: usize,
}

impl Value {
    pub fn str(text: &str) -> Value {
        Value::Str(Rc::from(text))
    }

    pub fn as_int(&self) -> i128 {
        match self {
            Value::Int(value) => *value,
            other => panic!("expected an integer, found {other:?}"),
        }
    }

    pub fn as_bool(&self) -> bool {
        match self {
            Value::Bool(value) => *value,
            other => panic!("expected a boolean, found {other:?}"),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Value::Str(text) => text,
            other => panic!("expected a string, found {other:?}"),
        }
    }
}

/// An insertion-ordered dictionary. Its keys are values of a type the
/// checker accepts as a key: integers, booleans, strings, `nothing`, and
/// structures, enums, and unions made only of those.
#[derive(Clone, Debug, Default)]
pub struct Dict {
    pub keys: Vec<Value>,
    pub values: Vec<Value>,
    index: HashMap<Key, usize>,
}

/// A key compared and hashed by content.
#[derive(Clone, Debug)]
struct Key(Value);

impl PartialEq for Key {
    fn eq(&self, other: &Key) -> bool {
        values_equal(&self.0, &other.0)
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_value(&self.0, state);
    }
}

fn hash_value(value: &Value, state: &mut impl Hasher) {
    match value {
        Value::Int(number) => number.hash(state),
        Value::Bool(flag) => flag.hash(state),
        Value::Nothing => 0u8.hash(state),
        Value::Str(text) => text.hash(state),
        Value::Union(tag, payload) => {
            tag.hash(state);
            hash_value(payload, state);
        }
        Value::Record(_, fields) => fields.iter().for_each(|field| hash_value(field, state)),
        Value::Variant(_, variant, fields) => {
            variant.hash(state);
            fields.iter().for_each(|field| hash_value(field, state));
        }
        other => panic!("{other:?} cannot be a dictionary key"),
    }
}

impl Dict {
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn find(&self, key: &Value) -> Option<usize> {
        self.index.get(&Key(key.clone())).copied()
    }

    /// Removes a key's entry, keeping the others in order.
    pub fn remove(&mut self, key: &Value) -> Option<Value> {
        let position = self.index.remove(&Key(key.clone()))?;
        self.keys.remove(position);
        let value = self.values.remove(position);
        for later in self.index.values_mut() {
            if *later > position {
                *later -= 1;
            }
        }
        Some(value)
    }

    /// Replaces the value of an existing key in place, or appends a new entry.
    pub fn set(&mut self, key: Value, value: Value) {
        let key = Key(key);
        match self.index.get(&key) {
            Some(&position) => self.values[position] = value,
            None => {
                self.keys.push(key.0.clone());
                self.index.insert(key, self.keys.len() - 1);
                self.values.push(value);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ErrorValue {
    pub code: String,
    pub message: String,
    /// `file:line:column` of the operation that raised it.
    pub location: Option<String>,
}

// ----- display ---------------------------------------------------------------

pub use sayon_runtime::text::{format_f32, format_f64};

/// The text `tostring` and `print` produce. Strings nested inside collections
/// are quoted so that `["a", "b"]` displays like the literal that made it.
/// Structures and enum values display like the expressions that build them.
pub fn display(value: &Value, out: &mut String, nested: bool, types: &[TypeDef]) {
    let display =
        |value: &Value, out: &mut String, nested: bool| display(value, out, nested, types);
    match value {
        Value::Int(number) => {
            let _ = write!(out, "{number}");
        }
        Value::F64(number) => out.push_str(&format_f64(*number)),
        Value::F32(number) => out.push_str(&format_f32(*number)),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Nothing => out.push_str("nothing"),
        Value::Str(text) => {
            if nested {
                quote(text, out);
            } else {
                out.push_str(text);
            }
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                display(item, out, true);
            }
            out.push(']');
        }
        Value::Dict(dict) => {
            out.push('[');
            for (index, (key, value)) in dict.keys.iter().zip(&dict.values).enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                display(key, out, true);
                out.push_str(" = ");
                display(value, out, true);
            }
            out.push(']');
        }
        Value::Union(_, payload) => display(payload, out, nested),
        Value::Record(id, fields) => {
            let def = &types[id.0 as usize];
            out.push_str(&def.name);
            out.push('(');
            for (index, (field, value)) in def.fields().iter().zip(fields).enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&field.name);
                out.push_str(" = ");
                display(value, out, true);
            }
            out.push(')');
        }
        Value::Variant(id, variant, fields) => {
            out.push_str(&types[id.0 as usize].variants()[*variant as usize].name);
            if !fields.is_empty() {
                out.push('(');
                for (index, value) in fields.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    display(value, out, true);
                }
                out.push(')');
            }
        }
        Value::Error(error) => {
            if nested {
                out.push_str("Error(");
                quote(&error.message, out);
                out.push(')');
            } else {
                out.push_str(&error.message);
            }
        }
        Value::Range { start, end, step } => {
            if *step == 1 {
                let _ = write!(out, "range({start}, {end})");
            } else {
                let _ = write!(out, "range({start}, {end}, {step})");
            }
        }
        Value::Function(..) => out.push_str("function"),
        Value::Stream(_) => out.push_str("stream"),
        // A shared value and a cell show their value.
        Value::Shared(shared) => display(&shared.borrow(), out, nested),
        Value::Weak(_) => out.push_str("weak"),
        Value::Task(_) => out.push_str("task"),
        Value::Channel(_) => out.push_str("channel"),
        // The machine shows a view's elements before displaying it.
        Value::View(view) => {
            let _ = write!(out, "view of {} elements", view.length);
        }
    }
}

use sayon_runtime::text::quote;

pub fn to_display_string(value: &Value, types: &[TypeDef]) -> String {
    let mut out = String::new();
    display(value, &mut out, false, types);
    out
}

// ----- equality and ordering -------------------------------------------------------

pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::F64(x), Value::F64(y)) => x == y,
        (Value::F32(x), Value::F32(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Nothing, Value::Nothing) => true,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| values_equal(p, q))
        }
        (Value::Dict(x), Value::Dict(y)) => {
            x.len() == y.len()
                && x.keys
                    .iter()
                    .zip(&x.values)
                    .all(|(key, value)| match y.find(key) {
                        Some(position) => values_equal(value, &y.values[position]),
                        None => false,
                    })
        }
        (Value::Union(t1, p1), Value::Union(t2, p2)) => t1 == t2 && values_equal(p1, p2),
        (Value::Record(_, x), Value::Record(_, y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| values_equal(p, q))
        }
        (Value::Variant(_, v1, x), Value::Variant(_, v2, y)) => {
            v1 == v2 && x.len() == y.len() && x.iter().zip(y).all(|(p, q)| values_equal(p, q))
        }
        (Value::Error(x), Value::Error(y)) => x.code == y.code && x.message == y.message,
        (
            Value::Range {
                start: s1,
                end: e1,
                step: t1,
            },
            Value::Range {
                start: s2,
                end: e2,
                step: t2,
            },
        ) => s1 == s2 && e1 == e2 && t1 == t2,
        _ => false,
    }
}

/// Ordering for numbers and strings; `None` when unordered (NaN).
pub fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::F64(x), Value::F64(y)) => x.partial_cmp(y),
        (Value::F32(x), Value::F32(y)) => x.partial_cmp(y),
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collections_quote_nested_strings() {
        let array = Value::Array(vec![Value::Int(1), Value::str("text"), Value::Bool(true)]);
        assert_eq!(to_display_string(&array, &[]), "[1, \"text\", true]");
        let mut dict = Dict::default();
        dict.set(Value::str("name"), Value::str("Alice"));
        dict.set(Value::str("age"), Value::Int(30));
        assert_eq!(
            to_display_string(&Value::Dict(Box::new(dict)), &[]),
            "[\"name\" = \"Alice\", \"age\" = 30]"
        );
        assert_eq!(to_display_string(&Value::str("plain"), &[]), "plain");
    }

    #[test]
    fn dictionary_equality_ignores_order() {
        let mut a = Dict::default();
        a.set(Value::str("x"), Value::Int(1));
        a.set(Value::str("y"), Value::Int(2));
        let mut b = Dict::default();
        b.set(Value::str("y"), Value::Int(2));
        b.set(Value::str("x"), Value::Int(1));
        assert!(values_equal(
            &Value::Dict(Box::new(a)),
            &Value::Dict(Box::new(b))
        ));
    }

    #[test]
    fn replacing_a_key_keeps_its_position() {
        let mut dict = Dict::default();
        dict.set(Value::str("a"), Value::Int(1));
        dict.set(Value::str("b"), Value::Int(2));
        dict.set(Value::str("a"), Value::Int(3));
        assert_eq!(dict.keys[0].as_str(), "a");
        assert!(values_equal(&dict.values[0], &Value::Int(3)));
    }
}
