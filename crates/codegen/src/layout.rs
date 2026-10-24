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
//! | Dict, Error | `ptr` to a runtime object | P |
//! | union | `{ i32 tag, [N x i64] payload }`, payload at offset 8 | 8 + 8N |
//! | structure | `{ fields... }` in declaration order, naturally aligned | sum with padding |
//! | enum | `{ i32 tag, [N x i64] payload }`; a variant's fields are laid out like a structure from offset 8 | 8 + 8N |
//! | function value | `%sayon.func = { ptr code, ptr environment }`, environment at offset P | 2P |
//! | View, MutableView | `%sayon.view = { ptr first, i64 length }`: the first two fields of an array | 16 |
//!
//! P is the size of an address on the target: 8, or 4 on WebAssembly. A
//! 64-bit integer is aligned to 8 bytes everywhere, so a string's, an
//! array's, and a view's length stays at offset 8 whatever P is.
//!
//! Every union places its payload at offset 8 and uses program-wide tags, so
//! a reference to a union value is also a valid reference to any narrower
//! union holding the same member. An enum's tag is its variant's position
//! plus one. An all-zero value is "empty": dropping it does nothing, which is
//! how moved-from places are represented.
//!
//! A closure's environment is a heap block: a pointer to its drop function
//! at offset 0, a pointer to its clone function at offset 8, then the
//! environment structure from offset 16, on every target. A named
//! function's value has a null environment. The code
//! receives a pointer to the environment structure before its arguments.

use sayon_check::types::{
    Eightbyte, FloatType, IntType, NamedType, Type, TypeDef, Wrapper, c_layout, c_size_align,
    structure_fields, sysv_eightbytes, variant_fields, win64_in_register,
};

/// The bytes before the value in a shared value's block.
pub const SHARED_HEADER: u64 = sayon_runtime::ffi::SHARED_HEADER as u64;
use sayon_ir::ForeignFunction;
use sayon_target::Target;
use std::collections::HashMap;

pub struct Layouts {
    types: Vec<TypeDef>,
    target: &'static Target,
    names: HashMap<Type, String>,
    pub definitions: Vec<String>,
}

impl Layouts {
    pub fn new(types: Vec<TypeDef>, target: &'static Target) -> Layouts {
        Layouts {
            types,
            target,
            names: HashMap::new(),
            definitions: Vec::new(),
        }
    }

    /// The size of an address, P in the table above.
    pub fn pointer_size(&self) -> u64 {
        self.target.pointer_size
    }

    /// The offset of a function value's environment: after its code.
    pub fn environment_offset(&self) -> u64 {
        self.pointer_size()
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
            Type::Dict(..) | Type::Error | Type::Pointer | Type::CString => "ptr".to_string(),
            Type::Stream => "i64".to_string(),
            // A cell is laid out like its value.
            Type::Wrapper(Wrapper::Cell, inner) => self.llvm(inner),
            Type::Wrapper(..) => "ptr".to_string(),
            Type::Function(_) => "%sayon.func".to_string(),
            Type::View(..) => "%sayon.view".to_string(),
            Type::Union(_) => self.tagged(ty, "union"),
            Type::Named(named) if self.is_enum(named) => self.tagged(ty, "enum"),
            Type::Named(named) => {
                if let Some(name) = self.names.get(ty) {
                    return name.clone();
                }
                let mut fields: Vec<String> = structure_fields(&self.types, named)
                    .iter()
                    .map(|field| self.llvm(field))
                    .collect();
                if self.is_finally(named) {
                    fields.push("i8".to_string());
                }
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

    /// Whether a structure has a finally function, and so a byte after its
    /// fields that tells a value from an empty place: 1 from when the value
    /// is made, and 0 once it has moved away or been destroyed.
    pub fn is_finally(&self, named: &NamedType) -> bool {
        self.types[named.id.0 as usize].finally
    }

    /// The offset of that byte.
    pub fn live_offset(&self, named: &NamedType) -> u64 {
        self.record_layout(&structure_fields(&self.types, named)).1
    }

    /// The types a structure's memory holds: its fields, and that byte.
    fn stored_fields(&self, named: &NamedType) -> Vec<Type> {
        let mut fields = structure_fields(&self.types, named);
        if self.is_finally(named) {
            fields.push(Type::Int(IntType::U8));
        }
        fields
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
            Type::Dict(..) | Type::Error | Type::Pointer | Type::CString => self.pointer_size(),
            Type::Stream => 8,
            Type::Wrapper(Wrapper::Cell, inner) => self.size_of(inner),
            Type::Wrapper(..) => self.pointer_size(),
            Type::Function(_) => 2 * self.pointer_size(),
            Type::View(..) => 16,
            Type::Union(_) => 8 + 8 * self.payload_words(ty),
            Type::Named(named) if self.is_enum(named) => 8 + 8 * self.payload_words(ty),
            Type::Named(named) => {
                let (_, size, align) = self.record_layout(&self.stored_fields(named));
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
                self.record_layout(&self.stored_fields(named)).2
            }
            Type::Wrapper(Wrapper::Cell, inner) => self.align_of(inner),
            Type::Dict(..)
            | Type::Error
            | Type::Pointer
            | Type::CString
            | Type::Wrapper(..)
            | Type::Function(_) => self.pointer_size(),
            // Each of the others holds a 64-bit integer.
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

/// The LLVM type a C function takes or gives a value of type `ty` as: a C
/// string is the address of its text.
pub fn c_llvm(ty: &Type) -> &'static str {
    match ty {
        Type::Int(int) => int_llvm(*int),
        Type::Float(FloatType::F64) => "double",
        Type::Float(FloatType::F32) => "float",
        // A function C calls back goes as the address of its C entry.
        Type::Pointer | Type::CString | Type::Function(_) => "ptr",
        Type::Nothing => "void",
        _ => "i64",
    }
}

/// How a C function takes or gives a value, under the target's convention.
pub enum CPass {
    /// As the LLVM value of its C type.
    Value,
    /// Windows x64: a structure of 1, 2, 4, or 8 bytes, in a register as an
    /// integer of this many bits.
    Register(u64),
    /// Windows x64 and WebAssembly: a structure by the address of a copy.
    /// For a result, on every convention: by the address of where C writes
    /// it.
    Address,
    /// System V: a structure as one value for each of its eightbytes, in a
    /// register of the kind it needs: its offset and LLVM type. WebAssembly:
    /// a structure of one number as that number.
    Pieces(Vec<(u64, String)>),
    /// System V: a structure copied onto the stack.
    Stack,
}

/// How a C function is called: its result, each parameter, and the LLVM
/// declaration's result type and parameters (each a type and its
/// attributes).
pub struct CPlan {
    pub ret: CPass,
    pub ret_llvm: String,
    pub params: Vec<CPass>,
    pub declared: Vec<(String, String)>,
    pub variadic: bool,
}

impl CPlan {
    /// The parameters of the declaration, with their attributes.
    pub fn declaration(&self) -> Vec<String> {
        let mut params: Vec<String> = self
            .declared
            .iter()
            .map(|(ty, attributes)| {
                if attributes.is_empty() {
                    ty.clone()
                } else {
                    format!("{ty} {attributes}")
                }
            })
            .collect();
        if self.variadic {
            params.push("...".to_string());
        }
        params
    }

    /// The function's type, for a call: the parameters' types alone.
    pub fn function_type(&self) -> String {
        let mut params: Vec<&str> = self.declared.iter().map(|(ty, _)| ty.as_str()).collect();
        if self.variadic {
            params.push("...");
        }
        format!("{} ({})", self.ret_llvm, params.join(", "))
    }
}

/// Whether the target passes C structures as System V does on x86-64.
fn system_v(target: &Target) -> bool {
    target.processor == "x64" && target.platform != "windows"
}

/// Whether compiled programs know how the target passes C structures: the
/// Windows x64, System V x86-64, and basic WebAssembly conventions so far.
pub fn structures_supported(target: &Target) -> bool {
    target.processor == "x64" || target.processor == "wasm32"
}

/// The one number or address a structure holds, with nothing else and no
/// padding, through any structures it is nested in. The basic C convention
/// of WebAssembly passes and gives such a structure as that value, and any
/// other structure by address.
fn single_scalar(types: &[TypeDef], ty: &Type, target: &Target) -> Option<Type> {
    let Type::Named(named) = ty else {
        return Some(ty.clone());
    };
    let layout = c_layout(types, named, target);
    let [(_, field)] = layout.fields.as_slice() else {
        return None;
    };
    let scalar = single_scalar(types, field, target)?;
    (c_size_align(types, &scalar, target).0 == layout.size).then_some(scalar)
}

/// How the target's convention calls a C function. System V puts a
/// structure's eightbytes in registers only when registers are left for
/// all of them, which depends on the parameters before it.
pub fn c_plan(types: &[TypeDef], function: &ForeignFunction, target: &Target) -> CPlan {
    let system_v = system_v(target);
    let wasm = target.processor == "wasm32";
    let mut declared = Vec::new();
    let mut integers = 0;
    let mut vectors = 0;
    let (ret, ret_llvm) = match &function.ret {
        Type::Named(_) => {
            let pass = if wasm {
                match single_scalar(types, &function.ret, target) {
                    Some(scalar) => CPass::Pieces(vec![(0, c_llvm(&scalar).to_string())]),
                    None => CPass::Address,
                }
            } else if system_v {
                match sysv_eightbytes(types, &function.ret, target) {
                    Some(classes) => {
                        CPass::Pieces(eightbyte_types(types, &function.ret, &classes, target))
                    }
                    None => CPass::Address,
                }
            } else {
                let (size, _) = c_size_align(types, &function.ret, target);
                if win64_in_register(size) {
                    CPass::Register(size * 8)
                } else {
                    CPass::Address
                }
            };
            let llvm = match &pass {
                CPass::Register(bits) => format!("i{bits}"),
                CPass::Pieces(pieces) if pieces.len() == 1 => pieces[0].1.clone(),
                CPass::Pieces(pieces) => {
                    let types: Vec<&str> = pieces.iter().map(|(_, ty)| ty.as_str()).collect();
                    format!("{{ {} }}", types.join(", "))
                }
                _ => {
                    let (size, align) = c_size_align(types, &function.ret, target);
                    declared.push((
                        "ptr".to_string(),
                        format!("sret([{size} x i8]) align {align}"),
                    ));
                    integers += 1;
                    "void".to_string()
                }
            };
            (pass, llvm)
        }
        other => (CPass::Value, c_llvm(other).to_string()),
    };
    let mut params = Vec::new();
    for param in &function.params {
        let pass = match param {
            // The address of a copy is what C's `byval` comes to there.
            Type::Named(_) if wasm => match single_scalar(types, param, target) {
                Some(scalar) => {
                    let llvm = c_llvm(&scalar).to_string();
                    declared.push((llvm.clone(), String::new()));
                    CPass::Pieces(vec![(0, llvm)])
                }
                None => {
                    declared.push(("ptr".to_string(), String::new()));
                    CPass::Address
                }
            },
            Type::Named(_) if system_v => {
                let (size, align) = c_size_align(types, param, target);
                let fits = |classes: &[Eightbyte]| {
                    let needed = |kind| classes.iter().filter(|c| **c == kind).count();
                    integers + needed(Eightbyte::Integer) <= 6
                        && vectors + needed(Eightbyte::Sse) <= 8
                };
                match sysv_eightbytes(types, param, target) {
                    Some(classes) if fits(&classes) => {
                        let pieces = eightbyte_types(types, param, &classes, target);
                        for ((_, ty), class) in pieces.iter().zip(&classes) {
                            declared.push((ty.clone(), String::new()));
                            match class {
                                Eightbyte::Integer => integers += 1,
                                Eightbyte::Sse => vectors += 1,
                            }
                        }
                        CPass::Pieces(pieces)
                    }
                    _ => {
                        declared.push((
                            "ptr".to_string(),
                            format!("byval([{size} x i8]) align {}", align.max(8)),
                        ));
                        CPass::Stack
                    }
                }
            }
            Type::Named(_) => {
                let (size, _) = c_size_align(types, param, target);
                if win64_in_register(size) {
                    declared.push((format!("i{}", size * 8), String::new()));
                    CPass::Register(size * 8)
                } else {
                    declared.push(("ptr".to_string(), String::new()));
                    CPass::Address
                }
            }
            other => {
                match other {
                    Type::Float(_) => vectors += 1,
                    _ => integers += 1,
                }
                let full = c_param(other);
                let (ty, attributes) = match full.split_once(' ') {
                    Some((ty, attributes)) => (ty.to_string(), attributes.to_string()),
                    None => (full, String::new()),
                };
                declared.push((ty, attributes));
                CPass::Value
            }
        };
        params.push(pass);
    }
    CPlan {
        ret,
        ret_llvm,
        params,
        declared,
        variadic: function.variadic,
    }
}

/// The LLVM type of each eightbyte of a structure, with its offset: an
/// integer of the eightbyte's bytes, or the float, two floats, or double
/// it holds.
fn eightbyte_types(
    types: &[TypeDef],
    ty: &Type,
    classes: &[Eightbyte],
    target: &Target,
) -> Vec<(u64, String)> {
    fn scalars(
        types: &[TypeDef],
        ty: &Type,
        offset: u64,
        out: &mut Vec<(u64, Type)>,
        target: &Target,
    ) {
        match ty {
            Type::Named(named) => {
                for (field_offset, field) in c_layout(types, named, target).fields {
                    scalars(types, &field, offset + field_offset, out, target);
                }
            }
            other => out.push((offset, other.clone())),
        }
    }
    let (size, _) = c_size_align(types, ty, target);
    let mut fields = Vec::new();
    scalars(types, ty, 0, &mut fields, target);
    classes
        .iter()
        .enumerate()
        .map(|(index, class)| {
            let start = index as u64 * 8;
            let llvm = match class {
                Eightbyte::Integer => format!("i{}", 8 * (size - start).min(8)),
                Eightbyte::Sse => {
                    let floats: Vec<&Type> = fields
                        .iter()
                        .filter(|(offset, _)| *offset >= start && *offset < start + 8)
                        .map(|(_, ty)| ty)
                        .collect();
                    match floats.as_slice() {
                        [Type::Float(FloatType::F32)] => "float".to_string(),
                        [Type::Float(FloatType::F32), Type::Float(FloatType::F32)] => {
                            "<2 x float>".to_string()
                        }
                        _ => "double".to_string(),
                    }
                }
            };
            (start, llvm)
        })
        .collect()
}

/// Whether a C function takes or gives a structure, which compiled programs
/// pass only on the targets whose conventions they know.
pub fn passes_structures(function: &ForeignFunction) -> bool {
    std::iter::once(&function.ret)
        .chain(&function.params)
        .any(|ty| matches!(ty, Type::Named(_)))
}

/// A C parameter's type with the extension C expects for a narrow integer.
pub fn c_param(ty: &Type) -> String {
    match ty {
        Type::Int(int) if int.bits() < 32 => {
            let extension = if int.signed() { "signext" } else { "zeroext" };
            format!("{} {extension}", int_llvm(*int))
        }
        other => c_llvm(other).to_string(),
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
        Type::String
        | Type::Array(_)
        | Type::Dict(..)
        | Type::Error
        | Type::Function(_)
        | Type::Stream
        | Type::CString
        | Type::Wrapper(
            Wrapper::Shared | Wrapper::Weak | Wrapper::Task | Wrapper::Channel | Wrapper::Mutex,
            _,
        ) => true,
        Type::Wrapper(Wrapper::Cell, inner) => needs_drop(inner),
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

    fn host_layouts(types: Vec<TypeDef>) -> Layouts {
        Layouts::new(types, sayon_target::find("windows-x64").unwrap())
    }

    #[test]
    fn union_sizes_cover_their_largest_member() {
        let small = Type::union([Type::INTEGER, Type::Nothing]);
        let layouts = host_layouts(Vec::new());
        assert_eq!(layouts.size_of(&small), 16);
        let large = Type::union([Type::String, Type::Nothing]);
        assert_eq!(layouts.size_of(&large), 32);
        let mut layouts = host_layouts(Vec::new());
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
                foreign: false,
                finally: false,
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
                foreign: false,
                finally: false,
            },
        ];
        let mut layouts = host_layouts(types);
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
    fn addresses_are_four_bytes_on_webassembly() {
        let wasm = Layouts::new(Vec::new(), sayon_target::find("wasi").unwrap());
        let function = Type::function(sayon_check::types::FunctionType {
            params: Vec::new(),
            ret: Type::Nothing,
        });
        assert_eq!(wasm.size_of(&function), 8);
        assert_eq!(wasm.align_of(&function), 4);
        assert_eq!(wasm.environment_offset(), 4);
        assert_eq!(wasm.size_of(&Type::Error), 4);
        // Lengths are 64-bit integers, aligned to 8 bytes everywhere.
        assert_eq!(wasm.size_of(&Type::String), 24);
        let host = host_layouts(Vec::new());
        assert_eq!(host.size_of(&function), 16);
        assert_eq!(host.environment_offset(), 8);
    }

    #[test]
    fn drop_requirements() {
        assert!(needs_drop(&Type::String));
        assert!(!needs_drop(&Type::union([Type::INTEGER, Type::Nothing])));
        assert!(needs_drop(&Type::union([Type::String, Type::Nothing])));
        assert!(!needs_drop(&Type::Range));
    }
}
