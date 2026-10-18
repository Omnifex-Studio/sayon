//! IR definitions.
//!
//! Functions are monomorphic control-flow graphs over typed slots. Ownership
//! is explicit: operands copy, move, or borrow a place; `Drop` destroys
//! whatever a place still holds (a moved-from place is empty, so dropping it
//! does nothing). Operations that can fail are block terminators with a normal
//! and an error successor; on failure the error is stored in the function's
//! error slot.

use sayon_check::types::{FloatType, IntType, Type, TypeDef};
use sayon_syntax::Span;
use sayon_syntax::ast::ParamMode;

/// Whether a parameter receives a reference to the caller's value: update
/// parameters always do, so changes reach the caller; read parameters do
/// for values that are not Copy.
pub fn by_reference(mode: ParamMode, ty: &Type) -> bool {
    match mode {
        ParamMode::Own => false,
        ParamMode::Update => true,
        ParamMode::Read => !ty.is_copy(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FunctionId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalId(pub u32);

#[derive(Debug)]
pub struct Module {
    pub functions: Vec<Function>,
    pub globals: Vec<Global>,
    /// Global initializers, run in order before the entry function.
    pub initializers: Vec<(GlobalId, FunctionId)>,
    pub entry: FunctionId,
    /// Member types of every union in the program. A union value's tag is the
    /// member type's position here plus one, the same in every union, so a
    /// value of `A or B or C` is already a valid `A or B` when it holds an `A`
    /// or a `B`. Tag zero marks an empty place.
    pub union_members: Vec<Type>,
    /// Test functions, when the program was lowered for testing.
    pub tests: Vec<(String, FunctionId)>,
    /// Declared structures and enums, indexed by `TypeId`, followed by the
    /// environment structures of closures.
    pub types: Vec<TypeDef>,
}

impl Module {
    pub fn function(&self, id: FunctionId) -> &Function {
        &self.functions[id.0 as usize]
    }

    pub fn union_member(&self, tag: u32) -> Option<&Type> {
        self.union_members.get(tag.checked_sub(1)? as usize)
    }

    /// The type stored at `projection` inside a value of type `ty`.
    pub fn project_type(&self, ty: &Type, projection: &Projection) -> Type {
        use sayon_check::types::{structure_fields, variant_fields};
        match (projection, ty) {
            (Projection::Index(_), Type::Array(element) | Type::View(element, _)) => {
                (**element).clone()
            }
            (Projection::DictValue(_), Type::Dict(_, value)) => (**value).clone(),
            (Projection::DictKey(_), Type::Dict(key, _)) => (**key).clone(),
            (Projection::Payload(tag), _) => {
                self.union_member(*tag).cloned().unwrap_or(Type::Unknown)
            }
            (Projection::Field(field), Type::Named(named)) => {
                structure_fields(&self.types, named)[*field as usize].clone()
            }
            (Projection::VariantField { variant, field }, Type::Named(named)) => {
                variant_fields(&self.types, named, *variant)[*field as usize].clone()
            }
            (projection, ty) => panic!("invalid projection {projection:?} on {ty}"),
        }
    }
}

#[derive(Debug)]
pub struct Global {
    pub name: String,
    pub ty: Type,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    /// Holds a value the function owns (or a Copy value).
    Value,
    /// Holds a reference to a place owned elsewhere.
    Ref { mutable: bool },
}

#[derive(Debug, Clone)]
pub struct Slot {
    pub ty: Type,
    pub kind: SlotKind,
    pub name: Option<String>,
}

#[derive(Debug)]
pub struct Function {
    pub name: String,
    pub span: Span,
    /// The first `params.len()` slots are the parameters, in order. Code
    /// reached through a function value takes a reference to its
    /// environment first.
    pub params: Vec<SlotId>,
    /// Parameters a caller must supply; the rest have defaults computed in
    /// the prologue when the caller passes fewer arguments.
    pub required_params: usize,
    pub ret: Type,
    pub ret_slot: SlotId,
    pub error_slot: SlotId,
    /// Holds the number of arguments the caller supplied.
    pub argc_slot: SlotId,
    pub slots: Vec<Slot>,
    pub blocks: Vec<Block>,
    pub is_entry: bool,
}

impl Function {
    pub fn slot(&self, id: SlotId) -> &Slot {
        &self.slots[id.0 as usize]
    }

    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.0 as usize]
    }
}

#[derive(Debug, Clone)]
pub struct Block {
    pub insts: Vec<Inst>,
    pub term: Terminator,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    /// Array element at the (already bounds-checked) index stored in the slot.
    Index(SlotId),
    /// Dictionary entry value at the position stored in the slot.
    DictValue(SlotId),
    /// Dictionary entry key at the position stored in the slot.
    DictKey(SlotId),
    /// Payload of a union known to hold the member with this tag.
    Payload(u32),
    /// A field of a structure, by position.
    Field(u32),
    /// A field of an enum value known to be the given variant.
    VariantField { variant: u32, field: u32 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Base {
    Slot(SlotId),
    Global(GlobalId),
}

/// A storage location. A place rooted at a reference slot denotes the
/// referenced location.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub base: Base,
    pub projection: Vec<Projection>,
}

impl Place {
    pub fn slot(slot: SlotId) -> Place {
        Place {
            base: Base::Slot(slot),
            projection: Vec::new(),
        }
    }

    pub fn global(global: GlobalId) -> Place {
        Place {
            base: Base::Global(global),
            projection: Vec::new(),
        }
    }

    pub fn project(&self, projection: Projection) -> Place {
        let mut place = self.clone();
        place.projection.push(projection);
        place
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Constant {
    Int(i128, IntType),
    Float(f64, FloatType),
    Bool(bool),
    Nothing,
    /// Static string data; values made from it are not heap allocations.
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    /// Duplicates a Copy value.
    Copy(Place),
    /// Takes the value, leaving the place empty.
    Move(Place),
    /// A reference to the place, for a reference parameter.
    Borrow {
        place: Place,
        mutable: bool,
    },
    Const(Constant),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Equal,
    NotEqual,
    Less,
    Greater,
    LessEqual,
    GreaterEqual,
}

#[derive(Debug, Clone)]
pub enum FormatPart {
    Text(String),
    /// A value displayed with `tostring` rules.
    Value(Operand, Type),
}

#[derive(Debug, Clone)]
pub enum Rvalue {
    Use(Operand),
    Not(Operand),
    FloatNegate(Operand),
    FloatArith(ArithOp, Operand, Operand),
    Compare {
        op: CompareOp,
        ty: Type,
        lhs: Operand,
        rhs: Operand,
    },
    /// Builds a string from text and displayed values.
    Format(Vec<FormatPart>),
    Concat(Operand, Operand),
    MakeArray {
        element: Type,
        items: Vec<Operand>,
    },
    /// A dictionary with keys of type `key` and values of type `value`;
    /// each key is copied in.
    MakeDict {
        key: Type,
        value: Type,
        entries: Vec<(Operand, Operand)>,
    },
    /// Wraps a value as the union member with `tag`.
    MakeUnion {
        tag: u32,
        value: Operand,
    },
    /// A structure value of type `ty`, with its fields in declaration order.
    MakeRecord {
        ty: Type,
        fields: Vec<Operand>,
    },
    /// A value of the enum `ty`: one variant and its fields.
    MakeVariant {
        ty: Type,
        variant: u32,
        fields: Vec<Operand>,
    },
    /// True when the enum value is the given variant.
    VariantIs {
        value: Operand,
        variant: u32,
    },
    /// True when the union holds the member with `tag`.
    TagIs {
        value: Operand,
        tag: u32,
    },
    /// Name of the member type a union currently holds.
    UnionTypeName(Operand),
    Length {
        value: Operand,
        ty: Type,
    },
    ByteLength(Operand),
    HasKey {
        dict: Operand,
        key: Operand,
    },
    Clone {
        value: Operand,
        ty: Type,
    },
    ErrorMessage(Operand),
    ErrorCode(Operand),
    /// Number of values a range yields.
    RangeLength(Operand),
    /// The k-th value of a range (k < length).
    RangeAt {
        range: Operand,
        index: Operand,
    },
    /// Removes the last element of an array, producing `T or nothing`. An
    /// element that is not already a union is wrapped with `element_tag`; an
    /// empty array produces `nothing`, wrapped with `nothing_tag`.
    Pop {
        array: Place,
        element_tag: Option<u32>,
        nothing_tag: Option<u32>,
    },
    /// Integer addition that cannot overflow (loop counters below a length).
    Increment(Operand),
    /// Removes a dictionary's entry for a key, producing its value, or
    /// `nothing` when there is none; wrapped as `Pop` wraps an element.
    RemoveKey {
        dict: Place,
        key: Operand,
        value_tag: Option<u32>,
        nothing_tag: Option<u32>,
    },
    /// The character starting at byte `offset` of a string, as a new
    /// string; advances `offset` past it.
    NextChar {
        text: Operand,
        offset: Place,
    },
    /// A function value calling `function`. A closure also gets a new
    /// environment: a structure of type `env` holding the captured values.
    MakeClosure {
        function: FunctionId,
        env: Option<Type>,
        captures: Vec<Operand>,
    },
}

#[derive(Debug, Clone)]
pub enum Inst {
    /// Initializes `dest` (which must be empty).
    Assign {
        dest: Place,
        value: Rvalue,
    },
    /// Makes a reference slot refer to a place.
    Bind {
        slot: SlotId,
        place: Place,
        mutable: bool,
    },
    /// Destroys the value at a place if it holds one, leaving it empty.
    Drop(Place),
    Push {
        array: Place,
        value: Operand,
    },
    /// Inserts or replaces a dictionary entry.
    DictSet {
        dict: Place,
        key: Operand,
        value: Operand,
    },
}

#[derive(Debug, Clone)]
pub enum Op {
    Call {
        function: FunctionId,
        args: Vec<Operand>,
    },
    /// Calls the function value at `callee`, passing a reference to its
    /// environment before the arguments.
    CallValue {
        callee: Place,
        args: Vec<Operand>,
    },
    IntArith {
        op: ArithOp,
        ty: IntType,
        lhs: Operand,
        rhs: Operand,
    },
    IntNegate {
        ty: IntType,
        value: Operand,
    },
    Convert {
        from: Type,
        to: Type,
        value: Operand,
    },
    /// Fails unless `index` is a valid position in `array`; stores it in dest.
    CheckIndex {
        array: Operand,
        index: Operand,
    },
    /// A view of the elements from `start` up to `end` of the array or view
    /// at `base` (all of them when the bounds are left out); fails when the
    /// bounds are outside it.
    MakeView {
        base: Place,
        start: Option<Operand>,
        end: Option<Operand>,
    },
    /// Exchanges two elements of an array; fails outside the array.
    Swap {
        array: Place,
        first: Operand,
        second: Operand,
    },
    /// Removes and produces the element at a position of an array, moving
    /// the later elements down; fails outside the array.
    RemoveAt {
        array: Place,
        index: Operand,
    },
    /// The character at a position of a string, as a new string; fails
    /// outside the string.
    CharAt {
        text: Operand,
        index: Operand,
    },
    /// Finds `key`; stores the entry position in dest or fails.
    DictFind {
        dict: Operand,
        key: Operand,
    },
    Print(Vec<(Operand, Type)>),
    Assert {
        condition: Operand,
        message: Option<Operand>,
    },
    MakeRange {
        start: Operand,
        end: Operand,
        step: Option<Operand>,
    },
    /// Parses text to `integer or float`.
    ToNumber {
        text: Operand,
        integer_tag: u32,
        float_tag: u32,
    },
    ReadFile(Operand),
    WriteFile {
        path: Operand,
        content: Operand,
    },
    /// Always fails with a UserError carrying the message.
    Fail(Operand),
    /// Always fails with the given Error value.
    Raise(Operand),
    /// Calls a host primitive. For a `string or nothing` result, `tags` holds
    /// the program-wide tags of `string` and `nothing`.
    Host {
        function: sayon_runtime::host::Host,
        args: Vec<Operand>,
        tags: Option<(u32, u32)>,
    },
}

#[derive(Debug, Clone)]
pub enum Terminator {
    Goto(BlockId),
    Branch {
        condition: Operand,
        then_block: BlockId,
        else_block: BlockId,
    },
    /// Returns the value in the return slot.
    Return,
    /// Propagates the error in the error slot to the caller.
    ReturnError,
    Op {
        op: Op,
        dest: Option<Place>,
        normal: BlockId,
        error: BlockId,
        span: Span,
    },
    Unreachable,
}

impl Terminator {
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Terminator::Goto(target) => vec![*target],
            Terminator::Branch {
                then_block,
                else_block,
                ..
            } => vec![*then_block, *else_block],
            Terminator::Op { normal, error, .. } => vec![*normal, *error],
            Terminator::Return | Terminator::ReturnError | Terminator::Unreachable => Vec::new(),
        }
    }
}
