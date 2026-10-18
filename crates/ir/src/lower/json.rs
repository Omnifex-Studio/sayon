//! Reading typed values from JSON and writing them as JSON.
//!
//! `json.from_json` and `json.to_json` lower to calls of generated functions,
//! one reader and one writer per type, made when a type is first converted.
//! A reader takes a `Json` value and the path of that value in the whole
//! document, such as `$.points[2].x`, for its errors; it handles one level
//! of the type and calls the readers of the parts, so recursive types work.
//!
//! The mapping: numbers, booleans, text, and `nothing` (null) are the JSON
//! values of the same kind; an array is a list; a dictionary with string
//! keys, and a structure, are objects; an enum variant without fields is the
//! variant's name as text, and one with fields an object with one member,
//! named after the variant, holding an object of its fields; a union is
//! whichever member reads that kind of JSON value; `Json` itself is any
//! value. A structure field missing from an object reads as `nothing` when
//! its type allows it; an object's other members are ignored.

use super::*;
use sayon_check::hir::JsonSupport;
use sayon_check::types::{FloatType, VariantDef};
use sayon_runtime::host::Host;

/// The variants of `standard/json`'s `Json`, by position.
struct JsonVariants {
    null: u32,
    flag: u32,
    number: u32,
    text: u32,
    list: u32,
    object: u32,
}

impl<'p> Lowerer<'p> {
    fn json_support(&self) -> JsonSupport {
        self.program
            .json
            .expect("typed JSON conversion needs standard/json")
    }

    fn json_type(&self) -> Type {
        let id = self.json_support().json;
        let def = &self.program.types[id.0 as usize];
        Type::named(NamedType {
            id,
            name: def.name.clone(),
            args: Vec::new(),
            copy: def.copy,
        })
    }

    fn json_variants(&self) -> JsonVariants {
        let def = &self.program.types[self.json_support().json.0 as usize];
        let position = |name: &str| {
            def.variants()
                .iter()
                .position(|variant| variant.name == name)
                .expect("a Json variant") as u32
        };
        JsonVariants {
            null: position("null"),
            flag: position("flag"),
            number: position("number"),
            text: position("text"),
            list: position("list"),
            object: position("object"),
        }
    }

    /// The function reading a `Json` value, and its path, into `ty`.
    pub(super) fn decoder(&mut self, ty: &Type, span: Span) -> FunctionId {
        if let Some(id) = self.decoders.get(ty) {
            return *id;
        }
        let id = FunctionId(self.functions.len() as u32);
        self.functions.push(None);
        self.decoders.insert(ty.clone(), id);
        let name = format!("<read {ty} from JSON>");
        let function = FnLower::new(self, &[], Vec::new(), name, span).lower_decoder(ty, span);
        self.functions[id.0 as usize] = Some(function);
        id
    }

    /// The function writing a value of `ty` as `Json`.
    pub(super) fn encoder(&mut self, ty: &Type, span: Span) -> FunctionId {
        if let Some(id) = self.encoders.get(ty) {
            return *id;
        }
        let id = FunctionId(self.functions.len() as u32);
        self.functions.push(None);
        self.encoders.insert(ty.clone(), id);
        let name = format!("<write {ty} as JSON>");
        let function = FnLower::new(self, &[], Vec::new(), name, span).lower_encoder(ty, span);
        self.functions[id.0 as usize] = Some(function);
        id
    }
}

/// The code the host's integer primitives use for an integer type.
fn int_code(int: IntType) -> i128 {
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

fn borrow(place: &Place) -> Operand {
    Operand::Borrow {
        place: place.clone(),
        mutable: false,
    }
}

fn text(value: &str) -> Operand {
    Operand::Const(Constant::Str(value.to_string()))
}

fn integer(value: i128) -> Operand {
    Operand::Const(Constant::Int(value, IntType::I64))
}

/// How a type is described in an error: "expected integer".
fn describe(ty: &Type) -> String {
    match ty {
        Type::Nothing => "null".to_string(),
        Type::Boolean => "a boolean".to_string(),
        Type::String => "text".to_string(),
        Type::Int(_) | Type::Float(_) => ty.to_string(),
        Type::Array(_) => format!("a list for {ty}"),
        Type::Dict(..) => format!("an object for {ty}"),
        Type::Union(members) => members
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(" or "),
        Type::Named(_) => format!("a {ty}"),
        other => other.to_string(),
    }
}

impl<'l, 'p> FnLower<'l, 'p> {
    /// Moves or copies a value out of a temporary.
    fn take(&self, slot: SlotId) -> Operand {
        let place = Place::slot(slot);
        if self.f.slots[slot.0 as usize].ty.is_copy() {
            Operand::Copy(place)
        } else {
            Operand::Move(place)
        }
    }

    /// Calls a function of `standard/json`, or a generated reader or writer,
    /// into a new temporary.
    fn call_into(
        &mut self,
        function: FunctionId,
        args: Vec<Operand>,
        ty: Type,
        span: Span,
    ) -> SlotId {
        let result = self.temp(ty);
        self.op(Op::Call { function, args }, Some(Place::slot(result)), span);
        result
    }

    /// Calls a `standard/json` function that always fails.
    fn fail_with(&mut self, function: hir::FuncId, args: Vec<Operand>, span: Span) {
        let function = self.l.instance(function, Vec::new());
        self.call_into(function, args, Type::Nothing, span);
        self.terminate(Terminator::Unreachable);
    }

    fn text_temp(&mut self, value: &str) -> SlotId {
        let slot = self.temp(Type::String);
        self.assign(Place::slot(slot), Rvalue::Use(text(value)));
        slot
    }

    /// Continues when `value` is the `Json` variant, and otherwise fails,
    /// saying what was expected.
    fn expect_variant(&mut self, value: &Place, path: &Place, variant: u32, ty: &Type, span: Span) {
        let ok = self.new_slot(Type::Boolean, SlotKind::Value, None);
        self.assign(
            Place::slot(ok),
            Rvalue::VariantIs {
                value: borrow(value),
                variant,
            },
        );
        let next = self.new_block();
        let fail = self.new_block();
        self.terminate(Terminator::Branch {
            condition: Operand::Copy(Place::slot(ok)),
            then_block: next,
            else_block: fail,
        });
        self.switch_to(fail);
        self.mismatch(value, path, ty, span);
        self.switch_to(next);
    }

    fn mismatch(&mut self, value: &Place, path: &Place, ty: &Type, span: Span) {
        let expected = self.text_temp(&describe(ty));
        let mismatch = self.l.json_support().mismatch;
        self.fail_with(
            mismatch,
            vec![borrow(value), borrow(path), borrow(&Place::slot(expected))],
            span,
        );
    }

    /// Branches on a flag: `then` continues in a new block, which is
    /// current afterwards; the other branch goes to the returned block.
    fn branch_on(&mut self, value: Rvalue) -> BlockId {
        let flag = self.new_slot(Type::Boolean, SlotKind::Value, None);
        self.assign(Place::slot(flag), value);
        let then = self.new_block();
        let otherwise = self.new_block();
        self.terminate(Terminator::Branch {
            condition: Operand::Copy(Place::slot(flag)),
            then_block: then,
            else_block: otherwise,
        });
        self.switch_to(then);
        otherwise
    }

    /// Runs `body` for each position from 0 below the length of `of`,
    /// each time in a scope of its own.
    fn each_position(&mut self, of: Operand, ty: Type, mut body: impl FnMut(&mut Self, SlotId)) {
        let count = self.new_slot(Type::INTEGER, SlotKind::Value, None);
        let index = self.new_slot(Type::INTEGER, SlotKind::Value, None);
        self.assign(Place::slot(count), Rvalue::Length { value: of, ty });
        self.assign(Place::slot(index), Rvalue::Use(integer(0)));
        let head = self.new_block();
        self.goto(head);
        self.switch_to(head);
        let exit = self.branch_on(Rvalue::Compare {
            op: CompareOp::Less,
            ty: Type::INTEGER,
            lhs: Operand::Copy(Place::slot(index)),
            rhs: Operand::Copy(Place::slot(count)),
        });
        self.push_scope();
        body(self, index);
        self.pop_scope();
        self.assign(
            Place::slot(index),
            Rvalue::Increment(Operand::Copy(Place::slot(index))),
        );
        self.goto(head);
        self.switch_to(exit);
    }

    // ----- reading -------------------------------------------------------------

    fn lower_decoder(mut self, ty: &Type, span: Span) -> Function {
        let json = self.l.json_type();
        let value = self.new_slot(
            json,
            SlotKind::Ref { mutable: false },
            Some("value".to_string()),
        );
        let path = self.new_slot(
            Type::String,
            SlotKind::Ref { mutable: false },
            Some("path".to_string()),
        );
        self.f.params = vec![value, path];
        self.f.required_params = 2;
        self.finish_slots(ty.clone());
        self.push_scope();
        self.block_scopes.push(0);
        let entry = self.new_block();
        self.switch_to(entry);
        self.push_scope();
        let result = self.decode(ty, &Place::slot(value), &Place::slot(path), span);
        let operand = self.take(result);
        self.assign(Place::slot(self.f.ret_slot), Rvalue::Use(operand));
        self.pop_scope();
        self.leave(0, 0, true);
        self.terminate(Terminator::Return);
        self.f
    }

    /// Reads a part of a value with the part's own reader.
    fn decode_part(&mut self, ty: &Type, value: &Place, path: &Place, span: Span) -> SlotId {
        let function = self.l.decoder(ty, span);
        self.call_into(
            function,
            vec![borrow(value), borrow(path)],
            ty.clone(),
            span,
        )
    }

    /// Reads the `Json` value at `value` as `ty`, into a new temporary.
    fn decode(&mut self, ty: &Type, value: &Place, path: &Place, span: Span) -> SlotId {
        let variants = self.l.json_variants();
        let json_ty = self.l.json_type();
        let result = self.temp(ty.clone());
        let dest = Place::slot(result);
        let field = |variant: u32| value.project(Projection::VariantField { variant, field: 0 });
        if *ty == json_ty {
            self.assign(
                dest,
                Rvalue::Clone {
                    value: borrow(value),
                    ty: json_ty,
                },
            );
            return result;
        }
        match ty {
            Type::Nothing => {
                self.expect_variant(value, path, variants.null, ty, span);
                self.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing)));
            }
            Type::Boolean => {
                self.expect_variant(value, path, variants.flag, ty, span);
                self.assign(dest, Rvalue::Use(Operand::Copy(field(variants.flag))));
            }
            Type::String => {
                self.expect_variant(value, path, variants.text, ty, span);
                self.assign(
                    dest,
                    Rvalue::Clone {
                        value: borrow(&field(variants.text)),
                        ty: Type::String,
                    },
                );
            }
            Type::Int(int) => {
                self.expect_variant(value, path, variants.number, ty, span);
                let (host, wide) = if int.signed() {
                    (Host::JsonInteger, IntType::I64)
                } else {
                    (Host::JsonUnsigned, IntType::U64)
                };
                let args = vec![
                    borrow(&field(variants.number)),
                    borrow(path),
                    integer(int_code(*int)),
                ];
                if *int == wide {
                    self.op(
                        Op::Host {
                            function: host,
                            args,
                            tags: None,
                        },
                        Some(dest),
                        span,
                    );
                } else {
                    let raw = self.temp(Type::Int(wide));
                    self.op(
                        Op::Host {
                            function: host,
                            args,
                            tags: None,
                        },
                        Some(Place::slot(raw)),
                        span,
                    );
                    self.op(
                        Op::Convert {
                            from: Type::Int(wide),
                            to: ty.clone(),
                            value: Operand::Copy(Place::slot(raw)),
                        },
                        Some(dest),
                        span,
                    );
                }
            }
            Type::Float(float) => {
                self.expect_variant(value, path, variants.number, ty, span);
                let bits = if *float == FloatType::F32 { 32 } else { 64 };
                let args = vec![borrow(&field(variants.number)), borrow(path), integer(bits)];
                if *float == FloatType::F64 {
                    self.op(
                        Op::Host {
                            function: Host::JsonFloat,
                            args,
                            tags: None,
                        },
                        Some(dest),
                        span,
                    );
                } else {
                    let raw = self.temp(Type::FLOAT);
                    self.op(
                        Op::Host {
                            function: Host::JsonFloat,
                            args,
                            tags: None,
                        },
                        Some(Place::slot(raw)),
                        span,
                    );
                    self.op(
                        Op::Convert {
                            from: Type::FLOAT,
                            to: ty.clone(),
                            value: Operand::Copy(Place::slot(raw)),
                        },
                        Some(dest),
                        span,
                    );
                }
            }
            Type::Array(element) => {
                self.expect_variant(value, path, variants.list, ty, span);
                let items = field(variants.list);
                self.assign(
                    dest.clone(),
                    Rvalue::MakeArray {
                        element: (**element).clone(),
                        items: Vec::new(),
                    },
                );
                let index_path = self
                    .l
                    .instance(self.l.json_support().index_path, Vec::new());
                let element = (**element).clone();
                let list_ty = Type::array(json_ty.clone());
                self.each_position(borrow(&items), list_ty, |this, index| {
                    let item = items.project(Projection::Index(index));
                    let item_path = this.call_into(
                        index_path,
                        vec![borrow(path), Operand::Copy(Place::slot(index))],
                        Type::String,
                        span,
                    );
                    let decoded = this.decode_part(&element, &item, &Place::slot(item_path), span);
                    let operand = this.take(decoded);
                    this.push(Inst::Push {
                        array: dest.clone(),
                        value: operand,
                    });
                });
            }
            Type::Dict(_, element) => {
                self.expect_variant(value, path, variants.object, ty, span);
                let fields = field(variants.object);
                self.assign(
                    dest.clone(),
                    Rvalue::MakeDict {
                        key: Type::String,
                        value: (**element).clone(),
                        entries: Vec::new(),
                    },
                );
                let member_path = self
                    .l
                    .instance(self.l.json_support().member_path, Vec::new());
                let element = (**element).clone();
                let object_ty = Type::dict(Type::String, json_ty.clone());
                self.each_position(borrow(&fields), object_ty, |this, index| {
                    let key = fields.project(Projection::DictKey(index));
                    let member = fields.project(Projection::DictValue(index));
                    let member_at = this.call_into(
                        member_path,
                        vec![borrow(path), borrow(&key)],
                        Type::String,
                        span,
                    );
                    let decoded =
                        this.decode_part(&element, &member, &Place::slot(member_at), span);
                    let operand = this.take(decoded);
                    this.push(Inst::DictSet {
                        dict: dest.clone(),
                        key: borrow(&key),
                        value: operand,
                    });
                });
            }
            Type::Named(named) => {
                let def = self.l.program.types[named.id.0 as usize].clone();
                if def.is_enum() {
                    self.decode_enum(
                        ty,
                        &def.name,
                        def.variants(),
                        &named.args,
                        value,
                        path,
                        &dest,
                        span,
                    );
                } else {
                    self.expect_variant(value, path, variants.object, ty, span);
                    let fields = field(variants.object);
                    let mut parts = Vec::new();
                    for part in def.fields() {
                        let part_ty = part.ty.substitute(&named.args);
                        parts.push(self.decode_field(&fields, &part.name, &part_ty, path, span));
                    }
                    let fields = parts.into_iter().map(|slot| self.take(slot)).collect();
                    self.assign(
                        dest,
                        Rvalue::MakeRecord {
                            ty: ty.clone(),
                            fields,
                        },
                    );
                }
            }
            Type::Union(members) => self.decode_union(ty, members, value, path, &dest, span),
            other => unreachable!("{other} cannot be read from JSON"),
        }
        result
    }

    /// Reads the member `name` of the object `fields` as `ty`. A missing
    /// member is `nothing` when `ty` allows it.
    fn decode_field(
        &mut self,
        fields: &Place,
        name: &str,
        ty: &Type,
        path: &Place,
        span: Span,
    ) -> SlotId {
        let support = self.l.json_support();
        let result = self.temp(ty.clone());
        let name_text = self.text_temp(name);
        let absent = self.branch_on(Rvalue::HasKey {
            dict: borrow(fields),
            key: borrow(&Place::slot(name_text)),
        });
        let join = self.new_block();
        self.push_scope();
        let position = self.new_slot(Type::INTEGER, SlotKind::Value, None);
        self.op(
            Op::DictFind {
                dict: borrow(fields),
                key: borrow(&Place::slot(name_text)),
            },
            Some(Place::slot(position)),
            span,
        );
        let member = fields.project(Projection::DictValue(position));
        let member_path = self.l.instance(support.member_path, Vec::new());
        let member_at = self.call_into(
            member_path,
            vec![borrow(path), borrow(&Place::slot(name_text))],
            Type::String,
            span,
        );
        let decoded = self.decode_part(ty, &member, &Place::slot(member_at), span);
        let operand = self.take(decoded);
        self.assign(Place::slot(result), Rvalue::Use(operand));
        self.pop_scope();
        self.goto(join);

        self.switch_to(absent);
        let allows_nothing = *ty == Type::Nothing || ty.union_members().contains(&Type::Nothing);
        if !allows_nothing {
            self.fail_with(
                support.missing_field,
                vec![borrow(path), borrow(&Place::slot(name_text))],
                span,
            );
        } else if *ty == Type::Nothing {
            self.assign(
                Place::slot(result),
                Rvalue::Use(Operand::Const(Constant::Nothing)),
            );
        } else {
            let tag = self.l.tag(&Type::Nothing);
            self.assign(
                Place::slot(result),
                Rvalue::MakeUnion {
                    tag,
                    value: Operand::Const(Constant::Nothing),
                },
            );
        }
        self.goto(join);
        self.switch_to(join);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_enum(
        &mut self,
        ty: &Type,
        type_name: &str,
        all: &[VariantDef],
        args: &[Type],
        value: &Place,
        path: &Place,
        dest: &Place,
        span: Span,
    ) {
        let variants = self.l.json_variants();
        let support = self.l.json_support();
        let done = self.new_block();
        let type_text = self.text_temp(type_name);

        // A variant without fields is its name.
        let not_text = self.branch_on(Rvalue::VariantIs {
            value: borrow(value),
            variant: variants.text,
        });
        let name = value.project(Projection::VariantField {
            variant: variants.text,
            field: 0,
        });
        for (position, variant) in all.iter().enumerate() {
            if !variant.fields.is_empty() {
                continue;
            }
            let next = self.branch_on(Rvalue::Compare {
                op: CompareOp::Equal,
                ty: Type::String,
                lhs: borrow(&name),
                rhs: text(&variant.name),
            });
            self.assign(
                dest.clone(),
                Rvalue::MakeVariant {
                    ty: ty.clone(),
                    variant: position as u32,
                    fields: Vec::new(),
                },
            );
            self.goto(done);
            self.switch_to(next);
        }
        self.fail_with(
            support.unknown_variant,
            vec![borrow(path), borrow(&name), borrow(&Place::slot(type_text))],
            span,
        );

        // One with fields is an object with one member named after it.
        self.switch_to(not_text);
        let not_object = self.branch_on(Rvalue::VariantIs {
            value: borrow(value),
            variant: variants.object,
        });
        let fields = value.project(Projection::VariantField {
            variant: variants.object,
            field: 0,
        });
        let count = self.new_slot(Type::INTEGER, SlotKind::Value, None);
        self.assign(
            Place::slot(count),
            Rvalue::Length {
                value: borrow(&fields),
                ty: Type::dict(Type::String, self.l.json_type()),
            },
        );
        let not_one = self.branch_on(Rvalue::Compare {
            op: CompareOp::Equal,
            ty: Type::INTEGER,
            lhs: Operand::Copy(Place::slot(count)),
            rhs: integer(1),
        });
        let first = self.new_slot(Type::INTEGER, SlotKind::Value, None);
        self.assign(Place::slot(first), Rvalue::Use(integer(0)));
        let key = fields.project(Projection::DictKey(first));
        let inner = fields.project(Projection::DictValue(first));
        let member_path = self.l.instance(support.member_path, Vec::new());
        for (position, variant) in all.iter().enumerate() {
            if variant.fields.is_empty() {
                continue;
            }
            let next = self.branch_on(Rvalue::Compare {
                op: CompareOp::Equal,
                ty: Type::String,
                lhs: borrow(&key),
                rhs: text(&variant.name),
            });
            self.push_scope();
            let inner_path = self.call_into(
                member_path,
                vec![borrow(path), borrow(&key)],
                Type::String,
                span,
            );
            let inner_path = Place::slot(inner_path);
            let object = Type::dict(Type::String, self.l.json_type());
            self.expect_variant(&inner, &inner_path, variants.object, &object, span);
            let inner_fields = inner.project(Projection::VariantField {
                variant: variants.object,
                field: 0,
            });
            let mut parts = Vec::new();
            for part in &variant.fields {
                let part_ty = part.ty.substitute(args);
                parts.push(self.decode_field(
                    &inner_fields,
                    &part.name,
                    &part_ty,
                    &inner_path,
                    span,
                ));
            }
            let fields = parts.into_iter().map(|slot| self.take(slot)).collect();
            self.assign(
                dest.clone(),
                Rvalue::MakeVariant {
                    ty: ty.clone(),
                    variant: position as u32,
                    fields,
                },
            );
            self.pop_scope();
            self.goto(done);
            self.switch_to(next);
        }
        self.fail_with(
            support.unknown_variant,
            vec![borrow(path), borrow(&key), borrow(&Place::slot(type_text))],
            span,
        );

        self.switch_to(not_one);
        self.mismatch(value, path, ty, span);
        self.switch_to(not_object);
        self.mismatch(value, path, ty, span);
        self.switch_to(done);
    }

    /// Reads a union as its member for the kind of JSON value it is.
    fn decode_union(
        &mut self,
        ty: &Type,
        members: &[Type],
        value: &Place,
        path: &Place,
        dest: &Place,
        span: Span,
    ) {
        let variants = self.l.json_variants();
        let json_ty = self.l.json_type();
        let program = self.l.program;
        let is_enum = |member: &Type| match member {
            Type::Named(named) => program.types[named.id.0 as usize].is_enum(),
            _ => false,
        };
        let find = |accepts: &dyn Fn(&Type) -> bool| {
            members
                .iter()
                .find(|m| accepts(m))
                .or_else(|| members.iter().find(|m| **m == json_ty))
                .cloned()
        };
        let integer_member = members.iter().find(|m| matches!(m, Type::Int(_))).cloned();
        let float_member = members
            .iter()
            .find(|m| matches!(m, Type::Float(_)))
            .cloned();
        let kinds: Vec<(u32, Option<Type>)> = vec![
            (variants.null, find(&|m| *m == Type::Nothing)),
            (variants.flag, find(&|m| *m == Type::Boolean)),
            (
                variants.number,
                find(&|m| matches!(m, Type::Int(_) | Type::Float(_))),
            ),
            (variants.text, find(&|m| *m == Type::String || is_enum(m))),
            (variants.list, find(&|m| matches!(m, Type::Array(_)))),
            (
                variants.object,
                find(&|m| {
                    matches!(m, Type::Dict(..)) || matches!(m, Type::Named(_)) && *m != json_ty
                }),
            ),
        ];
        let done = self.new_block();
        let read_as = |this: &mut Self, member: &Type| {
            let decoded = this.decode_part(member, value, path, span);
            let tag = this.l.tag(member);
            let operand = this.take(decoded);
            this.assign(
                dest.clone(),
                Rvalue::MakeUnion {
                    tag,
                    value: operand,
                },
            );
            this.goto(done);
        };
        for (variant, member) in kinds {
            let Some(member) = member else {
                continue;
            };
            let next = self.branch_on(Rvalue::VariantIs {
                value: borrow(value),
                variant,
            });
            match (&integer_member, &float_member) {
                // A number reads as the integer member when it is written
                // as an integer.
                (Some(int), Some(float)) if variant == variants.number => {
                    let digits = value.project(Projection::VariantField {
                        variant: variants.number,
                        field: 0,
                    });
                    let integral = self.new_slot(Type::Boolean, SlotKind::Value, None);
                    self.op(
                        Op::Host {
                            function: Host::JsonIntegral,
                            args: vec![borrow(&digits)],
                            tags: None,
                        },
                        Some(Place::slot(integral)),
                        span,
                    );
                    let as_float = self.new_block();
                    let as_int = self.new_block();
                    self.terminate(Terminator::Branch {
                        condition: Operand::Copy(Place::slot(integral)),
                        then_block: as_int,
                        else_block: as_float,
                    });
                    self.switch_to(as_int);
                    read_as(self, int);
                    self.switch_to(as_float);
                    read_as(self, float);
                }
                _ => read_as(self, &member),
            }
            self.switch_to(next);
        }
        self.mismatch(value, path, ty, span);
        self.switch_to(done);
    }

    // ----- writing -------------------------------------------------------------

    fn lower_encoder(mut self, ty: &Type, span: Span) -> Function {
        let value = self.new_slot(
            ty.clone(),
            SlotKind::Ref { mutable: false },
            Some("value".to_string()),
        );
        self.f.params = vec![value];
        self.f.required_params = 1;
        let json = self.l.json_type();
        self.finish_slots(json);
        self.push_scope();
        self.block_scopes.push(0);
        let entry = self.new_block();
        self.switch_to(entry);
        self.push_scope();
        let result = self.encode(ty, &Place::slot(value), span);
        let operand = self.take(result);
        self.assign(Place::slot(self.f.ret_slot), Rvalue::Use(operand));
        self.pop_scope();
        self.leave(0, 0, true);
        self.terminate(Terminator::Return);
        self.f
    }

    fn encode_part(&mut self, ty: &Type, value: &Place, span: Span) -> SlotId {
        let function = self.l.encoder(ty, span);
        let json = self.l.json_type();
        self.call_into(function, vec![borrow(value)], json, span)
    }

    fn make_json(&mut self, dest: &Place, variant: u32, fields: Vec<Operand>) {
        let ty = self.l.json_type();
        self.assign(
            dest.clone(),
            Rvalue::MakeVariant {
                ty,
                variant,
                fields,
            },
        );
    }

    /// An empty object, to fill with `DictSet`.
    fn empty_object(&mut self) -> SlotId {
        let json = self.l.json_type();
        let object = self.temp(Type::dict(Type::String, json.clone()));
        self.assign(
            Place::slot(object),
            Rvalue::MakeDict {
                key: Type::String,
                value: json,
                entries: Vec::new(),
            },
        );
        object
    }

    /// Writes the value at `value`, of type `ty`, as `Json` into a new
    /// temporary.
    fn encode(&mut self, ty: &Type, value: &Place, span: Span) -> SlotId {
        let variants = self.l.json_variants();
        let json_ty = self.l.json_type();
        let result = self.temp(json_ty.clone());
        let dest = Place::slot(result);
        if *ty == json_ty {
            self.assign(
                dest,
                Rvalue::Clone {
                    value: borrow(value),
                    ty: json_ty,
                },
            );
            return result;
        }
        match ty {
            Type::Nothing => self.make_json(&dest, variants.null, Vec::new()),
            Type::Boolean => {
                self.make_json(&dest, variants.flag, vec![Operand::Copy(value.clone())])
            }
            Type::String => {
                let copy = self.temp(Type::String);
                self.assign(
                    Place::slot(copy),
                    Rvalue::Clone {
                        value: borrow(value),
                        ty: Type::String,
                    },
                );
                self.make_json(&dest, variants.text, vec![Operand::Move(Place::slot(copy))]);
            }
            Type::Int(_) => {
                let digits = self.temp(Type::String);
                self.assign(
                    Place::slot(digits),
                    Rvalue::Format(vec![FormatPart::Value(
                        Operand::Copy(value.clone()),
                        ty.clone(),
                    )]),
                );
                self.make_json(
                    &dest,
                    variants.number,
                    vec![Operand::Move(Place::slot(digits))],
                );
            }
            Type::Float(float) => {
                let wide = if *float == FloatType::F64 {
                    Operand::Copy(value.clone())
                } else {
                    let wide = self.temp(Type::FLOAT);
                    self.op(
                        Op::Convert {
                            from: ty.clone(),
                            to: Type::FLOAT,
                            value: Operand::Copy(value.clone()),
                        },
                        Some(Place::slot(wide)),
                        span,
                    );
                    Operand::Copy(Place::slot(wide))
                };
                let bits = if *float == FloatType::F32 { 32 } else { 64 };
                let digits = self.temp(Type::String);
                self.op(
                    Op::Host {
                        function: Host::JsonFloatText,
                        args: vec![wide, integer(bits)],
                        tags: None,
                    },
                    Some(Place::slot(digits)),
                    span,
                );
                self.make_json(
                    &dest,
                    variants.number,
                    vec![Operand::Move(Place::slot(digits))],
                );
            }
            Type::Array(element) => {
                let items = self.temp(Type::array(json_ty.clone()));
                self.assign(
                    Place::slot(items),
                    Rvalue::MakeArray {
                        element: json_ty.clone(),
                        items: Vec::new(),
                    },
                );
                let element = (**element).clone();
                self.each_position(borrow(value), ty.clone(), |this, index| {
                    let item = value.project(Projection::Index(index));
                    let written = this.encode_part(&element, &item, span);
                    let operand = this.take(written);
                    this.push(Inst::Push {
                        array: Place::slot(items),
                        value: operand,
                    });
                });
                self.make_json(
                    &dest,
                    variants.list,
                    vec![Operand::Move(Place::slot(items))],
                );
            }
            Type::Dict(_, element) => {
                let object = self.empty_object();
                let element = (**element).clone();
                self.each_position(borrow(value), ty.clone(), |this, index| {
                    let key = value.project(Projection::DictKey(index));
                    let member = value.project(Projection::DictValue(index));
                    let written = this.encode_part(&element, &member, span);
                    let operand = this.take(written);
                    this.push(Inst::DictSet {
                        dict: Place::slot(object),
                        key: borrow(&key),
                        value: operand,
                    });
                });
                self.make_json(
                    &dest,
                    variants.object,
                    vec![Operand::Move(Place::slot(object))],
                );
            }
            Type::Named(named) => {
                let def = self.l.program.types[named.id.0 as usize].clone();
                if def.is_enum() {
                    self.encode_enum(def.variants(), &named.args, value, &dest, span);
                } else {
                    let object = self.empty_object();
                    for (position, part) in def.fields().iter().enumerate() {
                        let part_ty = part.ty.substitute(&named.args);
                        let place = value.project(Projection::Field(position as u32));
                        let written = self.encode_part(&part_ty, &place, span);
                        let operand = self.take(written);
                        self.push(Inst::DictSet {
                            dict: Place::slot(object),
                            key: text(&part.name),
                            value: operand,
                        });
                    }
                    self.make_json(
                        &dest,
                        variants.object,
                        vec![Operand::Move(Place::slot(object))],
                    );
                }
            }
            Type::Union(members) => {
                let done = self.new_block();
                for member in members {
                    let tag = self.l.tag(member);
                    let next = self.branch_on(Rvalue::TagIs {
                        value: borrow(value),
                        tag,
                    });
                    let payload = value.project(Projection::Payload(tag));
                    let written = self.encode_part(member, &payload, span);
                    let operand = self.take(written);
                    self.assign(dest.clone(), Rvalue::Use(operand));
                    self.goto(done);
                    self.switch_to(next);
                }
                self.terminate(Terminator::Unreachable);
                self.switch_to(done);
            }
            other => unreachable!("{other} cannot be written as JSON"),
        }
        result
    }

    fn encode_enum(
        &mut self,
        all: &[VariantDef],
        args: &[Type],
        value: &Place,
        dest: &Place,
        span: Span,
    ) {
        let variants = self.l.json_variants();
        let done = self.new_block();
        for (position, variant) in all.iter().enumerate() {
            let next = self.branch_on(Rvalue::VariantIs {
                value: borrow(value),
                variant: position as u32,
            });
            if variant.fields.is_empty() {
                let name = self.text_temp(&variant.name);
                self.make_json(dest, variants.text, vec![Operand::Move(Place::slot(name))]);
            } else {
                self.push_scope();
                let inner = self.empty_object();
                for (index, part) in variant.fields.iter().enumerate() {
                    let part_ty = part.ty.substitute(args);
                    let place = value.project(Projection::VariantField {
                        variant: position as u32,
                        field: index as u32,
                    });
                    let written = self.encode_part(&part_ty, &place, span);
                    let operand = self.take(written);
                    self.push(Inst::DictSet {
                        dict: Place::slot(inner),
                        key: text(&part.name),
                        value: operand,
                    });
                }
                let inner_json = self.temp(self.l.json_type());
                self.make_json(
                    &Place::slot(inner_json),
                    variants.object,
                    vec![Operand::Move(Place::slot(inner))],
                );
                let outer = self.empty_object();
                self.push(Inst::DictSet {
                    dict: Place::slot(outer),
                    key: text(&variant.name),
                    value: Operand::Move(Place::slot(inner_json)),
                });
                self.make_json(
                    dest,
                    variants.object,
                    vec![Operand::Move(Place::slot(outer))],
                );
                self.pop_scope();
            }
            self.goto(done);
            self.switch_to(next);
        }
        self.terminate(Terminator::Unreachable);
        self.switch_to(done);
    }
}
