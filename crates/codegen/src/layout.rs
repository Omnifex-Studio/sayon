//! Native representations of Sayon types.
//!
//! | Type | LLVM type | Size |
//! |---|---|---|
//! | integer widths | `i8` to `i64` | 1 to 8 |
//! | float, float32 | `double`, `float` | 8, 4 |
//! | boolean | `i1` | 1 |
//! | nothing | `i8` (always 0) | 1 |
//! | string | `%sayon.str = { ptr, i64 len, i64 cap }` | 24 |
//! | Array<T> | `%sayon.array = { ptr, i64 len, i64 cap }` | 24 |
//! | Range | `%sayon.range = { i64, i64, i64 }` | 24 |
//! | Dict, Error | `ptr` to a runtime object | 8 |
//! | union | `{ i32 tag, [N x i64] payload }`, payload at offset 8 | 8 + 8N |
//!
//! Every union places its payload at offset 8 and uses program-wide tags, so
//! a reference to a union value is also a valid reference to any narrower
//! union holding the same member. An all-zero value is "empty": dropping it
//! does nothing, which is how moved-from places are represented.

use sayon_check::types::{FloatType, IntType, Type};
use std::collections::HashMap;

#[derive(Default)]
pub struct Layouts {
    unions: HashMap<Type, String>,
    pub definitions: Vec<String>,
}

impl Layouts {
    pub fn llvm(&mut self, ty: &Type) -> String {
        match ty {
            Type::Int(int) => int_llvm(*int).to_string(),
            Type::Float(FloatType::F64) => "double".to_string(),
            Type::Float(FloatType::F32) => "float".to_string(),
            Type::Boolean => "i1".to_string(),
            Type::Nothing | Type::Unknown => "i8".to_string(),
            Type::String => "%sayon.str".to_string(),
            Type::Array(_) => "%sayon.array".to_string(),
            Type::Range => "%sayon.range".to_string(),
            Type::Dict(..) | Type::Error => "ptr".to_string(),
            Type::Union(_) => {
                if let Some(name) = self.unions.get(ty) {
                    return name.clone();
                }
                let words = payload_words(ty);
                let name = format!("%sayon.union.{}", self.unions.len());
                self.definitions
                    .push(format!("{name} = type {{ i32, [{words} x i64] }}"));
                self.unions.insert(ty.clone(), name.clone());
                name
            }
            Type::Param(_) | Type::Var(_) => panic!("unresolved type {ty} reached code generation"),
        }
    }
}

pub fn int_llvm(int: IntType) -> &'static str {
    match int.bits() {
        8 => "i8",
        16 => "i16",
        32 => "i32",
        _ => "i64",
    }
}

/// Code passed to the runtime for an integer type; see
/// `sayon_runtime::text::INTEGER_TYPE_NAMES`.
pub fn int_code(int: IntType) -> u32 {
    match int {
        IntType::I8 => 0,
        IntType::I16 => 1,
        IntType::I32 => 2,
        IntType::I64 => 3,
        IntType::U8 => 4,
        IntType::U16 => 5,
        IntType::U32 => 6,
        IntType::U64 => 7,
    }
}

fn payload_words(ty: &Type) -> u64 {
    let largest = ty.union_members().iter().map(size_of).max().unwrap_or(1);
    largest.div_ceil(8).max(1)
}

/// Allocation size in bytes, matching LLVM's alloc size for the layout above.
pub fn size_of(ty: &Type) -> u64 {
    match ty {
        Type::Int(int) => int.bits() as u64 / 8,
        Type::Float(FloatType::F64) => 8,
        Type::Float(FloatType::F32) => 4,
        Type::Boolean | Type::Nothing | Type::Unknown => 1,
        Type::String | Type::Array(_) | Type::Range => 24,
        Type::Dict(..) | Type::Error => 8,
        Type::Union(_) => 8 + 8 * payload_words(ty),
        Type::Param(_) | Type::Var(_) => 8,
    }
}

/// Whether values of the type own resources that must be released.
pub fn needs_drop(ty: &Type) -> bool {
    match ty {
        Type::String | Type::Array(_) | Type::Dict(..) | Type::Error => true,
        Type::Union(members) => members.iter().any(needs_drop),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_sizes_cover_their_largest_member() {
        let small = Type::union([Type::INTEGER, Type::Nothing]);
        assert_eq!(size_of(&small), 16);
        let large = Type::union([Type::String, Type::Nothing]);
        assert_eq!(size_of(&large), 32);
        let mut layouts = Layouts::default();
        assert_eq!(layouts.llvm(&small), "%sayon.union.0");
        assert_eq!(layouts.llvm(&small), "%sayon.union.0");
        assert_eq!(
            layouts.definitions[0],
            "%sayon.union.0 = type { i32, [1 x i64] }"
        );
    }

    #[test]
    fn drop_requirements() {
        assert!(needs_drop(&Type::String));
        assert!(!needs_drop(&Type::union([Type::INTEGER, Type::Nothing])));
        assert!(needs_drop(&Type::union([Type::String, Type::Nothing])));
        assert!(!needs_drop(&Type::Range));
    }
}
