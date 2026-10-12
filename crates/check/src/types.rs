//! Semantic types.

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
    /// A type parameter of a generic function.
    Param(u32),
    Var(TyVar),
    /// Produced after an error so that checking can continue quietly.
    Unknown,
    /// Declared last so that unions list `nothing` after every other member.
    Nothing,
}

impl Type {
    pub const INTEGER: Type = Type::Int(IntType::I64);
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

    pub fn has_vars(&self) -> bool {
        match self {
            Type::Var(_) => true,
            Type::Array(element) => element.has_vars(),
            Type::Dict(key, value) => key.has_vars() || value.has_vars(),
            Type::Union(members) => members.iter().any(Type::has_vars),
            _ => false,
        }
    }

    pub fn has_params(&self) -> bool {
        match self {
            Type::Param(_) => true,
            Type::Array(element) => element.has_params(),
            Type::Dict(key, value) => key.has_params() || value.has_params(),
            Type::Union(members) => members.iter().any(Type::has_params),
            _ => false,
        }
    }

    /// Whether values of this type are duplicated on assignment rather than moved.
    /// Unknown for unresolved variables and generic parameters, which are
    /// treated as owned.
    pub fn is_copy(&self) -> bool {
        match self {
            Type::Int(_) | Type::Float(_) | Type::Boolean | Type::Nothing | Type::Range => true,
            Type::Union(members) => members.iter().all(Type::is_copy),
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
            other => other.clone(),
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
