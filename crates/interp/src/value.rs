//! Runtime values and their display, equality, and ordering rules.

use sayon_check::types::{TypeDef, TypeId};
use sayon_ir::FunctionId;
use std::collections::HashMap;
use std::fmt::Write as _;
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

/// An insertion-ordered dictionary with string keys.
#[derive(Clone, Debug, Default)]
pub struct Dict {
    pub keys: Vec<Rc<str>>,
    pub values: Vec<Value>,
    index: HashMap<Rc<str>, usize>,
}

impl Dict {
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn find(&self, key: &str) -> Option<usize> {
        self.index.get(key).copied()
    }

    /// Replaces the value of an existing key in place, or appends a new entry.
    pub fn set(&mut self, key: &str, value: Value) {
        match self.index.get(key) {
            Some(&position) => self.values[position] = value,
            None => {
                let key: Rc<str> = Rc::from(key);
                self.index.insert(key.clone(), self.keys.len());
                self.keys.push(key);
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
                quote(key, out);
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
        dict.set("name", Value::str("Alice"));
        dict.set("age", Value::Int(30));
        assert_eq!(
            to_display_string(&Value::Dict(Box::new(dict)), &[]),
            "[\"name\" = \"Alice\", \"age\" = 30]"
        );
        assert_eq!(to_display_string(&Value::str("plain"), &[]), "plain");
    }

    #[test]
    fn dictionary_equality_ignores_order() {
        let mut a = Dict::default();
        a.set("x", Value::Int(1));
        a.set("y", Value::Int(2));
        let mut b = Dict::default();
        b.set("y", Value::Int(2));
        b.set("x", Value::Int(1));
        assert!(values_equal(
            &Value::Dict(Box::new(a)),
            &Value::Dict(Box::new(b))
        ));
    }

    #[test]
    fn replacing_a_key_keeps_its_position() {
        let mut dict = Dict::default();
        dict.set("a", Value::Int(1));
        dict.set("b", Value::Int(2));
        dict.set("a", Value::Int(3));
        assert_eq!(&*dict.keys[0], "a");
        assert!(values_equal(&dict.values[0], &Value::Int(3)));
    }
}
