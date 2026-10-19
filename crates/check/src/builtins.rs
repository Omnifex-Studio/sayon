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
    if host {
        match name {
            "typed_decode" => return Some(PreludeItem::Builtin(Builtin::TypedDecode)),
            "typed_encode" => return Some(PreludeItem::Builtin(Builtin::TypedEncode)),
            "task_spawn" => return Some(PreludeItem::Builtin(Builtin::TaskSpawn)),
            "task_spawn_parallel" => {
                return Some(PreludeItem::Builtin(Builtin::TaskSpawnParallel));
            }
            "task_join" => return Some(PreludeItem::Builtin(Builtin::TaskJoin)),
            "task_detach" => return Some(PreludeItem::Builtin(Builtin::TaskDetach)),
            "task_yield" => return Some(PreludeItem::Builtin(Builtin::TaskYield)),
            "task_sleep" => return Some(PreludeItem::Builtin(Builtin::TaskSleep)),
            "channel_new" => return Some(PreludeItem::Builtin(Builtin::ChannelNew)),
            "channel_send" => return Some(PreludeItem::Builtin(Builtin::ChannelSend)),
            "channel_send_result" => {
                return Some(PreludeItem::Builtin(Builtin::ChannelSendResult));
            }
            "channel_receive" => return Some(PreludeItem::Builtin(Builtin::ChannelReceive)),
            "channel_close" => return Some(PreludeItem::Builtin(Builtin::ChannelClose)),
            "channel_select" => return Some(PreludeItem::Builtin(Builtin::ChannelSelect)),
            "raw_read" => return Some(PreludeItem::Builtin(Builtin::RawRead)),
            "raw_write" => return Some(PreludeItem::Builtin(Builtin::RawWrite)),
            _ => {}
        }
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
        "share" => Builtin::Share,
        "cell" => Builtin::MakeCell,
        "mutex" => Builtin::MakeMutex,
        "haskey" => Builtin::HasKey,
        "readfile" => Builtin::ReadFile,
        "writefile" => Builtin::WriteFile,
        "clone" => Builtin::Clone,
        "error" => Builtin::Error,
        "raise" => Builtin::Raise,
        // A conversion, written like `integer32(x)`, that can fail.
        "c_string" => Builtin::Host(sayon_runtime::host::Host::CString),
        "move" => return Some(PreludeItem::Move),
        _ => {
            if let Some(int) = int_type_by_name(name) {
                return Some(PreludeItem::Conversion(Type::Int(int)));
            }
            return match name {
                "float" | "float64" | "c_double" => {
                    Some(PreludeItem::Conversion(Type::Float(FloatType::F64)))
                }
                "float32" | "c_float" => Some(PreludeItem::Conversion(Type::Float(FloatType::F32))),
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
        "Box" => ("explicit shared ownership", "M3"),

        "platform" => ("compile-time platform branches", "M4"),
        _ => return None,
    })
}
