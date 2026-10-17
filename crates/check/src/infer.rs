//! Unification with bounded inference variables.
//!
//! Variables can carry a literal kind (an unsuffixed integer literal may become
//! any integer type, a `u` literal any unsigned type, a float literal any float
//! type) and a set of operation traits required by the code that uses them.
//! Remaining literal kinds are defaulted; remaining variables in a function
//! signature are generalized together with their bounds.

use crate::types::{FloatType, IntType, NamedType, TyVar, Type};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiteralKind {
    Integer,
    Unsigned,
    Float,
}

impl LiteralKind {
    pub fn default_type(self) -> Type {
        match self {
            LiteralKind::Integer => Type::INTEGER,
            LiteralKind::Unsigned => Type::Int(IntType::U64),
            LiteralKind::Float => Type::FLOAT,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            LiteralKind::Integer => "an integer literal",
            LiteralKind::Unsigned => "an unsigned literal",
            LiteralKind::Float => "a float literal",
        }
    }

    fn accepts(self, ty: &Type) -> bool {
        match (self, ty) {
            (LiteralKind::Integer, Type::Int(_)) => true,
            (LiteralKind::Unsigned, Type::Int(int)) => !int.signed(),
            (LiteralKind::Float, Type::Float(_)) => true,
            _ => false,
        }
    }

    /// Combines two literal kinds that must describe the same type.
    fn merge(self, other: LiteralKind) -> Option<LiteralKind> {
        use LiteralKind::*;
        match (self, other) {
            (a, b) if a == b => Some(a),
            (Integer, Unsigned) | (Unsigned, Integer) => Some(Unsigned),
            _ => None,
        }
    }
}

/// Operations a type must support.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Traits(u8);

impl Traits {
    pub const NONE: Traits = Traits(0);
    /// `+`: numbers and strings.
    pub const ADD: Traits = Traits(1);
    /// `-`, `*`, `/`, `%`: numbers.
    pub const ARITHMETIC: Traits = Traits(2);
    /// `<`, `>`, `<=`, `>=`: numbers and strings.
    pub const ORDER: Traits = Traits(4);
    /// `==`, `!=`.
    pub const EQUALITY: Traits = Traits(8);
    /// `length`: strings, arrays, dictionaries.
    pub const LENGTH: Traits = Traits(16);
    /// Unary `-`: signed integers and floats.
    pub const NEGATE: Traits = Traits(32);
    /// Numeric conversion targets and sources.
    pub const NUMERIC: Traits = Traits(64);

    pub fn contains(self, other: Traits) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn union(self, other: Traits) -> Traits {
        Traits(self.0 | other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn each(self) -> impl Iterator<Item = Traits> {
        (0..7)
            .map(|bit| Traits(1 << bit))
            .filter(move |t| self.contains(*t))
    }

    pub fn describe(self) -> &'static str {
        match self {
            Traits::ADD => "`+`",
            Traits::ARITHMETIC => "arithmetic",
            Traits::ORDER => "ordering comparison",
            Traits::EQUALITY => "equality comparison",
            Traits::LENGTH => "`length`",
            Traits::NEGATE => "negation",
            Traits::NUMERIC => "numeric conversion",
            _ => "these operations",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
    pub literal: Option<LiteralKind>,
    pub traits: Traits,
}

impl Bounds {
    pub fn literal(kind: LiteralKind) -> Bounds {
        Bounds {
            literal: Some(kind),
            traits: Traits::NONE,
        }
    }

    pub fn traits(traits: Traits) -> Bounds {
        Bounds {
            literal: None,
            traits,
        }
    }

    /// Whether only numbers satisfy these bounds, which makes every value
    /// of the type Copy.
    pub fn only_numbers(&self) -> bool {
        self.literal.is_some()
            || [Traits::ARITHMETIC, Traits::NEGATE, Traits::NUMERIC]
                .iter()
                .any(|t| self.traits.contains(*t))
    }
}

#[derive(Clone, Debug)]
pub enum UnifyError {
    Mismatch(Type, Type),
    Infinite,
    /// The type does not support an operation required of it.
    Unsupported {
        ty: Type,
        traits: Traits,
    },
    /// A literal cannot have this type.
    Literal {
        ty: Type,
        kind: LiteralKind,
    },
    LiteralConflict,
}

impl UnifyError {
    pub fn describe(&self) -> String {
        match self {
            UnifyError::Mismatch(a, b) => format!("expected {a}, found {b}"),
            UnifyError::Infinite => "this would create an infinitely nested type".to_string(),
            UnifyError::Unsupported { ty, traits } => {
                format!("{ty} does not support {}", traits.describe())
            }
            UnifyError::Literal { ty, kind } => {
                format!("{} cannot have type {ty}", kind.describe())
            }
            UnifyError::LiteralConflict => {
                "an integer and a float cannot be mixed without an explicit conversion".to_string()
            }
        }
    }
}

struct VarState {
    binding: Option<Type>,
    bounds: Bounds,
}

#[derive(Default)]
pub struct Unifier {
    vars: Vec<VarState>,
    /// Every field type of each declared structure or enum, in terms of its
    /// type parameters; used to decide which operations it supports.
    pub field_types: Vec<Vec<Type>>,
    /// Named types whose equality support is being decided, so that a type
    /// reaching itself through a collection does not recurse forever.
    in_progress: Vec<Type>,
}

impl Unifier {
    pub fn new() -> Unifier {
        Unifier::default()
    }

    pub fn var_count(&self) -> u32 {
        self.vars.len() as u32
    }

    pub fn fresh(&mut self) -> Type {
        self.fresh_bounded(Bounds::default())
    }

    pub fn fresh_bounded(&mut self, bounds: Bounds) -> Type {
        let var = TyVar(self.vars.len() as u32);
        self.vars.push(VarState {
            binding: None,
            bounds,
        });
        Type::Var(var)
    }

    pub fn bounds(&self, var: TyVar) -> Bounds {
        self.vars[var.0 as usize].bounds
    }

    /// Follows variable bindings at the top level only.
    pub fn shallow(&self, ty: &Type) -> Type {
        let mut current = ty.clone();
        while let Type::Var(var) = current {
            match &self.vars[var.0 as usize].binding {
                Some(bound) => current = bound.clone(),
                None => return Type::Var(var),
            }
        }
        current
    }

    /// Resolves every bound variable inside a type.
    pub fn resolve(&self, ty: &Type) -> Type {
        match self.shallow(ty) {
            Type::Array(element) => Type::array(self.resolve(&element)),
            Type::Dict(key, value) => Type::dict(self.resolve(&key), self.resolve(&value)),
            Type::Union(members) => Type::union(members.iter().map(|m| self.resolve(m))),
            Type::Named(named) => Type::named(NamedType {
                args: named.args.iter().map(|a| self.resolve(a)).collect(),
                ..*named
            }),
            Type::Function(function) => Type::function(function.map(|t| self.resolve(t))),
            other => other,
        }
    }

    /// Like `resolve`, but shows unresolved literal variables as the type they
    /// would default to, for diagnostics.
    pub fn resolve_for_display(&self, ty: &Type) -> Type {
        match self.shallow(ty) {
            Type::Var(var) => match self.vars[var.0 as usize].bounds.literal {
                Some(kind) => kind.default_type(),
                None => Type::Var(var),
            },
            Type::Array(element) => Type::array(self.resolve_for_display(&element)),
            Type::Dict(key, value) => Type::dict(
                self.resolve_for_display(&key),
                self.resolve_for_display(&value),
            ),
            Type::Union(members) => {
                Type::union(members.iter().map(|m| self.resolve_for_display(m)))
            }
            Type::Named(named) => Type::named(NamedType {
                args: named
                    .args
                    .iter()
                    .map(|a| self.resolve_for_display(a))
                    .collect(),
                ..*named
            }),
            Type::Function(function) => {
                Type::function(function.map(|t| self.resolve_for_display(t)))
            }
            other => other,
        }
    }

    /// Binds a variable without checks; used for defaulting and generalization.
    pub fn force_bind(&mut self, var: TyVar, ty: Type) {
        self.vars[var.0 as usize].binding = Some(ty);
    }

    pub fn add_bounds(&mut self, ty: &Type, bounds: Bounds) -> Result<(), UnifyError> {
        match self.shallow(ty) {
            Type::Var(var) => {
                let state = &mut self.vars[var.0 as usize];
                let literal = match (state.bounds.literal, bounds.literal) {
                    (Some(a), Some(b)) => Some(a.merge(b).ok_or(UnifyError::LiteralConflict)?),
                    (a, b) => a.or(b),
                };
                state.bounds = Bounds {
                    literal,
                    traits: state.bounds.traits.union(bounds.traits),
                };
                if let Some(literal) = literal {
                    check_literal_traits(literal, state.bounds.traits, &Type::Var(var))?;
                }
                Ok(())
            }
            concrete => {
                if let Some(kind) = bounds.literal
                    && !kind.accepts(&concrete)
                {
                    return Err(UnifyError::Literal { ty: concrete, kind });
                }
                for one in bounds.traits.each() {
                    self.require_trait(&concrete, one)?;
                }
                Ok(())
            }
        }
    }

    fn require_trait(&mut self, ty: &Type, one: Traits) -> Result<(), UnifyError> {
        let ty = self.shallow(ty);
        if let Type::Var(_) = ty {
            return self.add_bounds(&ty, Bounds::traits(one));
        }
        if matches!(ty, Type::Unknown) {
            return Ok(());
        }
        let ok = match one {
            Traits::ADD => ty.is_numeric() || ty == Type::String,
            Traits::ARITHMETIC | Traits::NUMERIC => ty.is_numeric(),
            Traits::ORDER => ty.is_numeric() || ty == Type::String,
            Traits::NEGATE => {
                matches!(ty, Type::Float(_)) || matches!(ty, Type::Int(i) if i.signed())
            }
            Traits::LENGTH => matches!(ty, Type::String | Type::Array(_) | Type::Dict(..)),
            Traits::EQUALITY => match &ty {
                Type::Array(element) => return self.require_trait(element, Traits::EQUALITY),
                Type::Dict(_, value) => return self.require_trait(value, Traits::EQUALITY),
                Type::Union(members) => {
                    for member in members.clone() {
                        self.require_trait(&member, Traits::EQUALITY)?;
                    }
                    return Ok(());
                }
                Type::Named(named) => {
                    // Comparable when every field is; a type reached again
                    // through its own fields is assumed comparable.
                    let whole = Type::Named(named.clone());
                    if self.in_progress.contains(&whole) {
                        return Ok(());
                    }
                    let fields: Vec<Type> = self
                        .field_types
                        .get(named.id.0 as usize)
                        .map(|fields| fields.iter().map(|f| f.substitute(&named.args)).collect())
                        .unwrap_or_default();
                    self.in_progress.push(whole);
                    let result = fields
                        .iter()
                        .try_for_each(|field| self.require_trait(field, Traits::EQUALITY));
                    self.in_progress.pop();
                    return result.map_err(|_| UnifyError::Unsupported {
                        ty: ty.clone(),
                        traits: one,
                    });
                }
                // Function values have no meaningful equality.
                Type::Range | Type::Param(_) | Type::Function(_) => false,
                _ => true,
            },
            _ => true,
        };
        if ok {
            Ok(())
        } else {
            Err(UnifyError::Unsupported { ty, traits: one })
        }
    }

    fn occurs(&self, var: TyVar, ty: &Type) -> bool {
        match self.shallow(ty) {
            Type::Var(other) => other == var,
            Type::Array(element) => self.occurs(var, &element),
            Type::Dict(key, value) => self.occurs(var, &key) || self.occurs(var, &value),
            Type::Union(members) => members.iter().any(|m| self.occurs(var, m)),
            Type::Named(named) => named.args.iter().any(|a| self.occurs(var, a)),
            Type::Function(function) => function.types().any(|t| self.occurs(var, t)),
            _ => false,
        }
    }

    fn bind(&mut self, var: TyVar, ty: Type) -> Result<(), UnifyError> {
        if let Type::Var(other) = ty
            && other == var
        {
            return Ok(());
        }
        if self.occurs(var, &ty) {
            return Err(UnifyError::Infinite);
        }
        let bounds = self.vars[var.0 as usize].bounds;
        self.vars[var.0 as usize].binding = Some(ty.clone());
        if let Err(error) = self.add_bounds(&ty, bounds) {
            self.vars[var.0 as usize].binding = None;
            return Err(error);
        }
        Ok(())
    }

    pub fn unify(&mut self, expected: &Type, found: &Type) -> Result<(), UnifyError> {
        let a = self.shallow(expected);
        let b = self.shallow(found);
        match (&a, &b) {
            (Type::Unknown, _) | (_, Type::Unknown) => Ok(()),
            (Type::Var(x), _) => self.bind(*x, b.clone()),
            (_, Type::Var(y)) => self.bind(*y, a.clone()),
            (Type::Array(x), Type::Array(y)) => self.unify(x, y).map_err(|_| self.mismatch(&a, &b)),
            (Type::Dict(k1, v1), Type::Dict(k2, v2)) => self
                .unify(k1, k2)
                .and_then(|_| self.unify(v1, v2))
                .map_err(|_| self.mismatch(&a, &b)),
            (Type::Union(xs), Type::Union(ys)) => {
                if xs.len() != ys.len() {
                    return Err(self.mismatch(&a, &b));
                }
                // Members are sorted; unresolved members sort together, so a
                // pairwise match handles the common `T or nothing` shapes.
                for (x, y) in xs.iter().zip(ys) {
                    self.unify(x, y).map_err(|_| self.mismatch(&a, &b))?;
                }
                Ok(())
            }
            (Type::Function(x), Type::Function(y)) => {
                let same_shape = x.params.len() == y.params.len()
                    && x.params.iter().zip(&y.params).all(|(p, q)| p.0 == q.0);
                if !same_shape {
                    return Err(self.mismatch(&a, &b));
                }
                for ((_, p), (_, q)) in x.params.iter().zip(&y.params) {
                    self.unify(p, q).map_err(|_| self.mismatch(&a, &b))?;
                }
                self.unify(&x.ret, &y.ret)
                    .map_err(|_| self.mismatch(&a, &b))
            }
            (Type::Named(x), Type::Named(y)) if x.id == y.id => {
                for (p, q) in x.args.iter().zip(&y.args) {
                    self.unify(p, q).map_err(|_| self.mismatch(&a, &b))?;
                }
                Ok(())
            }
            _ if a == b => Ok(()),
            _ => Err(self.mismatch(&a, &b)),
        }
    }

    fn mismatch(&self, a: &Type, b: &Type) -> UnifyError {
        UnifyError::Mismatch(self.resolve_for_display(a), self.resolve_for_display(b))
    }

    /// Applies literal defaults to an unresolved variable. Returns false when
    /// the variable has no literal kind to default from.
    pub fn default_var(&mut self, var: TyVar) -> bool {
        let state = &self.vars[var.0 as usize];
        if state.binding.is_some() {
            return true;
        }
        match state.bounds.literal {
            Some(kind) => {
                self.vars[var.0 as usize].binding = Some(kind.default_type());
                true
            }
            None => false,
        }
    }

    /// Unresolved variables reachable from a type, in first-occurrence order.
    pub fn free_vars(&self, ty: &Type, out: &mut Vec<TyVar>) {
        match self.shallow(ty) {
            Type::Var(var) => {
                if !out.contains(&var) {
                    out.push(var);
                }
            }
            Type::Array(element) => self.free_vars(&element, out),
            Type::Dict(key, value) => {
                self.free_vars(&key, out);
                self.free_vars(&value, out);
            }
            Type::Union(members) => {
                for member in &members {
                    self.free_vars(member, out);
                }
            }
            Type::Named(named) => {
                for arg in &named.args {
                    self.free_vars(arg, out);
                }
            }
            Type::Function(function) => {
                for ty in function.types() {
                    self.free_vars(ty, out);
                }
            }
            _ => {}
        }
    }
}

fn check_literal_traits(literal: LiteralKind, traits: Traits, ty: &Type) -> Result<(), UnifyError> {
    // An unsigned literal cannot be negated.
    if literal == LiteralKind::Unsigned && traits.contains(Traits::NEGATE) {
        return Err(UnifyError::Unsupported {
            ty: ty.clone(),
            traits: Traits::NEGATE,
        });
    }
    Ok(())
}

pub fn int_type_by_name(name: &str) -> Option<IntType> {
    Some(match name {
        "integer" | "integer64" => IntType::I64,
        "integer8" => IntType::I8,
        "integer16" => IntType::I16,
        "integer32" => IntType::I32,
        "unsigned" | "unsigned64" => IntType::U64,
        "unsigned8" | "byte" => IntType::U8,
        "unsigned16" => IntType::U16,
        "unsigned32" => IntType::U32,
        _ => return None,
    })
}

pub fn float_type_by_name(name: &str) -> Option<FloatType> {
    match name {
        "float" | "float64" => Some(FloatType::F64),
        "float32" => Some(FloatType::F32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_variables_default_and_specialize() {
        let mut unifier = Unifier::new();
        let a = unifier.fresh_bounded(Bounds::literal(LiteralKind::Integer));
        unifier.unify(&Type::Int(IntType::I8), &a).unwrap();
        assert_eq!(unifier.resolve(&a), Type::Int(IntType::I8));

        let b = unifier.fresh_bounded(Bounds::literal(LiteralKind::Integer));
        assert!(unifier.unify(&Type::FLOAT, &b).is_err());

        let c = unifier.fresh_bounded(Bounds::literal(LiteralKind::Float));
        let Type::Var(var) = c else { panic!() };
        assert!(unifier.default_var(var));
        assert_eq!(unifier.resolve(&c), Type::FLOAT);
    }

    #[test]
    fn traits_are_checked_on_binding() {
        let mut unifier = Unifier::new();
        let a = unifier.fresh_bounded(Bounds::traits(Traits::ARITHMETIC));
        assert!(unifier.unify(&Type::String, &a).is_err());
        let b = unifier.fresh_bounded(Bounds::traits(Traits::ADD));
        unifier.unify(&Type::String, &b).unwrap();
        let c = unifier.fresh_bounded(Bounds::traits(Traits::EQUALITY));
        unifier.unify(&Type::array(Type::INTEGER), &c).unwrap();
    }

    #[test]
    fn variables_merge_bounds() {
        let mut unifier = Unifier::new();
        let a = unifier.fresh_bounded(Bounds::literal(LiteralKind::Integer));
        let b = unifier.fresh_bounded(Bounds::literal(LiteralKind::Float));
        assert!(matches!(
            unifier.unify(&a, &b),
            Err(UnifyError::LiteralConflict)
        ));
        let c = unifier.fresh_bounded(Bounds::traits(Traits::ARITHMETIC));
        let d = unifier.fresh_bounded(Bounds::literal(LiteralKind::Integer));
        unifier.unify(&c, &d).unwrap();
        let Type::Var(var) = unifier.shallow(&c) else {
            panic!()
        };
        assert_eq!(unifier.bounds(var).literal, Some(LiteralKind::Integer));
        assert!(unifier.bounds(var).traits.contains(Traits::ARITHMETIC));
    }

    #[test]
    fn occurs_check() {
        let mut unifier = Unifier::new();
        let a = unifier.fresh();
        assert!(matches!(
            unifier.unify(&a, &Type::array(a.clone())),
            Err(UnifyError::Infinite)
        ));
    }

    #[test]
    fn unions_unify_member_wise() {
        let mut unifier = Unifier::new();
        let a = unifier.fresh();
        let generic = Type::union([a.clone(), Type::Nothing]);
        let concrete = Type::union([Type::String, Type::Nothing]);
        unifier.unify(&generic, &concrete).unwrap();
        assert_eq!(unifier.resolve(&a), Type::String);
    }
}
