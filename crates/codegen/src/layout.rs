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
//! | structure | `{ fields... }` in declaration order, naturally aligned | sum with padding |
//! | enum | `{ i32 tag, [N x i64] payload }`; a variant's fields are laid out like a structure from offset 8 | 8 + 8N |
//! | function value | `%sayon.func = { ptr code, ptr environment }` | 16 |
//!
//! Every union places its payload at offset 8 and uses program-wide tags, so
//! a reference to a union value is also a valid reference to any narrower
//! union holding the same member. An enum's tag is its variant's position
//! plus one. An all-zero value is "empty": dropping it does nothing, which is
//! how moved-from places are represented.
//!
//! A closure's environment is a heap block: a pointer to its drop function,
//! a pointer to its clone function, then the environment structure from
//! offset 16. A named function's value has a null environment. The code
//! receives a pointer to the environment structure before its arguments.

use sayon_check::types::{
    FloatType, IntType, NamedType, Type, TypeDef, structure_fields, variant_fields,
};
use std::collections::HashMap;

#[derive(Default)]
pub struct Layouts {
    types: Vec<TypeDef>,
    names: HashMap<Type, String>,
    pub definitions: Vec<String>,
}

impl Layouts {
    pub fn new(types: Vec<TypeDef>) -> Layouts {
        Layouts {
            types,
            ..Layouts::default()
        }
    }

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
            Type::Function(_) => "%sayon.func".to_string(),
            Type::Union(_) => self.tagged(ty, "union"),
            Type::Named(named) if self.is_enum(named) => self.tagged(ty, "enum"),
            Type::Named(named) => {
                if let Some(name) = self.names.get(ty) {
                    return name.clone();
                }
                let fields: Vec<String> = structure_fields(&self.types, named)
                    .iter()
                    .map(|field| self.llvm(field))
                    .collect();
                let name = format!("%sayon.record.{}", self.names.len());
                self.definitions
                    .push(format!("{name} = type {{ {} }}", fields.join(", ")));
                self.names.insert(ty.clone(), name.clone());
                name
            }
            Type::Param(_) | Type::Var(_) => panic!("unresolved type {ty} reached code generation"),
        }
    }

    /// `{ i32, [N x i64] }` for a union or an enum.
    fn tagged(&mut self, ty: &Type, kind: &str) -> String {
        if let Some(name) = self.names.get(ty) {
            return name.clone();
        }
        let words = self.payload_words(ty);
        let name = format!("%sayon.{kind}.{}", self.names.len());
        self.definitions
            .push(format!("{name} = type {{ i32, [{words} x i64] }}"));
        self.names.insert(ty.clone(), name.clone());
        name
    }

    pub fn is_enum(&self, named: &NamedType) -> bool {
        self.types[named.id.0 as usize].is_enum()
    }

    fn payload_words(&self, ty: &Type) -> u64 {
        let largest = match ty {
            Type::Named(named) => (0..self.types[named.id.0 as usize].variants().len())
                .map(|v| {
                    self.record_layout(&variant_fields(&self.types, named, v as u32))
                        .1
                })
                .max()
                .unwrap_or(0),
            _ => ty
                .union_members()
                .iter()
                .map(|m| self.size_of(m))
                .max()
                .unwrap_or(0),
        };
        largest.div_ceil(8).max(1)
    }

    /// Allocation size in bytes, matching LLVM's alloc size for the layout above.
    pub fn size_of(&self, ty: &Type) -> u64 {
        match ty {
            Type::Int(int) => int.bits() as u64 / 8,
            Type::Float(FloatType::F64) => 8,
            Type::Float(FloatType::F32) => 4,
            Type::Boolean | Type::Nothing | Type::Unknown => 1,
            Type::String | Type::Array(_) | Type::Range => 24,
            Type::Dict(..) | Type::Error => 8,
            Type::Function(_) => 16,
            Type::Union(_) => 8 + 8 * self.payload_words(ty),
            Type::Named(named) if self.is_enum(named) => 8 + 8 * self.payload_words(ty),
            Type::Named(named) => {
                let (_, size, align) = self.record_layout(&structure_fields(&self.types, named));
                size.next_multiple_of(align)
            }
            Type::Param(_) | Type::Var(_) => 8,
        }
    }

    /// ABI alignment in bytes.
    pub fn align_of(&self, ty: &Type) -> u64 {
        match ty {
            Type::Int(int) => int.bits() as u64 / 8,
            Type::Float(FloatType::F64) => 8,
            Type::Float(FloatType::F32) => 4,
            Type::Boolean | Type::Nothing | Type::Unknown => 1,
            Type::Named(named) if !self.is_enum(named) => {
                self.record_layout(&structure_fields(&self.types, named)).2
            }
            _ => 8,
        }
    }

    /// Field offsets, the end of the last field, and the alignment of a
    /// structure with these field types, following LLVM's struct layout.
    fn record_layout(&self, fields: &[Type]) -> (Vec<u64>, u64, u64) {
        let mut offsets = Vec::with_capacity(fields.len());
        let mut offset: u64 = 0;
        let mut align: u64 = 1;
        for field in fields {
            let field_align = self.align_of(field);
            align = align.max(field_align);
            offset = offset.next_multiple_of(field_align);
            offsets.push(offset);
            offset += self.size_of(field);
        }
        (offsets, offset, align)
    }

    /// Byte offset of a structure's field.
    pub fn field_offset(&self, named: &NamedType, field: u32) -> u64 {
        self.record_layout(&structure_fields(&self.types, named)).0[field as usize]
    }

    /// Byte offset of a variant's field within an enum value.
    pub fn variant_field_offset(&self, named: &NamedType, variant: u32, field: u32) -> u64 {
        8 + self
            .record_layout(&variant_fields(&self.types, named, variant))
            .0[field as usize]
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

/// Whether values of the type own resources that must be released.
pub fn needs_drop(ty: &Type) -> bool {
    match ty {
        Type::String | Type::Array(_) | Type::Dict(..) | Type::Error | Type::Function(_) => true,
        Type::Union(members) => members.iter().any(needs_drop),
        // Every non-Copy field owns something.
        Type::Named(_) => !ty.is_copy(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sayon_check::types::{FieldDef, TypeDefKind, TypeId, VariantDef};

    fn field(name: &str, ty: Type) -> FieldDef {
        FieldDef {
            name: name.to_string(),
            ty,
        }
    }

    fn named(id: u32, copy: bool) -> Type {
        Type::named(NamedType {
            id: TypeId(id),
            name: format!("T{id}"),
            args: Vec::new(),
            copy,
        })
    }

    #[test]
    fn union_sizes_cover_their_largest_member() {
        let small = Type::union([Type::INTEGER, Type::Nothing]);
        let layouts = Layouts::default();
        assert_eq!(layouts.size_of(&small), 16);
        let large = Type::union([Type::String, Type::Nothing]);
        assert_eq!(layouts.size_of(&large), 32);
        let mut layouts = Layouts::default();
        assert_eq!(layouts.llvm(&small), "%sayon.union.0");
        assert_eq!(layouts.llvm(&small), "%sayon.union.0");
        assert_eq!(
            layouts.definitions[0],
            "%sayon.union.0 = type { i32, [1 x i64] }"
        );
    }

    #[test]
    fn structures_and_enums_are_naturally_aligned() {
        let types = vec![
            TypeDef {
                name: "Mixed".into(),
                generics: 0,
                kind: TypeDefKind::Structure(vec![
                    field("flag", Type::Boolean),
                    field("count", Type::INTEGER),
                    field("small", Type::Int(IntType::I16)),
                ]),
                copy: true,
            },
            TypeDef {
                name: "Shape".into(),
                generics: 0,
                kind: TypeDefKind::Enum(vec![
                    VariantDef {
                        name: "point".into(),
                        fields: Vec::new(),
                    },
                    VariantDef {
                        name: "labelled".into(),
                        fields: vec![field("ok", Type::Boolean), field("text", Type::String)],
                    },
                ]),
                copy: false,
            },
        ];
        let mut layouts = Layouts::new(types);
        let mixed = named(0, true);
        let Type::Named(mixed_named) = &mixed else {
            panic!()
        };
        assert_eq!(layouts.size_of(&mixed), 24);
        assert_eq!(layouts.align_of(&mixed), 8);
        assert_eq!(layouts.field_offset(mixed_named, 1), 8);
        assert_eq!(layouts.field_offset(mixed_named, 2), 16);
        assert_eq!(layouts.llvm(&mixed), "%sayon.record.0");
        assert_eq!(
            layouts.definitions[0],
            "%sayon.record.0 = type { i1, i64, i16 }"
        );

        let shape = named(1, false);
        let Type::Named(shape_named) = &shape else {
            panic!()
        };
        // tag, then { i1, %sayon.str } padded to 32 bytes
        assert_eq!(layouts.size_of(&shape), 40);
        assert_eq!(layouts.variant_field_offset(shape_named, 1, 1), 16);
        assert!(needs_drop(&shape));
        assert!(!needs_drop(&mixed));
    }

    #[test]
    fn drop_requirements() {
        assert!(needs_drop(&Type::String));
        assert!(!needs_drop(&Type::union([Type::INTEGER, Type::Nothing])));
        assert!(needs_drop(&Type::union([Type::String, Type::Nothing])));
        assert!(!needs_drop(&Type::Range));
    }
}
