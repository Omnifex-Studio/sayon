//! Semantic types.

use sayon_syntax::ast::ParamMode;
use sayon_target::Target;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IntType {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
}

impl IntType {
    pub const ALL: [IntType; 8] = [
        IntType::I8,
        IntType::I16,
        IntType::I32,
        IntType::I64,
        IntType::U8,
        IntType::U16,
        IntType::U32,
        IntType::U64,
    ];

    pub fn signed(self) -> bool {
        matches!(
            self,
            IntType::I8 | IntType::I16 | IntType::I32 | IntType::I64
        )
    }

    pub fn bits(self) -> u32 {
        match self {
            IntType::I8 | IntType::U8 => 8,
            IntType::I16 | IntType::U16 => 16,
            IntType::I32 | IntType::U32 => 32,
            IntType::I64 | IntType::U64 => 64,
        }
    }

    pub fn min_value(self) -> i128 {
        if self.signed() {
            -(1i128 << (self.bits() - 1))
        } else {
            0
        }
    }

    pub fn max_value(self) -> i128 {
        if self.signed() {
            (1i128 << (self.bits() - 1)) - 1
        } else {
            (1i128 << self.bits()) - 1
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            IntType::I8 => "integer8",
            IntType::I16 => "integer16",
            IntType::I32 => "integer32",
            IntType::I64 => "integer",
            IntType::U8 => "unsigned8",
            IntType::U16 => "unsigned16",
            IntType::U32 => "unsigned32",
            IntType::U64 => "unsigned",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FloatType {
    F32,
    F64,
}

impl FloatType {
    pub fn name(self) -> &'static str {
        match self {
            FloatType::F32 => "float32",
            FloatType::F64 => "float",
        }
    }
}

/// An inference variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TyVar(pub u32);

/// A structure or enum declared in the program; an index into the program's
/// type definitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(pub u32);

/// The type of a function value: how it takes each argument, and what it
/// returns.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FunctionType {
    pub params: Vec<(ParamMode, Type)>,
    pub ret: Type,
}

/// A use of a declared structure or enum with its type arguments.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NamedType {
    pub id: TypeId,
    pub name: String,
    pub args: Vec<Type>,
    /// Whether the definition is Copy when its type arguments are: every
    /// field is Copy, treating the type parameters as Copy.
    pub copy: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Type {
    Int(IntType),
    Float(FloatType),
    Boolean,
    String,
    Range,
    Error,
    Array(Box<Type>),
    Dict(Box<Type>, Box<Type>),
    /// A normalized union: flat, deduplicated, sorted, at least two members.
    Union(Vec<Type>),
    /// A declared structure or enum.
    Named(Box<NamedType>),
    /// A function value: a named function or a closure with its captures.
    Function(Box<FunctionType>),
    /// A borrowed slice of an array's elements: `View<T>`, or with `true`,
    /// the exclusive `MutableView<T>`.
    View(Box<Type>, bool),
    /// An open file, `HostStream` in the standard library's code: a counted
    /// reference to the runtime's stream, closed when the last is dropped.
    Stream,
    /// A raw address for C code: `pointer`, which `null_pointer` is one of.
    Pointer,
    /// An owned NUL-terminated copy of a string for C code: `c_string`.
    CString,
    /// `Shared<T>`, `Weak<T>`, or `Cell<T>`.
    Wrapper(Wrapper, Box<Type>),
    /// A type parameter of a generic function.
    Param(u32),
    Var(TyVar),
    /// Produced after an error so that checking can continue quietly.
    Unknown,
    /// Declared last so that unions list `nothing` after every other member.
    Nothing,
}

/// The types that hold one value in a particular way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Wrapper {
    /// A counted handle to a value many owners share; it cannot change.
    Shared,
    /// A handle to a shared value that does not keep it alive.
    Weak,
    /// A value that can be replaced through a read-only path, such as
    /// inside a shared value. Nothing inside it can be borrowed.
    Cell,
    /// A task of `standard/threads`: its one owner joins it for its result.
    Task,
    /// A channel of `standard/threads`, a counted handle every task holding
    /// it sends and receives through.
    Channel,
    /// A value that tasks on several threads share, one at a time: a
    /// counted handle to the value and its lock.
    Mutex,
}

impl Wrapper {
    pub fn name(self) -> &'static str {
        match self {
            Wrapper::Shared => "Shared",
            Wrapper::Weak => "Weak",
            Wrapper::Cell => "Cell",
            Wrapper::Task => "Task",
            Wrapper::Channel => "Channel",
            Wrapper::Mutex => "Mutex",
        }
    }
}

impl Type {
    pub const INTEGER: Type = Type::Int(IntType::I64);

    pub fn wrapper(kind: Wrapper, inner: Type) -> Type {
        Type::Wrapper(kind, Box::new(inner))
    }

    /// Whether a copy of a value of this type is a counted handle that a
    /// copy retains, so that using the value again keeps it alive:
    /// `Shared`, `Weak`, and unions of them with Copy types.
    pub fn is_retained(&self) -> bool {
        match self {
            Type::Wrapper(
                Wrapper::Shared | Wrapper::Weak | Wrapper::Channel | Wrapper::Mutex,
                _,
            ) => true,
            Type::Union(members) => {
                members.iter().any(Type::is_retained)
                    && members.iter().all(|m| m.is_copy() || m.is_retained())
            }
            _ => false,
        }
    }

    /// Whether a value can still be used after it is assigned or passed on:
    /// a Copy value, which is copied, or a counted handle, which is retained.
    pub fn is_duplicable(&self) -> bool {
        self.is_copy() || self.is_retained()
    }
    pub const FLOAT: Type = Type::Float(FloatType::F64);

    pub fn array(element: Type) -> Type {
        Type::Array(Box::new(element))
    }

    pub fn dict(key: Type, value: Type) -> Type {
        Type::Dict(Box::new(key), Box::new(value))
    }

    /// Builds a normalized union from arbitrary members.
    pub fn union(members: impl IntoIterator<Item = Type>) -> Type {
        let mut flat = Vec::new();
        for member in members {
            match member {
                Type::Union(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => Type::Nothing,
            1 => flat.pop().unwrap(),
            _ => Type::Union(flat),
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Type::Int(_) | Type::Float(_))
    }

    pub fn named(named: NamedType) -> Type {
        Type::Named(Box::new(named))
    }

    pub fn has_vars(&self) -> bool {
        match self {
            Type::Var(_) => true,
            Type::Array(element) => element.has_vars(),
            Type::Dict(key, value) => key.has_vars() || value.has_vars(),
            Type::Union(members) => members.iter().any(Type::has_vars),
            Type::Named(named) => named.args.iter().any(Type::has_vars),
            Type::Function(function) => function.types().any(Type::has_vars),
            Type::View(element, _) | Type::Wrapper(_, element) => element.has_vars(),
            _ => false,
        }
    }

    pub fn has_params(&self) -> bool {
        match self {
            Type::Param(_) => true,
            Type::Array(element) => element.has_params(),
            Type::Dict(key, value) => key.has_params() || value.has_params(),
            Type::Union(members) => members.iter().any(Type::has_params),
            Type::Named(named) => named.args.iter().any(Type::has_params),
            Type::Function(function) => function.types().any(Type::has_params),
            Type::View(element, _) | Type::Wrapper(_, element) => element.has_params(),
            _ => false,
        }
    }

    /// Whether values of this type are duplicated on assignment rather than moved.
    /// Unknown for unresolved variables and generic parameters, which are
    /// treated as owned.
    pub fn is_copy(&self) -> bool {
        match self {
            Type::Int(_)
            | Type::Float(_)
            | Type::Boolean
            | Type::Nothing
            | Type::Range
            | Type::Pointer => true,
            Type::Union(members) => members.iter().all(Type::is_copy),
            Type::Named(named) => named.copy && named.args.iter().all(Type::is_copy),
            // A read-only view is copied; a mutable one is exclusive.
            Type::View(_, mutable) => !mutable,
            Type::Unknown => true,
            _ => false,
        }
    }

    /// Replaces generic parameters with the given arguments.
    pub fn substitute(&self, args: &[Type]) -> Type {
        match self {
            Type::Param(index) => args.get(*index as usize).cloned().unwrap_or(Type::Unknown),
            Type::Array(element) => Type::array(element.substitute(args)),
            Type::Dict(key, value) => Type::dict(key.substitute(args), value.substitute(args)),
            Type::Union(members) => Type::union(members.iter().map(|m| m.substitute(args))),
            Type::Named(named) => Type::named(NamedType {
                args: named.args.iter().map(|a| a.substitute(args)).collect(),
                ..(**named).clone()
            }),
            Type::Function(function) => Type::function(function.map(|t| t.substitute(args))),
            Type::View(element, mutable) => Type::view(element.substitute(args), *mutable),
            Type::Wrapper(kind, inner) => Type::wrapper(*kind, inner.substitute(args)),
            other => other.clone(),
        }
    }

    pub fn view(element: Type, mutable: bool) -> Type {
        Type::View(Box::new(element), mutable)
    }

    /// Whether a view appears inside this type, somewhere a view cannot be
    /// kept: in a collection, structure, enum, union, or function result.
    pub fn holds_view(&self) -> bool {
        fn inside(ty: &Type) -> bool {
            match ty {
                Type::View(..) => true,
                Type::Array(element) => inside(element),
                Type::Dict(key, value) => inside(key) || inside(value),
                Type::Union(members) => members.iter().any(inside),
                Type::Named(named) => named.args.iter().any(inside),
                Type::Wrapper(_, inner) => inside(inner),
                Type::Function(function) => {
                    inside(&function.ret) || function.params.iter().any(|(_, p)| p.holds_view())
                }
                _ => false,
            }
        }
        match self {
            Type::View(element, _) => inside(element),
            Type::Function(function) => {
                inside(&function.ret) || function.params.iter().any(|(_, p)| p.holds_view())
            }
            other => inside(other),
        }
    }

    pub fn function(function: FunctionType) -> Type {
        Type::Function(Box::new(function))
    }

    /// The structure or enum this type names, if any.
    pub fn as_named(&self) -> Option<&NamedType> {
        match self {
            Type::Named(named) => Some(named),
            _ => None,
        }
    }

    pub fn union_members(&self) -> &[Type] {
        match self {
            Type::Union(members) => members,
            other => std::slice::from_ref(other),
        }
    }

    /// Canonical name reported by `typeof`.
    pub fn type_name(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Int(int) => f.write_str(int.name()),
            Type::Float(float) => f.write_str(float.name()),
            Type::Boolean => f.write_str("boolean"),
            Type::String => f.write_str("string"),
            Type::Nothing => f.write_str("nothing"),
            Type::Range => f.write_str("Range"),
            Type::Stream => f.write_str("HostStream"),
            Type::Pointer => f.write_str("pointer"),
            Type::CString => f.write_str("c_string"),
            Type::Error => f.write_str("Error"),
            Type::Array(element) => write!(f, "Array<{element}>"),
            Type::Dict(key, value) => write!(f, "Dict<{key}, {value}>"),
            Type::Union(members) => {
                for (index, member) in members.iter().enumerate() {
                    if index > 0 {
                        f.write_str(" or ")?;
                    }
                    write!(f, "{member}")?;
                }
                Ok(())
            }
            Type::Named(named) => {
                f.write_str(&named.name)?;
                if !named.args.is_empty() {
                    f.write_str("<")?;
                    for (index, arg) in named.args.iter().enumerate() {
                        if index > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{arg}")?;
                    }
                    f.write_str(">")?;
                }
                Ok(())
            }
            Type::View(element, false) => write!(f, "View<{element}>"),
            Type::View(element, true) => write!(f, "MutableView<{element}>"),
            Type::Wrapper(kind, inner) => write!(f, "{}<{inner}>", kind.name()),
            Type::Function(function) => {
                f.write_str("function(")?;
                for (index, (mode, param)) in function.params.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    match mode {
                        ParamMode::Own => f.write_str("own ")?,
                        ParamMode::Update => f.write_str("update ")?,
                        ParamMode::Read => {}
                    }
                    write!(f, "{param}")?;
                }
                f.write_str(")")?;
                if function.ret != Type::Nothing {
                    write!(f, " -> {}", function.ret)?;
                }
                Ok(())
            }
            Type::Param(index) => {
                let letters = ["T", "U", "V", "W"];
                match letters.get(*index as usize) {
                    Some(letter) => f.write_str(letter),
                    None => write!(f, "T{index}"),
                }
            }
            Type::Var(_) => f.write_str("_"),
            Type::Unknown => f.write_str("?"),
        }
    }
}

impl FunctionType {
    /// The parameter types and the result type.
    pub fn types(&self) -> impl Iterator<Item = &Type> {
        self.params
            .iter()
            .map(|(_, ty)| ty)
            .chain(std::iter::once(&self.ret))
    }

    /// The same function type with `f` applied to every type in it.
    pub fn map(&self, mut f: impl FnMut(&Type) -> Type) -> FunctionType {
        FunctionType {
            params: self
                .params
                .iter()
                .map(|(mode, ty)| (*mode, f(ty)))
                .collect(),
            ret: f(&self.ret),
        }
    }
}

/// A declared structure or enum. Field types refer to the definition's own
/// type parameters as `Type::Param(0..generics)`.
#[derive(Clone, Debug)]
pub struct TypeDef {
    pub name: String,
    pub generics: u32,
    pub kind: TypeDefKind,
    /// Whether values are Copy when the type arguments are.
    pub copy: bool,
    /// A structure declared with `foreign type`, laid out as C lays it out.
    pub foreign: bool,
    /// A structure with a `finally function`, which runs when a value of it
    /// is destroyed: it is neither Copy nor cloned, and is destroyed whole.
    pub finally: bool,
}

#[derive(Clone, Debug)]
pub enum TypeDefKind {
    Structure(Vec<FieldDef>),
    Enum(Vec<VariantDef>),
}

#[derive(Clone, Debug)]
pub struct FieldDef {
    pub name: String,
    pub ty: Type,
}

#[derive(Clone, Debug)]
pub struct VariantDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
}

impl TypeDef {
    pub fn is_enum(&self) -> bool {
        matches!(self.kind, TypeDefKind::Enum(_))
    }

    /// The fields of a structure; empty for an enum.
    pub fn fields(&self) -> &[FieldDef] {
        match &self.kind {
            TypeDefKind::Structure(fields) => fields,
            TypeDefKind::Enum(_) => &[],
        }
    }

    /// The variants of an enum; empty for a structure.
    pub fn variants(&self) -> &[VariantDef] {
        match &self.kind {
            TypeDefKind::Enum(variants) => variants,
            TypeDefKind::Structure(_) => &[],
        }
    }

    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields().iter().position(|f| f.name == name)
    }

    pub fn variant_index(&self, name: &str) -> Option<usize> {
        self.variants().iter().position(|v| v.name == name)
    }
}

/// The field types of a structure value, with its type arguments applied.
pub fn structure_fields(types: &[TypeDef], named: &NamedType) -> Vec<Type> {
    types[named.id.0 as usize]
        .fields()
        .iter()
        .map(|field| field.ty.substitute(&named.args))
        .collect()
}

/// Where C puts the fields of a structure: each at the next multiple of its
/// alignment, in order, and the whole rounded up to the largest alignment.
pub struct CLayout {
    pub fields: Vec<(u64, Type)>,
    pub size: u64,
    pub align: u64,
}

/// The C layout on `target` of a structure declared with `foreign type`.
pub fn c_layout(types: &[TypeDef], named: &NamedType, target: &Target) -> CLayout {
    let mut fields = Vec::new();
    let mut offset = 0;
    let mut align = 1;
    for field in structure_fields(types, named) {
        let (size, field_align) = c_size_align(types, &field, target);
        offset = u64::next_multiple_of(offset, field_align);
        fields.push((offset, field));
        offset += size;
        align = align.max(field_align);
    }
    CLayout {
        fields,
        size: u64::next_multiple_of(offset, align),
        align,
    }
}

/// Whether Windows x64 passes and returns a structure of this size in a
/// register, as an integer of its size; any other goes by its address.
pub fn win64_in_register(size: u64) -> bool {
    matches!(size, 1 | 2 | 4 | 8)
}

/// The kind of register an eightbyte of a structure goes in, under the
/// System V convention of x86-64 (Linux, macOS).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eightbyte {
    /// A general register: the eightbyte holds an integer or a pointer.
    Integer,
    /// A vector register: it holds only floats.
    Sse,
}

/// How the System V convention of x86-64 passes and returns a structure:
/// each of its eightbytes in a register of the kind it needs, or, for one
/// of more than 16 bytes, in memory (`None`).
pub fn sysv_eightbytes(types: &[TypeDef], ty: &Type, target: &Target) -> Option<Vec<Eightbyte>> {
    fn visit(
        types: &[TypeDef],
        ty: &Type,
        offset: u64,
        classes: &mut [Option<Eightbyte>],
        target: &Target,
    ) {
        let class = match ty {
            Type::Named(named) => {
                for (field_offset, field) in c_layout(types, named, target).fields {
                    visit(types, &field, offset + field_offset, classes, target);
                }
                return;
            }
            Type::Float(_) => Eightbyte::Sse,
            _ => Eightbyte::Integer,
        };
        let slot = &mut classes[(offset / 8) as usize];
        // An integer anywhere in an eightbyte makes it an integer one.
        *slot = match (*slot, class) {
            (Some(Eightbyte::Integer), _) | (_, Eightbyte::Integer) => Some(Eightbyte::Integer),
            _ => Some(Eightbyte::Sse),
        };
    }
    let (size, _) = c_size_align(types, ty, target);
    if size > 16 {
        return None;
    }
    let mut classes = vec![None; size.div_ceil(8) as usize];
    visit(types, ty, 0, &mut classes, target);
    Some(
        classes
            .into_iter()
            .map(|class| class.unwrap_or(Eightbyte::Integer))
            .collect(),
    )
}

/// The size and alignment C gives on `target` a value of a type C functions
/// take: a number, a pointer, or a `foreign type` structure.
pub fn c_size_align(types: &[TypeDef], ty: &Type, target: &Target) -> (u64, u64) {
    match ty {
        Type::Int(int) => {
            let bytes = int.bits() as u64 / 8;
            (bytes, bytes)
        }
        Type::Float(FloatType::F32) => (4, 4),
        Type::Float(FloatType::F64) => (8, 8),
        Type::Named(named) => {
            let layout = c_layout(types, named, target);
            (layout.size, layout.align)
        }
        // A pointer, a C string, or a function C calls back: an address.
        _ => (target.pointer_size, target.pointer_size),
    }
}

/// The part of a type that keeps its values from being dictionary keys, if
/// any. Keys are compared by value and must keep their identity: integers,
/// booleans, strings, `nothing`, and structures, enums, and unions of those.
/// Floats (NaN is unequal to itself), collections, function values, errors,
/// and ranges are not keys. Type parameters and unknown types are decided
/// where they are filled in.
pub fn non_key_part(types: &[TypeDef], ty: &Type) -> Option<Type> {
    fn search(types: &[TypeDef], ty: &Type, seen: &mut Vec<TypeId>) -> Option<Type> {
        match ty {
            Type::Int(_)
            | Type::Boolean
            | Type::String
            | Type::Nothing
            | Type::Unknown
            | Type::Param(_)
            | Type::Var(_) => None,
            Type::Union(members) => members.iter().find_map(|m| search(types, m, seen)),
            Type::Named(named) => {
                if seen.contains(&named.id) {
                    return None;
                }
                seen.push(named.id);
                let def = &types[named.id.0 as usize];
                let variant_fields = def.variants().iter().flat_map(|v| &v.fields);
                def.fields()
                    .iter()
                    .chain(variant_fields)
                    .find_map(|field| search(types, &field.ty.substitute(&named.args), seen))
            }
            Type::Float(_)
            | Type::Array(_)
            | Type::Dict(..)
            | Type::Function(_)
            | Type::View(..)
            | Type::Wrapper(..)
            | Type::Error
            | Type::Stream
            | Type::Pointer
            | Type::CString
            | Type::Range => Some(ty.clone()),
        }
    }
    search(types, ty, &mut Vec::new())
}

/// Whether a value of the type can contain a function value.
pub fn holds_function(types: &[TypeDef], ty: &Type) -> bool {
    fn search(types: &[TypeDef], ty: &Type, seen: &mut Vec<TypeId>) -> bool {
        match ty {
            Type::Function(_) => true,
            Type::Union(members) => members.iter().any(|m| search(types, m, seen)),
            Type::Array(element)
            | Type::Dict(_, element)
            | Type::View(element, _)
            | Type::Wrapper(_, element) => search(types, element, seen),
            Type::Named(named) => {
                if seen.contains(&named.id) {
                    return false;
                }
                seen.push(named.id);
                let def = &types[named.id.0 as usize];
                let variant_fields = def.variants().iter().flat_map(|v| &v.fields);
                def.fields()
                    .iter()
                    .chain(variant_fields)
                    .any(|field| search(types, &field.ty, seen))
                    || named.args.iter().any(|a| search(types, a, seen))
            }
            _ => false,
        }
    }
    search(types, ty, &mut Vec::new())
}

/// The field types of one variant of an enum value, with its type arguments
/// applied.
pub fn variant_fields(types: &[TypeDef], named: &NamedType, variant: u32) -> Vec<Type> {
    types[named.id.0 as usize].variants()[variant as usize]
        .fields
        .iter()
        .map(|field| field.ty.substitute(&named.args))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unions_are_normalized() {
        let nested = Type::union([
            Type::String,
            Type::union([Type::INTEGER, Type::Nothing]),
            Type::INTEGER,
        ]);
        assert_eq!(
            nested,
            Type::Union(vec![Type::INTEGER, Type::String, Type::Nothing])
        );
        assert_eq!(nested.to_string(), "integer or string or nothing");
        assert_eq!(Type::union([Type::Boolean, Type::Boolean]), Type::Boolean);
    }

    #[test]
    fn integer_ranges() {
        assert_eq!(IntType::I8.min_value(), -128);
        assert_eq!(IntType::I8.max_value(), 127);
        assert_eq!(IntType::U8.max_value(), 255);
        assert_eq!(IntType::I64.min_value(), i64::MIN as i128);
        assert_eq!(IntType::U64.max_value(), u64::MAX as i128);
    }

    #[test]
    fn copy_types() {
        assert!(Type::INTEGER.is_copy());
        assert!(Type::union([Type::INTEGER, Type::Nothing]).is_copy());
        assert!(!Type::union([Type::String, Type::Nothing]).is_copy());
        assert!(!Type::array(Type::INTEGER).is_copy());
        assert!(!Type::Param(0).is_copy());
    }
}
