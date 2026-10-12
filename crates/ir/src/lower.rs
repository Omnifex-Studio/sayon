//! Lowering from checked HIR to IR.
//!
//! Generic functions are instantiated once per distinct list of concrete
//! type arguments. Every lexical scope records the owned slots it must destroy;
//! normal exits, `break`/`continue`/`return`, and error edges all emit the
//! drops for the scopes they leave, innermost first, running any `finally`
//! blocks they pass through.

use crate::ir::*;
use sayon_check::hir::{self, Builtin, ErrorField, LocalId, Usage};
use sayon_check::types::{IntType, Type};
use sayon_syntax::ast::{BinaryOp, ParamMode, UnaryOp};
use sayon_syntax::{Diagnostic, Span};
use std::collections::HashMap;

pub struct LowerOutput {
    pub module: Module,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn lower(program: &hir::Program) -> LowerOutput {
    let mut lowerer = Lowerer {
        program,
        instances: HashMap::new(),
        functions: Vec::new(),
        queue: Vec::new(),
        diagnostics: Vec::new(),
        union_members: Vec::new(),
        union_tags: HashMap::new(),
    };
    let globals = program
        .constants
        .iter()
        .map(|c| Global {
            name: c.name.clone(),
            ty: c.ty.clone(),
        })
        .collect();
    let mut initializers = Vec::new();
    for id in &program.init_order {
        let constant = program.constant(*id);
        let function_id = FunctionId(lowerer.functions.len() as u32);
        lowerer.functions.push(None);
        let name = format!("<initialize {}>", constant.name);
        let function = FnLower::new(
            &mut lowerer,
            &constant.locals,
            Vec::new(),
            name,
            constant.span,
        )
        .lower_constant(&constant.value, &constant.ty);
        lowerer.functions[function_id.0 as usize] = Some(function);
        initializers.push((GlobalId(id.0), function_id));
    }
    let entry = lowerer.instance(program.entry, Vec::new());
    while let Some((func, args, id)) = lowerer.queue.pop() {
        let source = program.function(func);
        let name = instance_name(&source.name, &args);
        let function = FnLower::new(&mut lowerer, &source.locals, args, name, source.span)
            .lower_function(source);
        lowerer.functions[id.0 as usize] = Some(function);
    }
    let functions = lowerer
        .functions
        .into_iter()
        .map(|f| f.expect("lowered function"))
        .collect();
    let module = Module {
        functions,
        globals,
        initializers,
        entry,
        union_members: lowerer.union_members,
    };
    LowerOutput {
        module,
        diagnostics: lowerer.diagnostics,
    }
}

fn instance_name(name: &str, args: &[Type]) -> String {
    if args.is_empty() {
        name.to_string()
    } else {
        let args: Vec<String> = args.iter().map(Type::to_string).collect();
        format!("{name}<{}>", args.join(", "))
    }
}

struct Lowerer<'p> {
    program: &'p hir::Program,
    instances: HashMap<(hir::FuncId, Vec<Type>), FunctionId>,
    functions: Vec<Option<Function>>,
    queue: Vec<(hir::FuncId, Vec<Type>, FunctionId)>,
    diagnostics: Vec<Diagnostic>,
    union_members: Vec<Type>,
    union_tags: HashMap<Type, u32>,
}

impl<'p> Lowerer<'p> {
    /// The program-wide tag of a union member type.
    fn tag(&mut self, member: &Type) -> u32 {
        if let Some(tag) = self.union_tags.get(member) {
            return *tag;
        }
        self.union_members.push(member.clone());
        let tag = self.union_members.len() as u32;
        self.union_tags.insert(member.clone(), tag);
        tag
    }

    fn instance(&mut self, func: hir::FuncId, args: Vec<Type>) -> FunctionId {
        if let Some(id) = self.instances.get(&(func, args.clone())) {
            return *id;
        }
        let id = FunctionId(self.functions.len() as u32);
        self.functions.push(None);
        self.instances.insert((func, args.clone()), id);
        self.queue.push((func, args, id));
        id
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    Owned,
    Read,
    Mut,
}

struct Scope {
    id: u32,
    slots: Vec<SlotId>,
}

struct LoopCtx {
    break_to: BlockId,
    continue_to: BlockId,
    scope_depth: usize,
    finally_depth: usize,
}

struct HandlerCtx {
    target: BlockId,
    scope_depth: usize,
}

#[derive(Clone, Copy)]
struct FinallyCtx<'p> {
    body: &'p hir::Block,
    scope_depth: usize,
    handler_depth: usize,
}

struct FnLower<'l, 'p> {
    l: &'l mut Lowerer<'p>,
    locals: &'p [hir::Local],
    type_args: Vec<Type>,
    f: Function,
    current: BlockId,
    terminated: bool,
    local_slots: HashMap<LocalId, SlotId>,
    scopes: Vec<Scope>,
    next_scope_id: u32,
    block_scopes: Vec<usize>,
    loops: Vec<LoopCtx>,
    handlers: Vec<HandlerCtx>,
    finallies: Vec<FinallyCtx<'p>>,
    /// Set while lowering a `finally` copy on a return path: the handler depth
    /// of that `finally`, so errors escaping it also destroy the return value.
    returning: Option<usize>,
    cleanup_cache: HashMap<CleanupKey, BlockId>,
}

/// Identifies an error-cleanup block: the handler depth, whether the return
/// value is dropped, and the scopes (by identity and size) it destroys.
type CleanupKey = (usize, bool, Vec<(u32, usize)>);

impl<'l, 'p> FnLower<'l, 'p> {
    fn new(
        l: &'l mut Lowerer<'p>,
        locals: &'p [hir::Local],
        type_args: Vec<Type>,
        name: String,
        span: Span,
    ) -> Self {
        let f = Function {
            name,
            span,
            params: Vec::new(),
            required_params: 0,
            ret: Type::Nothing,
            ret_slot: SlotId(0),
            error_slot: SlotId(0),
            argc_slot: SlotId(0),
            slots: Vec::new(),
            blocks: Vec::new(),
            is_entry: false,
        };
        FnLower {
            l,
            locals,
            type_args,
            f,
            current: BlockId(0),
            terminated: false,
            local_slots: HashMap::new(),
            scopes: Vec::new(),
            next_scope_id: 0,
            block_scopes: Vec::new(),
            loops: Vec::new(),
            handlers: Vec::new(),
            finallies: Vec::new(),
            returning: None,
            cleanup_cache: HashMap::new(),
        }
    }

    // ----- infrastructure ----------------------------------------------------

    fn ty(&self, ty: &Type) -> Type {
        if self.type_args.is_empty() {
            ty.clone()
        } else {
            ty.substitute(&self.type_args)
        }
    }

    fn new_slot(&mut self, ty: Type, kind: SlotKind, name: Option<String>) -> SlotId {
        let id = SlotId(self.f.slots.len() as u32);
        self.f.slots.push(Slot { ty, kind, name });
        id
    }

    fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.f.blocks.len() as u32);
        self.f.blocks.push(Block {
            insts: Vec::new(),
            term: Terminator::Unreachable,
        });
        id
    }

    fn switch_to(&mut self, block: BlockId) {
        self.current = block;
        self.terminated = false;
    }

    fn push(&mut self, inst: Inst) {
        if !self.terminated {
            self.f.blocks[self.current.0 as usize].insts.push(inst);
        }
    }

    fn assign(&mut self, dest: Place, value: Rvalue) {
        self.push(Inst::Assign { dest, value });
    }

    fn terminate(&mut self, term: Terminator) {
        if !self.terminated {
            self.f.blocks[self.current.0 as usize].term = term;
            self.terminated = true;
        }
    }

    fn goto(&mut self, target: BlockId) {
        self.terminate(Terminator::Goto(target));
    }

    fn push_scope(&mut self) {
        let id = self.next_scope_id;
        self.next_scope_id += 1;
        self.scopes.push(Scope {
            id,
            slots: Vec::new(),
        });
    }

    fn pop_scope(&mut self) {
        let scope = self.scopes.pop().expect("scope");
        for slot in scope.slots.iter().rev() {
            self.push(Inst::Drop(Place::slot(*slot)));
        }
    }

    /// A temporary owned by the innermost scope.
    fn temp(&mut self, ty: Type) -> SlotId {
        let copy = ty.is_copy();
        let slot = self.new_slot(ty, SlotKind::Value, None);
        if !copy {
            self.scopes.last_mut().expect("scope").slots.push(slot);
        }
        slot
    }

    fn declare_local(&mut self, local: LocalId) -> SlotId {
        let info = &self.locals[local.0 as usize];
        let ty = self.ty(&info.ty);
        let copy = ty.is_copy();
        let slot = self.new_slot(ty, SlotKind::Value, Some(info.name.clone()));
        self.local_slots.insert(local, slot);
        if !copy {
            let index = *self.block_scopes.last().expect("block scope");
            self.scopes[index].slots.push(slot);
        }
        slot
    }

    fn emit_drops(&mut self, depth: usize) {
        let slots: Vec<SlotId> = self.scopes[depth..]
            .iter()
            .rev()
            .flat_map(|scope| scope.slots.iter().rev().copied())
            .collect();
        for slot in slots {
            self.push(Inst::Drop(Place::slot(slot)));
        }
    }

    /// The block that failures at this point jump to: it destroys the scopes
    /// being left and continues at the innermost handler, or returns the error.
    fn error_target(&mut self) -> BlockId {
        let handler = self.handlers.last().map(|h| (h.target, h.scope_depth));
        let depth = handler.map(|(_, depth)| depth).unwrap_or(0);
        let drop_return = self.returning.is_some_and(|d| self.handlers.len() <= d);
        let shape: Vec<(u32, usize)> = self.scopes[depth..]
            .iter()
            .map(|s| (s.id, s.slots.len()))
            .collect();
        let key = (self.handlers.len(), drop_return, shape);
        if let Some(block) = self.cleanup_cache.get(&key) {
            return *block;
        }
        let block = self.new_block();
        let slots: Vec<SlotId> = self.scopes[depth..]
            .iter()
            .rev()
            .flat_map(|scope| scope.slots.iter().rev().copied())
            .collect();
        let mut insts: Vec<Inst> = slots
            .into_iter()
            .map(|slot| Inst::Drop(Place::slot(slot)))
            .collect();
        if drop_return {
            insts.push(Inst::Drop(Place::slot(self.f.ret_slot)));
        }
        let term = match handler {
            Some((target, _)) => Terminator::Goto(target),
            None => Terminator::ReturnError,
        };
        self.f.blocks[block.0 as usize] = Block { insts, term };
        self.cleanup_cache.insert(key, block);
        block
    }

    /// Emits an operation that may fail and continues in its normal successor.
    fn op(&mut self, op: Op, dest: Option<Place>, span: Span) {
        if self.terminated {
            return;
        }
        let error = self.error_target();
        let normal = self.new_block();
        self.terminate(Terminator::Op {
            op,
            dest,
            normal,
            error,
            span,
        });
        self.switch_to(normal);
    }

    /// Leaves scopes down to `scope_depth`, running the `finally` blocks of
    /// regions down to `finally_depth`.
    fn leave(&mut self, scope_depth: usize, finally_depth: usize, returning: bool) {
        let mut index = self.finallies.len();
        while index > finally_depth {
            index -= 1;
            let finally = self.finallies[index];
            self.emit_drops(finally.scope_depth);
            let saved_finallies = self.finallies.split_off(index);
            let saved_handlers = self
                .handlers
                .split_off(finally.handler_depth.min(self.handlers.len()));
            let saved_returning = self.returning;
            if returning {
                self.returning = Some(finally.handler_depth);
            }
            self.lower_block(finally.body);
            self.returning = saved_returning;
            self.handlers.extend(saved_handlers);
            self.finallies.extend(saved_finallies);
            if self.terminated {
                return;
            }
        }
        self.emit_drops(scope_depth);
    }

    fn report(&mut self, diagnostic: Diagnostic) {
        self.l.diagnostics.push(diagnostic);
    }

    // ----- functions ------------------------------------------------------------

    fn finish_slots(&mut self, ret: Type) {
        self.f.ret = ret.clone();
        self.f.ret_slot = self.new_slot(ret, SlotKind::Value, Some("<return>".to_string()));
        self.f.error_slot =
            self.new_slot(Type::Error, SlotKind::Value, Some("<error>".to_string()));
        self.f.argc_slot =
            self.new_slot(Type::INTEGER, SlotKind::Value, Some("<argc>".to_string()));
    }

    fn lower_constant(mut self, value: &'p hir::Expr, ty: &Type) -> Function {
        self.finish_slots(ty.clone());
        self.push_scope();
        self.block_scopes.push(0);
        let entry = self.new_block();
        self.switch_to(entry);
        self.push_scope();
        let ret = Place::slot(self.f.ret_slot);
        self.expr_into(value, ret);
        self.pop_scope();
        self.leave(0, 0, true);
        self.terminate(Terminator::Return);
        self.f
    }

    fn lower_function(mut self, source: &'p hir::Function) -> Function {
        self.f.is_entry = source.is_entry;
        let mut params = Vec::new();
        let mut owned_params = Vec::new();
        for param in &source.params {
            let ty = self.ty(&param.ty);
            let reference = param.mode != ParamMode::Own && !ty.is_copy();
            let kind = if reference {
                SlotKind::Ref {
                    mutable: param.mode == ParamMode::Update,
                }
            } else {
                SlotKind::Value
            };
            let name = self.locals[param.local.0 as usize].name.clone();
            let copy = ty.is_copy();
            let slot = self.new_slot(ty, kind, Some(name));
            self.local_slots.insert(param.local, slot);
            params.push(slot);
            if !reference && !copy {
                owned_params.push(slot);
            }
        }
        self.f.params = params.clone();
        self.f.required_params = source
            .params
            .iter()
            .take_while(|p| p.default.is_none())
            .count();
        let ret = self.ty(&source.ret);
        self.finish_slots(ret.clone());

        self.push_scope();
        self.block_scopes.push(0);
        self.scopes[0].slots.extend(owned_params);
        let entry = self.new_block();
        self.switch_to(entry);

        // Defaults for parameters the caller omitted.
        for (index, param) in source.params.iter().enumerate() {
            let Some(default) = &param.default else {
                continue;
            };
            let supplied = self.new_slot(Type::Boolean, SlotKind::Value, None);
            self.assign(
                Place::slot(supplied),
                Rvalue::Compare {
                    op: CompareOp::Greater,
                    ty: Type::INTEGER,
                    lhs: Operand::Copy(Place::slot(self.f.argc_slot)),
                    rhs: Operand::Const(Constant::Int(index as i128, IntType::I64)),
                },
            );
            let compute = self.new_block();
            let next = self.new_block();
            self.terminate(Terminator::Branch {
                condition: Operand::Copy(Place::slot(supplied)),
                then_block: next,
                else_block: compute,
            });
            self.switch_to(compute);
            self.push_scope();
            let slot = params[index];
            match self.f.slots[slot.0 as usize].kind {
                SlotKind::Value => self.expr_into(default, Place::slot(slot)),
                SlotKind::Ref { mutable } => {
                    // The callee owns a default value; the parameter refers to it.
                    let ty = self.f.slots[slot.0 as usize].ty.clone();
                    let owned = self.new_slot(ty, SlotKind::Value, None);
                    self.scopes[0].slots.push(owned);
                    self.expr_into(default, Place::slot(owned));
                    self.push(Inst::Bind {
                        slot,
                        place: Place::slot(owned),
                        mutable,
                    });
                }
            }
            self.pop_scope();
            self.goto(next);
            self.switch_to(next);
        }

        self.lower_block(&source.body);
        if !self.terminated {
            if ret == Type::Nothing {
                let place = Place::slot(self.f.ret_slot);
                self.assign(place, Rvalue::Use(Operand::Const(Constant::Nothing)));
            }
            self.leave(0, 0, true);
            self.terminate(Terminator::Return);
        }
        self.f
    }

    // ----- statements -------------------------------------------------------------

    fn lower_block(&mut self, block: &'p hir::Block) {
        self.push_scope();
        self.block_scopes.push(self.scopes.len() - 1);
        for stmt in &block.stmts {
            if self.terminated {
                break;
            }
            self.push_scope();
            self.stmt(stmt);
            self.pop_scope();
        }
        self.block_scopes.pop();
        self.pop_scope();
    }

    /// Evaluates a boolean condition, destroying its temporaries before branching.
    fn condition(&mut self, condition: &'p hir::Expr) -> Operand {
        let slot = self.new_slot(Type::Boolean, SlotKind::Value, None);
        self.push_scope();
        self.expr_into(condition, Place::slot(slot));
        self.pop_scope();
        Operand::Copy(Place::slot(slot))
    }

    fn stmt(&mut self, stmt: &'p hir::Stmt) {
        use hir::StmtKind::*;
        match &stmt.kind {
            Let { local, value } => {
                let slot = self.declare_local(*local);
                self.expr_into(value, Place::slot(slot));
            }
            Assign { place, value } => self.assign_stmt(place, value),
            CompoundAssign { place, op, value } => {
                self.compound_assign(place, *op, value, stmt.span)
            }
            Expr(value) => {
                let ty = self.ty(&value.ty);
                let slot = self.temp(ty);
                self.expr_into(value, Place::slot(slot));
            }
            If {
                condition,
                then_block,
                else_block,
            } => {
                let condition = self.condition(condition);
                let then_entry = self.new_block();
                let else_entry = self.new_block();
                let join = self.new_block();
                self.terminate(Terminator::Branch {
                    condition,
                    then_block: then_entry,
                    else_block: else_entry,
                });
                self.switch_to(then_entry);
                self.lower_block(then_block);
                self.goto(join);
                self.switch_to(else_entry);
                if let Some(block) = else_block {
                    self.lower_block(block);
                }
                self.goto(join);
                self.switch_to(join);
            }
            While { condition, body } => {
                let head = self.new_block();
                let body_entry = self.new_block();
                let exit = self.new_block();
                self.goto(head);
                self.switch_to(head);
                let condition = self.condition(condition);
                self.terminate(Terminator::Branch {
                    condition,
                    then_block: body_entry,
                    else_block: exit,
                });
                self.switch_to(body_entry);
                self.loops.push(LoopCtx {
                    break_to: exit,
                    continue_to: head,
                    scope_depth: self.scopes.len(),
                    finally_depth: self.finallies.len(),
                });
                self.lower_block(body);
                self.loops.pop();
                self.goto(head);
                self.switch_to(exit);
            }
            For {
                bindings,
                iterable,
                body,
            } => self.for_loop(bindings, iterable, body),
            Break | Continue => {
                let Some(target) = self.loops.last() else {
                    return;
                };
                let (destination, depth, finally_depth) = match stmt.kind {
                    Break => (target.break_to, target.scope_depth, target.finally_depth),
                    _ => (target.continue_to, target.scope_depth, target.finally_depth),
                };
                self.leave(depth, finally_depth, false);
                self.goto(destination);
            }
            Return(value) => {
                let ret = Place::slot(self.f.ret_slot);
                match value {
                    Some(value) => self.expr_into(value, ret),
                    None => self.assign(ret, Rvalue::Use(Operand::Const(Constant::Nothing))),
                }
                self.leave(0, 0, true);
                self.terminate(Terminator::Return);
            }
            Try {
                body,
                catch,
                finally,
            } => self.try_stmt(body, catch.as_ref(), finally.as_ref()),
            Block(block) => self.lower_block(block),
        }
    }

    fn for_loop(&mut self, bindings: &[LocalId], iterable: &'p hir::Expr, body: &'p hir::Block) {
        let iterable_ty = self.ty(&iterable.ty);
        self.push_scope();
        let count = self.new_slot(Type::INTEGER, SlotKind::Value, None);
        let index = self.new_slot(Type::INTEGER, SlotKind::Value, None);
        let below = self.new_slot(Type::Boolean, SlotKind::Value, None);

        // The collection is read in place when it is a place, otherwise owned
        // by the loop for its duration.
        let collection = match &iterable_ty {
            Type::Range => {
                let range = self.new_slot(Type::Range, SlotKind::Value, None);
                self.expr_into(iterable, Place::slot(range));
                Place::slot(range)
            }
            _ => self.place_of(iterable, Want::Read),
        };
        let head = self.new_block();
        let body_entry = self.new_block();
        let step = self.new_block();
        let exit = self.new_block();
        self.assign(
            Place::slot(index),
            Rvalue::Use(Operand::Const(Constant::Int(0, IntType::I64))),
        );
        if iterable_ty == Type::Range {
            self.assign(
                Place::slot(count),
                Rvalue::RangeLength(Operand::Copy(collection.clone())),
            );
        }
        self.goto(head);

        self.switch_to(head);
        if iterable_ty != Type::Range {
            // Re-read each time; the loop's borrow keeps the length fixed.
            self.assign(
                Place::slot(count),
                Rvalue::Length {
                    value: Operand::Borrow {
                        place: collection.clone(),
                        mutable: false,
                    },
                    ty: iterable_ty.clone(),
                },
            );
        }
        self.assign(
            Place::slot(below),
            Rvalue::Compare {
                op: CompareOp::Less,
                ty: Type::INTEGER,
                lhs: Operand::Copy(Place::slot(index)),
                rhs: Operand::Copy(Place::slot(count)),
            },
        );
        self.terminate(Terminator::Branch {
            condition: Operand::Copy(Place::slot(below)),
            then_block: body_entry,
            else_block: exit,
        });

        self.switch_to(body_entry);
        let element_places: Vec<Place> = match &iterable_ty {
            Type::Array(_) => vec![collection.project(Projection::Index(index))],
            Type::Dict(..) => {
                vec![
                    collection.project(Projection::DictKey(index)),
                    collection.project(Projection::DictValue(index)),
                ]
            }
            _ => Vec::new(),
        };
        for (position, local) in bindings.iter().enumerate() {
            let info = &self.locals[local.0 as usize];
            let ty = self.ty(&info.ty);
            let name = Some(info.name.clone());
            if iterable_ty == Type::Range {
                let slot = self.new_slot(ty, SlotKind::Value, name);
                self.local_slots.insert(*local, slot);
                self.assign(
                    Place::slot(slot),
                    Rvalue::RangeAt {
                        range: Operand::Copy(collection.clone()),
                        index: Operand::Copy(Place::slot(index)),
                    },
                );
            } else if ty.is_copy() {
                let slot = self.new_slot(ty, SlotKind::Value, name);
                self.local_slots.insert(*local, slot);
                self.assign(
                    Place::slot(slot),
                    Rvalue::Use(Operand::Copy(element_places[position].clone())),
                );
            } else {
                let slot = self.new_slot(ty, SlotKind::Ref { mutable: false }, name);
                self.local_slots.insert(*local, slot);
                self.push(Inst::Bind {
                    slot,
                    place: element_places[position].clone(),
                    mutable: false,
                });
            }
        }
        self.loops.push(LoopCtx {
            break_to: exit,
            continue_to: step,
            scope_depth: self.scopes.len(),
            finally_depth: self.finallies.len(),
        });
        self.lower_block(body);
        self.loops.pop();
        self.goto(step);

        self.switch_to(step);
        self.assign(
            Place::slot(index),
            Rvalue::Increment(Operand::Copy(Place::slot(index))),
        );
        self.goto(head);

        self.switch_to(exit);
        self.pop_scope();
    }

    fn try_stmt(
        &mut self,
        body: &'p hir::Block,
        catch: Option<&'p (LocalId, hir::Block)>,
        finally: Option<&'p hir::Block>,
    ) {
        let after = self.new_block();
        let finally_depth = self.finallies.len();
        if let Some(block) = finally {
            self.finallies.push(FinallyCtx {
                body: block,
                scope_depth: self.scopes.len(),
                handler_depth: self.handlers.len(),
            });
        }

        let on_body_error = self.new_block();
        self.handlers.push(HandlerCtx {
            target: on_body_error,
            scope_depth: self.scopes.len(),
        });
        self.lower_block(body);
        self.handlers.pop();
        if !self.terminated {
            self.run_finally_inline(finally_depth);
            self.goto(after);
        }

        self.switch_to(on_body_error);
        match catch {
            Some((local, handler)) => {
                let on_handler_error = finally.map(|_| self.new_block());
                self.push_scope();
                self.block_scopes.push(self.scopes.len() - 1);
                let slot = self.declare_local(*local);
                let error = Place::slot(self.f.error_slot);
                self.assign(Place::slot(slot), Rvalue::Use(Operand::Move(error)));
                if let Some(target) = on_handler_error {
                    self.handlers.push(HandlerCtx {
                        target,
                        scope_depth: self.scopes.len() - 1,
                    });
                }
                self.lower_block(handler);
                if on_handler_error.is_some() {
                    self.handlers.pop();
                }
                self.block_scopes.pop();
                self.pop_scope();
                if !self.terminated {
                    self.run_finally_inline(finally_depth);
                    self.goto(after);
                }
                if let Some(target) = on_handler_error {
                    self.switch_to(target);
                    self.finally_then_propagate(finally_depth);
                }
            }
            None => self.finally_then_propagate(finally_depth),
        }
        self.finallies.truncate(finally_depth);
        self.switch_to(after);
    }

    /// Runs the `finally` block of the region at `depth` on a normal exit.
    fn run_finally_inline(&mut self, depth: usize) {
        if let Some(finally) = self.finallies.get(depth).copied() {
            let saved = self.finallies.split_off(depth);
            self.lower_block(finally.body);
            self.finallies.extend(saved);
        }
    }

    /// On a failure path: keeps the pending error aside, runs the `finally`
    /// block, then continues propagating the error.
    fn finally_then_propagate(&mut self, depth: usize) {
        if let Some(finally) = self.finallies.get(depth).copied() {
            self.push_scope();
            let pending = self.temp(Type::Error);
            let error = Place::slot(self.f.error_slot);
            self.assign(
                Place::slot(pending),
                Rvalue::Use(Operand::Move(error.clone())),
            );
            let saved_finallies = self.finallies.split_off(depth);
            self.lower_block(finally.body);
            self.finallies.extend(saved_finallies);
            self.assign(error, Rvalue::Use(Operand::Move(Place::slot(pending))));
            self.pop_scope();
        }
        if !self.terminated {
            let target = self.error_target();
            self.goto(target);
        }
    }

    fn assign_stmt(&mut self, place: &'p hir::Place, value: &'p hir::Expr) {
        match place {
            hir::Place::Local(local) => {
                let slot = self.local_slots[local];
                let ty = self.f.slots[slot.0 as usize].ty.clone();
                let temp = self.temp(ty.clone());
                self.expr_into(value, Place::slot(temp));
                self.push(Inst::Drop(Place::slot(slot)));
                let operand = if ty.is_copy() {
                    Operand::Copy(Place::slot(temp))
                } else {
                    Operand::Move(Place::slot(temp))
                };
                self.assign(Place::slot(slot), Rvalue::Use(operand));
            }
            hir::Place::Index {
                target,
                target_ty,
                index,
            } => {
                let target_ty = self.ty(target_ty);
                let base = self.hir_place(target);
                match target_ty {
                    Type::Dict(..) => {
                        let key = self.operand(index, Want::Read);
                        let value_ty = self.ty(&value.ty);
                        let temp = self.temp(value_ty.clone());
                        self.expr_into(value, Place::slot(temp));
                        let value = if value_ty.is_copy() {
                            Operand::Copy(Place::slot(temp))
                        } else {
                            Operand::Move(Place::slot(temp))
                        };
                        self.push(Inst::DictSet {
                            dict: base,
                            key,
                            value,
                        });
                    }
                    _ => {
                        let index_operand = self.operand(index, Want::Owned);
                        let value_ty = self.ty(&value.ty);
                        let temp = self.temp(value_ty.clone());
                        self.expr_into(value, Place::slot(temp));
                        let position = self.new_slot(Type::INTEGER, SlotKind::Value, None);
                        self.op(
                            Op::CheckIndex {
                                array: Operand::Borrow {
                                    place: base.clone(),
                                    mutable: false,
                                },
                                index: index_operand,
                            },
                            Some(Place::slot(position)),
                            index.span,
                        );
                        let element = base.project(Projection::Index(position));
                        self.push(Inst::Drop(element.clone()));
                        let value = if value_ty.is_copy() {
                            Operand::Copy(Place::slot(temp))
                        } else {
                            Operand::Move(Place::slot(temp))
                        };
                        self.assign(element, Rvalue::Use(value));
                    }
                }
            }
        }
    }

    fn compound_assign(
        &mut self,
        place: &'p hir::Place,
        op: BinaryOp,
        value: &'p hir::Expr,
        span: Span,
    ) {
        let target = self.hir_place_read(place);
        let ty = self.place_type(place);
        let rhs = self.operand(value, Want::Read);
        let current = if ty.is_copy() {
            Operand::Copy(target.clone())
        } else {
            Operand::Borrow {
                place: target.clone(),
                mutable: false,
            }
        };
        let result = self.temp(ty.clone());
        self.binary_values(op, &ty, current, rhs, Place::slot(result), span);
        self.push(Inst::Drop(target.clone()));
        let operand = if ty.is_copy() {
            Operand::Copy(Place::slot(result))
        } else {
            Operand::Move(Place::slot(result))
        };
        self.assign(target, Rvalue::Use(operand));
    }

    fn place_type(&self, place: &hir::Place) -> Type {
        match place {
            hir::Place::Local(local) => self.f.slots[self.local_slots[local].0 as usize].ty.clone(),
            hir::Place::Index { target_ty, .. } => match self.ty(target_ty) {
                Type::Array(element) => *element,
                Type::Dict(_, value) => *value,
                other => other,
            },
        }
    }

    /// The place an assignment target refers to, without the final element
    /// step (which assignment handles itself).
    fn hir_place(&mut self, place: &'p hir::Place) -> Place {
        self.hir_place_read(place)
    }

    /// Resolves a HIR place, checking every index and key on the way.
    fn hir_place_read(&mut self, place: &'p hir::Place) -> Place {
        match place {
            hir::Place::Local(local) => Place::slot(self.local_slots[local]),
            hir::Place::Index {
                target,
                target_ty,
                index,
            } => {
                let base = self.hir_place_read(target);
                let target_ty = self.ty(target_ty);
                self.element_place(base, &target_ty, index, index.span)
            }
        }
    }

    /// `base[index]`, with the bounds or key check.
    fn element_place(
        &mut self,
        base: Place,
        target_ty: &Type,
        index: &'p hir::Expr,
        span: Span,
    ) -> Place {
        let position = self.new_slot(Type::INTEGER, SlotKind::Value, None);
        match target_ty {
            Type::Dict(..) => {
                let key = self.operand(index, Want::Read);
                self.op(
                    Op::DictFind {
                        dict: Operand::Borrow {
                            place: base.clone(),
                            mutable: false,
                        },
                        key,
                    },
                    Some(Place::slot(position)),
                    span,
                );
                base.project(Projection::DictValue(position))
            }
            _ => {
                let index_operand = self.operand(index, Want::Owned);
                self.op(
                    Op::CheckIndex {
                        array: Operand::Borrow {
                            place: base.clone(),
                            mutable: false,
                        },
                        index: index_operand,
                    },
                    Some(Place::slot(position)),
                    span,
                );
                base.project(Projection::Index(position))
            }
        }
    }

    // ----- expressions --------------------------------------------------------

    /// A place holding the expression's value: the variable itself for names
    /// and elements, or a temporary for computed values.
    fn place_of(&mut self, expr: &'p hir::Expr, want: Want) -> Place {
        match &expr.kind {
            hir::ExprKind::Local { local, .. } => Place::slot(self.local_slots[local]),
            hir::ExprKind::Constant(id) => Place::global(GlobalId(id.0)),
            hir::ExprKind::Narrow(inner) => {
                if let hir::ExprKind::Local { local, .. } = &inner.kind {
                    let slot = self.local_slots[local];
                    let ty = self.ty(&expr.ty);
                    return self.narrowed_place(Place::slot(slot), &ty);
                }
                self.temp_place(expr, want)
            }
            hir::ExprKind::Index { target, index } => {
                let target_ty = self.ty(&target.ty);
                let base = self.place_of(target, Want::Read);
                self.element_place(base, &target_ty, index, expr.span)
            }
            _ => self.temp_place(expr, want),
        }
    }

    /// The place of a local narrowed to `ty`: the payload for a single member,
    /// or the union itself for a smaller union (tags are program-wide).
    fn narrowed_place(&mut self, local: Place, ty: &Type) -> Place {
        let local_ty = self.place_ty(&local);
        if local_ty == *ty || matches!(ty, Type::Union(_)) || !matches!(local_ty, Type::Union(_)) {
            return local;
        }
        let tag = self.l.tag(ty);
        local.project(Projection::Payload(tag))
    }

    fn place_ty(&self, place: &Place) -> Type {
        match &place.base {
            Base::Slot(slot) => self.f.slots[slot.0 as usize].ty.clone(),
            Base::Global(global) => self.l.program.constants[global.0 as usize].ty.clone(),
        }
    }

    fn temp_place(&mut self, expr: &'p hir::Expr, _want: Want) -> Place {
        let ty = self.ty(&expr.ty);
        let slot = self.temp(ty);
        self.expr_into(expr, Place::slot(slot));
        Place::slot(slot)
    }

    fn operand(&mut self, expr: &'p hir::Expr, want: Want) -> Operand {
        let ty = self.ty(&expr.ty);
        let usage = match &expr.kind {
            hir::ExprKind::Local { usage, .. } => Some(*usage),
            hir::ExprKind::Narrow(inner) => match &inner.kind {
                hir::ExprKind::Local { usage, .. } => Some(*usage),
                _ => None,
            },
            _ => None,
        };
        let placeable = match &expr.kind {
            hir::ExprKind::Local { .. }
            | hir::ExprKind::Constant(_)
            | hir::ExprKind::Index { .. } => true,
            hir::ExprKind::Narrow(inner) => matches!(inner.kind, hir::ExprKind::Local { .. }),
            _ => false,
        };
        if !placeable {
            let slot = self.temp(ty.clone());
            self.expr_into(expr, Place::slot(slot));
            let place = Place::slot(slot);
            return match want {
                _ if ty.is_copy() => Operand::Copy(place),
                Want::Owned => Operand::Move(place),
                Want::Read => Operand::Borrow {
                    place,
                    mutable: false,
                },
                Want::Mut => Operand::Borrow {
                    place,
                    mutable: true,
                },
            };
        }
        let place = self.place_of(expr, want);
        if ty.is_copy() {
            return Operand::Copy(place);
        }
        match (usage, want) {
            (Some(Usage::Move), _) => Operand::Move(place),
            (Some(Usage::BorrowUpdate), _) | (_, Want::Mut) => Operand::Borrow {
                place,
                mutable: true,
            },
            (_, Want::Owned) => {
                // The checker only lets owned values be consumed.
                debug_assert!(false, "owned use of a borrowed value in {}", self.f.name);
                Operand::Move(place)
            }
            _ => Operand::Borrow {
                place,
                mutable: false,
            },
        }
    }

    fn literal_int(&mut self, value: i128, ty: &Type, span: Span) -> Constant {
        match ty {
            Type::Int(int) => {
                if value < int.min_value() || value > int.max_value() {
                    self.report(
                        Diagnostic::error(
                            "E_LITERAL_RANGE",
                            format!("{value} does not fit in {}", int.name()),
                        )
                        .primary(span, format!("in the instance `{}`", self.f.name)),
                    );
                }
                Constant::Int(value, *int)
            }
            _ => Constant::Int(value, IntType::I64),
        }
    }

    /// Evaluates an expression and initializes `dest` with its value.
    fn expr_into(&mut self, expr: &'p hir::Expr, dest: Place) {
        use hir::ExprKind as H;
        let ty = self.ty(&expr.ty);
        let span = expr.span;
        match &expr.kind {
            H::Integer(value) => {
                let constant = self.literal_int(*value, &ty, span);
                self.assign(dest, Rvalue::Use(Operand::Const(constant)));
            }
            H::Float(value) => {
                let float = match ty {
                    Type::Float(float) => float,
                    _ => sayon_check::types::FloatType::F64,
                };
                self.assign(
                    dest,
                    Rvalue::Use(Operand::Const(Constant::Float(*value, float))),
                );
            }
            H::Boolean(value) => {
                self.assign(dest, Rvalue::Use(Operand::Const(Constant::Bool(*value))))
            }
            H::String(text) => self.assign(
                dest,
                Rvalue::Use(Operand::Const(Constant::Str(text.clone()))),
            ),
            H::Nothing => self.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing))),
            H::Local { .. } | H::Constant(_) | H::Index { .. } | H::Narrow(_) => {
                let operand = self.operand(expr, Want::Owned);
                self.assign(dest, Rvalue::Use(operand));
            }
            H::Call {
                function,
                type_args,
                args,
            } => {
                let instance_args: Vec<Type> = type_args.iter().map(|t| self.ty(t)).collect();
                let callee_source = self.l.program.function(*function);
                let callee = self.l.instance(*function, instance_args.clone());
                let mut operands = Vec::new();
                for (index, arg) in args.iter().enumerate() {
                    let param_ty = callee_source.params[index].ty.substitute(&instance_args);
                    let reference = arg.mode != ParamMode::Own && !param_ty.is_copy();
                    let want = if !reference {
                        Want::Owned
                    } else if arg.mode == ParamMode::Update {
                        Want::Mut
                    } else {
                        Want::Read
                    };
                    operands.push(self.operand(&arg.expr, want));
                }
                self.op(
                    Op::Call {
                        function: callee,
                        args: operands,
                    },
                    Some(dest),
                    span,
                );
            }
            H::Builtin { builtin, args } => self.builtin(*builtin, args, &ty, dest, span),
            H::Binary {
                op: op @ (BinaryOp::And | BinaryOp::Or),
                lhs,
                rhs,
            } => {
                self.expr_into(lhs, dest.clone());
                let evaluate_rhs = self.new_block();
                let join = self.new_block();
                let condition = Operand::Copy(dest.clone());
                let (then_block, else_block) = if *op == BinaryOp::And {
                    (evaluate_rhs, join)
                } else {
                    (join, evaluate_rhs)
                };
                self.terminate(Terminator::Branch {
                    condition,
                    then_block,
                    else_block,
                });
                self.switch_to(evaluate_rhs);
                self.push_scope();
                self.expr_into(rhs, dest);
                self.pop_scope();
                self.goto(join);
                self.switch_to(join);
            }
            H::Binary { op, lhs, rhs } => {
                let lhs_ty = self.ty(&lhs.ty);
                let rhs_ty = self.ty(&rhs.ty);
                if op.is_comparison()
                    && matches!(op, BinaryOp::Equal | BinaryOp::NotEqual)
                    && lhs_ty != rhs_ty
                {
                    let (union, member) = if matches!(lhs_ty, Type::Union(_)) {
                        (lhs, rhs)
                    } else {
                        (rhs, lhs)
                    };
                    self.member_equality(union, member, *op == BinaryOp::NotEqual, dest);
                    return;
                }
                let left = self.operand(lhs, Want::Read);
                let right = self.operand(rhs, Want::Read);
                self.binary_values(*op, &lhs_ty, left, right, dest, span);
            }
            H::Unary {
                op: UnaryOp::Not,
                operand,
            } => {
                let value = self.operand(operand, Want::Owned);
                self.assign(dest, Rvalue::Not(value));
            }
            H::Unary {
                op: UnaryOp::Negate,
                operand,
            } => {
                let value = self.operand(operand, Want::Owned);
                match ty {
                    Type::Int(int) => self.op(Op::IntNegate { ty: int, value }, Some(dest), span),
                    _ => self.assign(dest, Rvalue::FloatNegate(value)),
                }
            }
            H::Array(items) => {
                if let Type::Dict(_, value) = &ty {
                    self.assign(
                        dest,
                        Rvalue::MakeDict {
                            value: (**value).clone(),
                            entries: Vec::new(),
                        },
                    );
                    return;
                }
                let element = match &ty {
                    Type::Array(element) => (**element).clone(),
                    _ => Type::Unknown,
                };
                let operands = items
                    .iter()
                    .map(|item| self.operand(item, Want::Owned))
                    .collect();
                self.assign(
                    dest,
                    Rvalue::MakeArray {
                        element,
                        items: operands,
                    },
                );
            }
            H::Dict(entries) => {
                let value_ty = match &ty {
                    Type::Dict(_, value) => (**value).clone(),
                    _ => Type::Unknown,
                };
                let operands = entries
                    .iter()
                    .map(|(key, value)| {
                        (
                            self.operand(key, Want::Owned),
                            self.operand(value, Want::Owned),
                        )
                    })
                    .collect();
                self.assign(
                    dest,
                    Rvalue::MakeDict {
                        value: value_ty,
                        entries: operands,
                    },
                );
            }
            H::Interpolate(segments) => {
                let parts = segments
                    .iter()
                    .map(|segment| match segment {
                        hir::Segment::Text(text) => FormatPart::Text(text.clone()),
                        hir::Segment::Value(value) => {
                            let value_ty = self.ty(&value.ty);
                            FormatPart::Value(self.operand(value, Want::Read), value_ty)
                        }
                    })
                    .collect();
                self.assign(dest, Rvalue::Format(parts));
            }
            H::Inject(inner) => {
                let inner_ty = self.ty(&inner.ty);
                let value = self.operand(inner, Want::Owned);
                if inner_ty == ty
                    || matches!(inner_ty, Type::Union(_))
                    || !matches!(ty, Type::Union(_))
                {
                    // Program-wide tags make a smaller union a valid larger one.
                    self.assign(dest, Rvalue::Use(value));
                } else {
                    let tag = self.l.tag(&inner_ty);
                    self.assign(dest, Rvalue::MakeUnion { tag, value });
                }
            }
            H::IsMember { value, member } => {
                let value_ty = self.ty(&value.ty);
                let member = self.ty(member);
                if matches!(value_ty, Type::Union(_)) && value_ty.union_members().contains(&member)
                {
                    let tag = self.l.tag(&member);
                    let operand = self.operand(value, Want::Read);
                    self.assign(
                        dest,
                        Rvalue::TagIs {
                            value: operand,
                            tag,
                        },
                    );
                } else {
                    let _ = self.operand(value, Want::Read);
                    self.assign(
                        dest,
                        Rvalue::Use(Operand::Const(Constant::Bool(value_ty == member))),
                    );
                }
            }
            H::UnionTypeName(value) => {
                let operand = self.operand(value, Want::Read);
                self.assign(dest, Rvalue::UnionTypeName(operand));
            }
            H::ErrorField { value, field } => {
                let operand = self.operand(value, Want::Read);
                let rvalue = match field {
                    ErrorField::Message => Rvalue::ErrorMessage(operand),
                    ErrorField::Code => Rvalue::ErrorCode(operand),
                };
                self.assign(dest, rvalue);
            }
        }
    }

    /// `union == member`: true when the union holds that member with an equal value.
    fn member_equality(
        &mut self,
        union: &'p hir::Expr,
        member: &'p hir::Expr,
        negate: bool,
        dest: Place,
    ) {
        let union_ty = self.ty(&union.ty);
        let member_ty = self.ty(&member.ty);
        let union_place = self.place_of(union, Want::Read);
        let member_operand = self.operand(member, Want::Read);
        if !union_ty.union_members().contains(&member_ty) {
            self.assign(dest, Rvalue::Use(Operand::Const(Constant::Bool(negate))));
            return;
        }
        let tag = self.l.tag(&member_ty);
        let union_operand = if union_ty.is_copy() {
            Operand::Copy(union_place.clone())
        } else {
            Operand::Borrow {
                place: union_place.clone(),
                mutable: false,
            }
        };
        let holds = self.new_slot(Type::Boolean, SlotKind::Value, None);
        self.assign(
            Place::slot(holds),
            Rvalue::TagIs {
                value: union_operand,
                tag,
            },
        );
        let compare = self.new_block();
        let join = self.new_block();
        self.assign(
            dest.clone(),
            Rvalue::Use(Operand::Const(Constant::Bool(false))),
        );
        self.terminate(Terminator::Branch {
            condition: Operand::Copy(Place::slot(holds)),
            then_block: compare,
            else_block: join,
        });
        self.switch_to(compare);
        let payload = union_place.project(Projection::Payload(tag));
        let payload_operand = if member_ty.is_copy() {
            Operand::Copy(payload)
        } else {
            Operand::Borrow {
                place: payload,
                mutable: false,
            }
        };
        self.assign(
            dest.clone(),
            Rvalue::Compare {
                op: CompareOp::Equal,
                ty: member_ty,
                lhs: payload_operand,
                rhs: member_operand,
            },
        );
        self.goto(join);
        self.switch_to(join);
        if negate {
            let result = self.new_slot(Type::Boolean, SlotKind::Value, None);
            self.assign(
                Place::slot(result),
                Rvalue::Use(Operand::Copy(dest.clone())),
            );
            self.assign(dest, Rvalue::Not(Operand::Copy(Place::slot(result))));
        }
    }

    fn binary_values(
        &mut self,
        op: BinaryOp,
        ty: &Type,
        lhs: Operand,
        rhs: Operand,
        dest: Place,
        span: Span,
    ) {
        let compare = |op| Rvalue::Compare {
            op,
            ty: ty.clone(),
            lhs: lhs.clone(),
            rhs: rhs.clone(),
        };
        let arith = match op {
            BinaryOp::Equal => return self.assign(dest, compare(CompareOp::Equal)),
            BinaryOp::NotEqual => return self.assign(dest, compare(CompareOp::NotEqual)),
            BinaryOp::Less => return self.assign(dest, compare(CompareOp::Less)),
            BinaryOp::Greater => return self.assign(dest, compare(CompareOp::Greater)),
            BinaryOp::LessEqual => return self.assign(dest, compare(CompareOp::LessEqual)),
            BinaryOp::GreaterEqual => return self.assign(dest, compare(CompareOp::GreaterEqual)),
            BinaryOp::Add => ArithOp::Add,
            BinaryOp::Subtract => ArithOp::Subtract,
            BinaryOp::Multiply => ArithOp::Multiply,
            BinaryOp::Divide => ArithOp::Divide,
            BinaryOp::Remainder => ArithOp::Remainder,
            BinaryOp::And | BinaryOp::Or => {
                unreachable!("short-circuit operators are lowered separately")
            }
        };
        match ty {
            Type::Int(int) => self.op(
                Op::IntArith {
                    op: arith,
                    ty: *int,
                    lhs,
                    rhs,
                },
                Some(dest),
                span,
            ),
            Type::String => self.assign(dest, Rvalue::Concat(lhs, rhs)),
            _ => self.assign(dest, Rvalue::FloatArith(arith, lhs, rhs)),
        }
    }

    fn builtin(
        &mut self,
        builtin: Builtin,
        args: &'p [hir::Arg],
        ty: &Type,
        dest: Place,
        span: Span,
    ) {
        let nothing = |this: &mut Self, dest: Place| {
            this.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing)));
        };
        match builtin {
            Builtin::Print => {
                let values = args
                    .iter()
                    .map(|arg| {
                        let arg_ty = self.ty(&arg.expr.ty);
                        (self.operand(&arg.expr, Want::Read), arg_ty)
                    })
                    .collect();
                self.op(Op::Print(values), None, span);
                nothing(self, dest);
            }
            Builtin::Length => {
                let arg_ty = self.ty(&args[0].expr.ty);
                let value = self.operand(&args[0].expr, Want::Read);
                self.assign(dest, Rvalue::Length { value, ty: arg_ty });
            }
            Builtin::ByteLength => {
                let value = self.operand(&args[0].expr, Want::Read);
                self.assign(dest, Rvalue::ByteLength(value));
            }
            Builtin::TypeOf => {
                let arg_ty = self.ty(&args[0].expr.ty);
                let value = self.operand(&args[0].expr, Want::Read);
                if matches!(arg_ty, Type::Union(_)) {
                    self.assign(dest, Rvalue::UnionTypeName(value));
                } else {
                    self.assign(
                        dest,
                        Rvalue::Use(Operand::Const(Constant::Str(arg_ty.type_name()))),
                    );
                }
            }
            Builtin::ToString => {
                let arg_ty = self.ty(&args[0].expr.ty);
                let value = self.operand(&args[0].expr, Want::Read);
                self.assign(dest, Rvalue::Format(vec![FormatPart::Value(value, arg_ty)]));
            }
            Builtin::Assert => {
                let condition = self.operand(&args[0].expr, Want::Owned);
                let message = args.get(1).map(|arg| self.operand(&arg.expr, Want::Read));
                self.op(Op::Assert { condition, message }, None, span);
                nothing(self, dest);
            }
            Builtin::Range => {
                let mut operands: Vec<Operand> = args
                    .iter()
                    .map(|a| self.operand(&a.expr, Want::Owned))
                    .collect();
                let (start, end, step) = match operands.len() {
                    1 => (
                        Operand::Const(Constant::Int(0, IntType::I64)),
                        operands.remove(0),
                        None,
                    ),
                    2 => {
                        let end = operands.remove(1);
                        (operands.remove(0), end, None)
                    }
                    _ => {
                        let step = operands.remove(2);
                        let end = operands.remove(1);
                        (operands.remove(0), end, Some(step))
                    }
                };
                self.op(Op::MakeRange { start, end, step }, Some(dest), span);
            }
            Builtin::ToNumber => {
                let arg_ty = self.ty(&args[0].expr.ty);
                if arg_ty.is_numeric() {
                    let value = self.operand(&args[0].expr, Want::Owned);
                    self.assign(dest, Rvalue::Use(value));
                } else {
                    let text = self.operand(&args[0].expr, Want::Read);
                    let integer_tag = self.l.tag(&Type::INTEGER);
                    let float_tag = self.l.tag(&Type::FLOAT);
                    self.op(
                        Op::ToNumber {
                            text,
                            integer_tag,
                            float_tag,
                        },
                        Some(dest),
                        span,
                    );
                }
            }
            Builtin::Push => {
                let array = self.place_of(&args[0].expr, Want::Mut);
                let value = self.operand(&args[1].expr, Want::Owned);
                self.push(Inst::Push { array, value });
                nothing(self, dest);
            }
            Builtin::Pop => {
                let element = match self.ty(&args[0].expr.ty) {
                    Type::Array(element) => *element,
                    other => other,
                };
                let wraps = matches!(ty, Type::Union(_));
                let element_tag =
                    (wraps && !matches!(element, Type::Union(_))).then(|| self.l.tag(&element));
                let nothing_tag = wraps.then(|| self.l.tag(&Type::Nothing));
                let array = self.place_of(&args[0].expr, Want::Mut);
                self.assign(
                    dest,
                    Rvalue::Pop {
                        array,
                        element_tag,
                        nothing_tag,
                    },
                );
            }
            Builtin::HasKey => {
                let dict = self.operand(&args[0].expr, Want::Read);
                let key = self.operand(&args[1].expr, Want::Read);
                self.assign(dest, Rvalue::HasKey { dict, key });
            }
            Builtin::ReadFile => {
                let path = self.operand(&args[0].expr, Want::Read);
                self.op(Op::ReadFile(path), Some(dest), span);
            }
            Builtin::WriteFile => {
                let path = self.operand(&args[0].expr, Want::Read);
                let content = self.operand(&args[1].expr, Want::Read);
                self.op(Op::WriteFile { path, content }, None, span);
                nothing(self, dest);
            }
            Builtin::Clone => {
                let arg_ty = self.ty(&args[0].expr.ty);
                let value = self.operand(&args[0].expr, Want::Read);
                self.assign(dest, Rvalue::Clone { value, ty: arg_ty });
            }
            Builtin::Error => {
                let message = self.operand(&args[0].expr, Want::Read);
                self.op(Op::Fail(message), None, span);
                self.terminate(Terminator::Unreachable);
            }
            Builtin::Raise => {
                let problem = self.operand(&args[0].expr, Want::Owned);
                self.op(Op::Raise(problem), None, span);
                self.terminate(Terminator::Unreachable);
            }
            Builtin::Host(function) => {
                let operands = args
                    .iter()
                    .map(|arg| self.operand(&arg.expr, Want::Read))
                    .collect();
                let tags = (function.result() == sayon_runtime::host::HostType::OptionalString)
                    .then(|| (self.l.tag(&Type::String), self.l.tag(&Type::Nothing)));
                self.op(
                    Op::Host {
                        function,
                        args: operands,
                        tags,
                    },
                    Some(dest),
                    span,
                );
            }
            Builtin::Convert => {
                let from = self.ty(&args[0].expr.ty);
                let value = self.operand(&args[0].expr, Want::Owned);
                self.op(
                    Op::Convert {
                        from,
                        to: ty.clone(),
                        value,
                    },
                    Some(dest),
                    span,
                );
            }
        }
    }
}
