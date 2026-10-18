//! The prelude: functions available in every module without an import.

use crate::hir::Builtin;
use crate::infer::int_type_by_name;
use crate::types::{FloatType, Type};

pub enum PreludeItem {
    Builtin(Builtin),
    /// `move(value)`: an explicit ownership transfer.
    Move,
    /// A checked numeric conversion such as `integer(x)` or `float32(x)`.
    Conversion(Type),
}

/// Looks up a prelude function. `host` admits the primitives that only
/// standard library modules may call.
pub fn lookup(name: &str, host: bool) -> Option<PreludeItem> {
    if host && let Some(primitive) = sayon_runtime::host::Host::lookup(name) {
        return Some(PreludeItem::Builtin(Builtin::Host(primitive)));
    }
    let builtin = match name {
        "print" => Builtin::Print,
        "length" => Builtin::Length,
        "bytelength" => Builtin::ByteLength,
        "typeof" => Builtin::TypeOf,
        "assert" => Builtin::Assert,
        "range" => Builtin::Range,
        "tonumber" => Builtin::ToNumber,
        "tostring" => Builtin::ToString,
        "push" => Builtin::Push,
        "pop" => Builtin::Pop,
        "remove" => Builtin::Remove,
        "swap" => Builtin::Swap,
        "view" => Builtin::View,
        "mutable_view" => Builtin::MutableView,
        "split_view" => Builtin::SplitView,
        "haskey" => Builtin::HasKey,
        "readfile" => Builtin::ReadFile,
        "writefile" => Builtin::WriteFile,
        "clone" => Builtin::Clone,
        "error" => Builtin::Error,
        "raise" => Builtin::Raise,
        "move" => return Some(PreludeItem::Move),
        _ => {
            if let Some(int) = int_type_by_name(name) {
                return Some(PreludeItem::Conversion(Type::Int(int)));
            }
            return match name {
                "float" | "float64" => Some(PreludeItem::Conversion(Type::Float(FloatType::F64))),
                "float32" => Some(PreludeItem::Conversion(Type::Float(FloatType::F32))),
                _ => None,
            };
        }
    };
    Some(PreludeItem::Builtin(builtin))
}

/// Names used by later milestones, reported with a targeted message instead of
/// "undefined name".
pub fn planned(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "Shared" | "Weak" | "Cell" | "Mutex" | "Box" => ("explicit shared ownership", "M3"),

        "platform" => ("compile-time platform branches", "M4"),
        "null_pointer" | "pointer" => ("foreign pointers", "M3"),
        _ => return None,
    })
}
