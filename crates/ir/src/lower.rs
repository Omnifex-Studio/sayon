//! Lowering from checked HIR to IR.
//!
//! Generic functions are instantiated once per distinct list of concrete
//! type arguments. Every lexical scope records the owned slots it must destroy;
//! normal exits, `break`/`continue`/`return`, and error edges all emit the
//! drops for the scopes they leave, innermost first, running any `finally`
//! blocks they pass through.

use crate::ir::*;
use sayon_check::hir::{self, Builtin, ErrorField, LocalId, Usage};
use sayon_check::types::{
    FieldDef, IntType, NamedType, Type, TypeDef, TypeDefKind, TypeId, Wrapper,
};
use sayon_runtime::host::{CANCELLED_CODE, EXIT_CODE};
use sayon_syntax::ast::{BinaryOp, ParamMode, UnaryOp};
use sayon_syntax::{Diagnostic, Span};
use std::collections::{HashMap, HashSet};

/// How deeply the type arguments of a function's instance may nest. A
/// generic function that calls itself, or calls itself through others,
/// with a type argument that grows each time would otherwise be
/// instantiated without end.
const SPECIALIZATION_DEPTH: usize = 32;

/// How deeply a type nests: 1 for a type made of no other types.
fn type_depth(ty: &Type) -> usize {
    let inner = match ty {
        Type::Array(element) | Type::View(element, _) | Type::Wrapper(_, element) => {
            type_depth(element)
        }
        Type::Dict(key, value) => type_depth(key).max(type_depth(value)),
        Type::Union(members) => members.iter().map(type_depth).max().unwrap_or(0),
        Type::Named(named) => named.args.iter().map(type_depth).max().unwrap_or(0),
        Type::Function(function) => function.types().map(type_depth).max().unwrap_or(0),
        _ => 0,
    };
    inner + 1
}

mod json;

pub struct LowerOutput {
    pub module: Module,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn lower(program: &hir::Program) -> LowerOutput {
    lower_program(program, false)
}

/// Lowers a program together with its test blocks.
pub fn lower_with_tests(program: &hir::Program) -> LowerOutput {
    lower_program(program, true)
}

fn lower_program(program: &hir::Program, include_tests: bool) -> LowerOutput {
    let mut lowerer = Lowerer {
        program,
        instances: HashMap::new(),
        functions: Vec::new(),
        queue: Vec::new(),
        diagnostics: Vec::new(),
        union_members: Vec::new(),
        union_tags: HashMap::new(),
        types: program.types.clone(),
        envs: HashMap::new(),
        thunks: HashMap::new(),
        decoders: HashMap::new(),
        encoders: HashMap::new(),
        unbounded: HashSet::new(),
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
    let tests = if include_tests {
        program
            .tests
            .iter()
            .map(|(name, id)| (name.clone(), lowerer.instance(*id, Vec::new())))
            .collect()
    } else {
        Vec::new()
    };
    while let Some((func, args, id)) = lowerer.queue.pop() {
        let source = program.function(func);
        let name = instance_name(&source.name, &args);
        let env = if source.closure {
            lowerer.env_type(func, &args)
        } else {
            None
        };
        let function = FnLower::new(&mut lowerer, &source.locals, args, name, source.span)
            .lower_function(source, env);
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
        tests,
        types: lowerer.types,
        foreign: program.foreign.clone(),
    };
    LowerOutput {
        module,
        diagnostics: lowerer.diagnostics,
    }
}

/// Every way to pick one alternative of each `or` pattern met along the
/// way, in the order matching tries them. Each list gives the chosen
/// alternative of each `or`, in the order a left-to-right walk reaches them.
fn choice_lists(pattern: &hir::Pattern) -> Vec<Vec<usize>> {
    let sequence = |patterns: &[hir::Pattern]| {
        patterns.iter().fold(vec![Vec::new()], |lists, pattern| {
            let parts = choice_lists(pattern);
            lists
                .iter()
                .flat_map(|list| {
                    parts.iter().map(move |part| {
                        let mut joined = list.clone();
                        joined.extend_from_slice(part);
                        joined
                    })
                })
                .collect()
        })
    };
    match pattern {
        hir::Pattern::Wildcard | hir::Pattern::Binding(_) | hir::Pattern::Literal(_) => {
            vec![Vec::new()]
        }
        hir::Pattern::Variant { fields, .. } | hir::Pattern::Record { fields, .. } => {
            sequence(fields)
        }
        hir::Pattern::Member { inner, .. } => choice_lists(inner),
        hir::Pattern::Or(alternatives) => alternatives
            .iter()
            .enumerate()
            .flat_map(|(index, alternative)| {
                choice_lists(alternative).into_iter().map(move |rest| {
                    let mut list = vec![index];
                    list.extend(rest);
                    list
                })
            })
            .collect(),
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
    /// The program's structures and enums, then closure environments.
    types: Vec<TypeDef>,
    envs: HashMap<(hir::FuncId, Vec<Type>), Option<Type>>,
    /// The code behind each named function used as a value.
    thunks: HashMap<(hir::FuncId, Vec<Type>), FunctionId>,
    /// The generated functions reading each type from JSON, and writing it.
    decoders: HashMap<Type, FunctionId>,
    encoders: HashMap<Type, FunctionId>,
    /// The functions reported for type arguments that grow without end.
    unbounded: HashSet<hir::FuncId>,
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
        if args.iter().map(type_depth).max().unwrap_or(0) > SPECIALIZATION_DEPTH {
            let source = self.program.function(func);
            if self.unbounded.insert(func) {
                self.diagnostics.push(
                    Diagnostic::error(
                        "E_SPECIALIZATION_LIMIT",
                        format!("`{}` would be specialized without end", source.name),
                    )
                    .primary(
                        source.span,
                        format!(
                            "each call gives it a larger type argument, past {SPECIALIZATION_DEPTH} levels"
                        ),
                    )
                    .help("give the calls the same type arguments the function has, or hold the growing part in an enum or a union"),
                );
            }
            // Another instance stands in, so that lowering ends; the
            // program is not run.
            if let Some(id) = self
                .instances
                .iter()
                .find(|((other, _), _)| *other == func)
                .map(|(_, id)| *id)
            {
                return id;
            }
        }
        let id = FunctionId(self.functions.len() as u32);
        self.functions.push(None);
        self.instances.insert((func, args.clone()), id);
        self.queue.push((func, args, id));
        id
    }

    /// The environment structure of a closure instance: one field per
    /// captured variable. A closure that captures nothing has none.
    fn env_type(&mut self, func: hir::FuncId, args: &[Type]) -> Option<Type> {
        let key = (func, args.to_vec());
        if let Some(env) = self.envs.get(&key) {
            return env.clone();
        }
        let source = self.program.function(func);
        let env = (!source.captures.is_empty()).then(|| {
            let fields = source
                .captures
                .iter()
                .map(|local| {
                    let info = &source.locals[local.0 as usize];
                    FieldDef {
                        name: info.name.clone(),
                        ty: if args.is_empty() {
                            info.ty.clone()
                        } else {
                            info.ty.substitute(args)
                        },
                    }
                })
                .collect();
            let id = TypeId(self.types.len() as u32);
            let name = format!("<environment of {}>", instance_name(&source.name, args));
            self.types.push(TypeDef {
                name: name.clone(),
                generics: 0,
                kind: TypeDefKind::Structure(fields),
                copy: false,
            });
            Type::named(NamedType {
                id,
                name,
                args: Vec::new(),
                copy: false,
            })
        });
        self.envs.insert(key, env.clone());
        env
    }

    /// The code a named function's value calls.
    fn thunk(&mut self, func: hir::FuncId, args: Vec<Type>) -> FunctionId {
        let key = (func, args.clone());
        if let Some(id) = self.thunks.get(&key) {
            return *id;
        }
        let callee = self.instance(func, args.clone());
        let id = FunctionId(self.functions.len() as u32);
        self.functions.push(None);
        self.thunks.insert(key, id);
        let program = self.program;
        let source = program.function(func);
        let name = format!("{} as a value", instance_name(&source.name, &args));
        let function =
            FnLower::new(self, &source.locals, args, name, source.span).lower_thunk(callee, source);
        self.functions[id.0 as usize] = Some(function);
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

    /// The code behind a named function's value: it takes the (empty)
    /// environment first and passes its arguments on.
    fn lower_thunk(mut self, callee: FunctionId, source: &'p hir::Function) -> Function {
        let env = self.new_slot(
            Type::Nothing,
            SlotKind::Ref { mutable: true },
            Some("<environment>".to_string()),
        );
        let mut params = vec![env];
        let mut args = Vec::new();
        for param in &source.params {
            let ty = self.ty(&param.ty);
            let name = self.locals[param.local.0 as usize].name.clone();
            let mutable = param.mode == ParamMode::Update;
            let reference = by_reference(param.mode, &ty);
            let copy = ty.is_copy();
            let kind = if reference {
                SlotKind::Ref { mutable }
            } else {
                SlotKind::Value
            };
            let slot = self.new_slot(ty, kind, Some(name));
            params.push(slot);
            let place = Place::slot(slot);
            args.push(if reference {
                Operand::Borrow { place, mutable }
            } else if copy {
                Operand::Copy(place)
            } else {
                Operand::Move(place)
            });
        }
        self.f.required_params = params.len();
        self.f.params = params;
        let ret = self.ty(&source.ret);
        self.finish_slots(ret);
        self.push_scope();
        let entry = self.new_block();
        self.switch_to(entry);
        let ret_place = Place::slot(self.f.ret_slot);
        self.op(
            Op::Call {
                function: callee,
                args,
            },
            Some(ret_place),
            source.span,
        );
        self.terminate(Terminator::Return);
        self.f
    }

    /// Lowers a function body. A closure takes a reference to its
    /// environment first, and its captured variables refer into it.
    fn lower_function(mut self, source: &'p hir::Function, env: Option<Type>) -> Function {
        self.f.is_entry = source.is_entry;
        let mut params = Vec::new();
        let mut owned_params = Vec::new();
        let env_slot = source.closure.then(|| {
            let ty = env.unwrap_or(Type::Nothing);
            let slot = self.new_slot(
                ty,
                SlotKind::Ref { mutable: true },
                Some("<environment>".to_string()),
            );
            params.push(slot);
            slot
        });
        let first_param = params.len();
        for param in &source.params {
            let ty = self.ty(&param.ty);
            let reference = by_reference(param.mode, &ty);
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
        self.f.required_params = first_param
            + source
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
        if let Some(env_slot) = env_slot {
            for (field, local) in source.captures.iter().enumerate() {
                let info = &self.locals[local.0 as usize];
                let ty = self.ty(&info.ty);
                let name = info.name.clone();
                let slot = self.new_slot(ty, SlotKind::Ref { mutable: true }, Some(name));
                self.local_slots.insert(*local, slot);
                self.push(Inst::Bind {
                    slot,
                    place: Place::slot(env_slot).project(Projection::Field(field as u32)),
                    mutable: true,
                });
            }
        }

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
            let slot = params[first_param + index];
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
                next: Some(next),
            } => self.iterator_loop(bindings, iterable, body, next),
            For {
                bindings,
                iterable,
                body,
                next: None,
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
            Match {
                scrutinee,
                owned,
                arms,
            } => self.match_stmt(scrutinee, *owned, arms),
        }
    }

    /// Tests each arm's pattern in order; the first that matches (and whose
    /// guard holds) binds its names and runs. A match that owns its value
    /// keeps it in a temporary: unguarded arms move their bindings out of it,
    /// and whatever remains is destroyed when the match ends.
    fn match_stmt(&mut self, scrutinee: &'p hir::Expr, owned: bool, arms: &'p [hir::MatchArm]) {
        self.push_scope();
        let ty = self.ty(&scrutinee.ty);
        let copy = ty.is_copy();
        let place = if owned || copy {
            let slot = self.temp(ty.clone());
            self.expr_into(scrutinee, Place::slot(slot));
            Place::slot(slot)
        } else {
            self.place_of(scrutinee, Want::Read)
        };
        let consuming = owned && !copy;
        let end = self.new_block();
        // An arm with `or` alternatives is lowered once for each combination
        // of alternatives, in order, so a failed guard tries the next one.
        let expanded = arms.iter().flat_map(|arm| {
            choice_lists(&arm.pattern)
                .into_iter()
                .map(move |choices| (arm, choices))
        });
        for (arm, choices) in expanded {
            if self.terminated {
                break;
            }
            let next = self.new_block();
            self.test_pattern(&place, &ty, &arm.pattern, next, &mut choices.iter());
            self.push_scope();
            self.block_scopes.push(self.scopes.len() - 1);
            let take = consuming && arm.guard.is_none();
            self.bind_pattern(&place, &ty, &arm.pattern, take, &mut choices.iter());
            if let Some(guard) = &arm.guard {
                // Guarded bindings only copy or borrow, so failing leaves
                // nothing to clean up.
                let condition = self.condition(guard);
                let body = self.new_block();
                self.terminate(Terminator::Branch {
                    condition,
                    then_block: body,
                    else_block: next,
                });
                self.switch_to(body);
            }
            self.lower_block(&arm.body);
            self.block_scopes.pop();
            self.pop_scope();
            self.goto(end);
            self.switch_to(next);
        }
        // Exhaustiveness checking makes this unreachable.
        self.terminate(Terminator::Unreachable);
        self.switch_to(end);
        self.pop_scope();
    }

    /// Branches to `fail` unless the value at `place` matches `pattern`,
    /// taking the alternative of each `or` pattern from `choices`.
    fn test_pattern(
        &mut self,
        place: &Place,
        ty: &Type,
        pattern: &'p hir::Pattern,
        fail: BlockId,
        choices: &mut std::slice::Iter<usize>,
    ) {
        match pattern {
            hir::Pattern::Wildcard | hir::Pattern::Binding(_) => {}
            hir::Pattern::Or(alternatives) => {
                let choice = *choices.next().expect("a choice for each alternative");
                self.test_pattern(place, ty, &alternatives[choice], fail, choices);
            }
            hir::Pattern::Record { fields, .. } => {
                let field_types = self.record_field_types(ty);
                for (index, (field, field_ty)) in fields.iter().zip(&field_types).enumerate() {
                    let inner = place.project(Projection::Field(index as u32));
                    self.test_pattern(&inner, field_ty, field, fail, choices);
                }
            }
            hir::Pattern::Literal(literal) => {
                let flag = self.new_slot(Type::Boolean, SlotKind::Value, None);
                let lhs = self.read_operand(place, ty);
                self.push_scope();
                let rhs = self.operand(literal, Want::Read);
                self.assign(
                    Place::slot(flag),
                    Rvalue::Compare {
                        op: CompareOp::Equal,
                        ty: ty.clone(),
                        lhs,
                        rhs,
                    },
                );
                self.pop_scope();
                self.branch_unless(flag, fail);
            }
            hir::Pattern::Variant {
                variant, fields, ..
            } => {
                let flag = self.new_slot(Type::Boolean, SlotKind::Value, None);
                let value = self.read_operand(place, ty);
                self.assign(
                    Place::slot(flag),
                    Rvalue::VariantIs {
                        value,
                        variant: *variant,
                    },
                );
                self.branch_unless(flag, fail);
                let field_types = self.variant_field_types(ty, *variant);
                for (index, (field, field_ty)) in fields.iter().zip(&field_types).enumerate() {
                    let inner = place.project(Projection::VariantField {
                        variant: *variant,
                        field: index as u32,
                    });
                    self.test_pattern(&inner, field_ty, field, fail, choices);
                }
            }
            hir::Pattern::Member { member, inner } => {
                let member = self.ty(member);
                let tag = self.l.tag(&member);
                let flag = self.new_slot(Type::Boolean, SlotKind::Value, None);
                let value = self.read_operand(place, ty);
                self.assign(Place::slot(flag), Rvalue::TagIs { value, tag });
                self.branch_unless(flag, fail);
                let payload = place.project(Projection::Payload(tag));
                self.test_pattern(&payload, &member, inner, fail, choices);
            }
        }
    }

    /// Binds a pattern's names to the parts of the value at `place`: Copy
    /// values are copied; others are moved out when `take` is set, and
    /// borrowed otherwise.
    fn bind_pattern(
        &mut self,
        place: &Place,
        ty: &Type,
        pattern: &'p hir::Pattern,
        take: bool,
        choices: &mut std::slice::Iter<usize>,
    ) {
        match pattern {
            hir::Pattern::Wildcard | hir::Pattern::Literal(_) => {}
            hir::Pattern::Or(alternatives) => {
                let choice = *choices.next().expect("a choice for each alternative");
                self.bind_pattern(place, ty, &alternatives[choice], take, choices);
            }
            hir::Pattern::Record { fields, .. } => {
                let field_types = self.record_field_types(ty);
                for (index, (field, field_ty)) in fields.iter().zip(&field_types).enumerate() {
                    let inner = place.project(Projection::Field(index as u32));
                    self.bind_pattern(&inner, field_ty, field, take, choices);
                }
            }
            hir::Pattern::Binding(local) => {
                let info = &self.locals[local.0 as usize];
                let local_ty = self.ty(&info.ty);
                let name = Some(info.name.clone());
                let source = self.narrow(place.clone(), ty, &local_ty);
                if local_ty.is_copy() {
                    let slot = self.new_slot(local_ty, SlotKind::Value, name);
                    self.local_slots.insert(*local, slot);
                    self.assign(Place::slot(slot), Rvalue::Use(Operand::Copy(source)));
                } else if take {
                    let slot = self.declare_local(*local);
                    self.assign(Place::slot(slot), Rvalue::Use(Operand::Move(source)));
                } else {
                    let slot = self.new_slot(local_ty, SlotKind::Ref { mutable: false }, name);
                    self.local_slots.insert(*local, slot);
                    self.push(Inst::Bind {
                        slot,
                        place: source,
                        mutable: false,
                    });
                }
            }
            hir::Pattern::Variant {
                variant, fields, ..
            } => {
                let field_types = self.variant_field_types(ty, *variant);
                for (index, (field, field_ty)) in fields.iter().zip(&field_types).enumerate() {
                    let inner = place.project(Projection::VariantField {
                        variant: *variant,
                        field: index as u32,
                    });
                    self.bind_pattern(&inner, field_ty, field, take, choices);
                }
            }
            hir::Pattern::Member { member, inner } => {
                let member = self.ty(member);
                let tag = self.l.tag(&member);
                let payload = place.project(Projection::Payload(tag));
                self.bind_pattern(&payload, &member, inner, take, choices);
            }
        }
    }

    fn record_field_types(&self, ty: &Type) -> Vec<Type> {
        match ty {
            Type::Named(named) => {
                sayon_check::types::structure_fields(&self.l.program.types, named)
            }
            _ => Vec::new(),
        }
    }

    fn variant_field_types(&self, ty: &Type, variant: u32) -> Vec<Type> {
        match ty {
            Type::Named(named) => {
                sayon_check::types::variant_fields(&self.l.program.types, named, variant)
            }
            _ => Vec::new(),
        }
    }

    /// An operand that reads the value at `place` without taking it.
    fn read_operand(&self, place: &Place, ty: &Type) -> Operand {
        if ty.is_copy() {
            Operand::Copy(place.clone())
        } else {
            Operand::Borrow {
                place: place.clone(),
                mutable: false,
            }
        }
    }

    /// Continues in a new block when the flag is true, else jumps to `fail`.
    fn branch_unless(&mut self, flag: SlotId, fail: BlockId) {
        let next = self.new_block();
        self.terminate(Terminator::Branch {
            condition: Operand::Copy(Place::slot(flag)),
            then_block: next,
            else_block: fail,
        });
        self.switch_to(next);
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
        let borrowed = Operand::Borrow {
            place: collection.clone(),
            mutable: false,
        };
        if iterable_ty == Type::String {
            // A string loop steps through the UTF-8 bytes; `index` is the
            // byte offset of the next character.
            self.assign(Place::slot(count), Rvalue::ByteLength(borrowed));
        } else if iterable_ty != Type::Range {
            // Re-read each time; the loop's borrow keeps the length fixed.
            self.assign(
                Place::slot(count),
                Rvalue::Length {
                    value: borrowed,
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
        if iterable_ty == Type::String {
            self.string_loop_body(bindings, collection, index, body, [head, exit, step]);
            self.switch_to(exit);
            self.pop_scope();
            return;
        }
        let element_places: Vec<Place> = match &iterable_ty {
            Type::Array(_) | Type::View(..) => vec![collection.project(Projection::Index(index))],
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

    /// A loop over an iterator: each iteration calls `next`, and the loop
    /// ends when it returns `nothing`; otherwise the loop variable owns the
    /// value until the iteration ends.
    fn iterator_loop(
        &mut self,
        bindings: &[LocalId],
        iterable: &'p hir::Expr,
        body: &'p hir::Block,
        next: &'p hir::NextCall,
    ) {
        self.push_scope();
        let iterator_ty = self.ty(&iterable.ty);
        let iterator = self.place_of(iterable, Want::Mut);
        let reference = self.new_slot(iterator_ty, SlotKind::Ref { mutable: true }, None);
        self.push(Inst::Bind {
            slot: reference,
            place: iterator,
            mutable: true,
        });
        self.local_slots.insert(next.iterator, reference);
        let result_ty = self.ty(&next.call.ty);
        let result = self.temp(result_ty.clone());
        let ended = self.new_slot(Type::Boolean, SlotKind::Value, None);
        let head = self.new_block();
        let body_entry = self.new_block();
        let exit = self.new_block();
        self.goto(head);

        self.switch_to(head);
        self.expr_into(&next.call, Place::slot(result));
        let value = self.read_operand(&Place::slot(result), &result_ty);
        let tag = self.l.tag(&Type::Nothing);
        self.assign(Place::slot(ended), Rvalue::TagIs { value, tag });
        self.terminate(Terminator::Branch {
            condition: Operand::Copy(Place::slot(ended)),
            then_block: exit,
            else_block: body_entry,
        });

        self.switch_to(body_entry);
        let scope_depth = self.scopes.len();
        self.push_scope();
        let local = bindings[0];
        let info = &self.locals[local.0 as usize];
        let ty = self.ty(&info.ty);
        let name = Some(info.name.clone());
        let slot = self.new_slot(ty.clone(), SlotKind::Value, name);
        self.scopes.last_mut().expect("scope").slots.push(slot);
        self.local_slots.insert(local, slot);
        let payload = self.narrow(Place::slot(result), &result_ty, &ty);
        let operand = if ty.is_copy() {
            Operand::Copy(payload)
        } else {
            Operand::Move(payload)
        };
        self.assign(Place::slot(slot), Rvalue::Use(operand));
        self.loops.push(LoopCtx {
            break_to: exit,
            continue_to: head,
            scope_depth,
            finally_depth: self.finallies.len(),
        });
        self.lower_block(body);
        self.loops.pop();
        self.pop_scope();
        self.goto(head);

        self.switch_to(exit);
        self.pop_scope();
    }

    /// One iteration of a loop over a string: the loop variable owns the
    /// next character and is destroyed when the iteration ends, however it
    /// ends.
    fn string_loop_body(
        &mut self,
        bindings: &[LocalId],
        text: Place,
        offset: SlotId,
        body: &'p hir::Block,
        [head, exit, step]: [BlockId; 3],
    ) {
        let scope_depth = self.scopes.len();
        self.push_scope();
        let local = bindings[0];
        let info = &self.locals[local.0 as usize];
        let name = Some(info.name.clone());
        let slot = self.new_slot(Type::String, SlotKind::Value, name);
        self.scopes.last_mut().expect("scope").slots.push(slot);
        self.local_slots.insert(local, slot);
        self.assign(
            Place::slot(slot),
            Rvalue::NextChar {
                text: Operand::Borrow {
                    place: text,
                    mutable: false,
                },
                offset: Place::slot(offset),
            },
        );
        self.loops.push(LoopCtx {
            break_to: exit,
            continue_to: step,
            scope_depth,
            finally_depth: self.finallies.len(),
        });
        self.lower_block(body);
        self.loops.pop();
        self.pop_scope();
        self.goto(step);
        self.switch_to(step);
        self.goto(head);
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
                let exiting = self.new_block();
                self.unless_exiting(exiting);
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
                self.switch_to(exiting);
                self.finally_then_propagate(finally_depth);
            }
            None => self.finally_then_propagate(finally_depth),
        }
        self.finallies.truncate(finally_depth);
        self.switch_to(after);
    }

    /// At a handler: continues when the pending error is an ordinary one,
    /// and jumps to `exiting` when it is the program exiting (`os.exit`),
    /// which `catch` and `try` never stop.
    fn unless_exiting(&mut self, exiting: BlockId) {
        self.push_scope();
        let code = self.temp(Type::String);
        let error = Place::slot(self.f.error_slot);
        self.assign(
            Place::slot(code),
            Rvalue::ErrorCode(Operand::Borrow {
                place: error,
                mutable: false,
            }),
        );
        // `Exit` ends the program and `Cancelled` a task; neither stops.
        let mut flags = Vec::new();
        for passing in [EXIT_CODE, CANCELLED_CODE] {
            let flag = self.new_slot(Type::Boolean, SlotKind::Value, None);
            self.assign(
                Place::slot(flag),
                Rvalue::Compare {
                    op: CompareOp::Equal,
                    ty: Type::String,
                    lhs: Operand::Borrow {
                        place: Place::slot(code),
                        mutable: false,
                    },
                    rhs: Operand::Const(Constant::Str(passing.to_string())),
                },
            );
            flags.push(flag);
        }
        self.pop_scope();
        for flag in flags {
            let next = self.new_block();
            self.terminate(Terminator::Branch {
                condition: Operand::Copy(Place::slot(flag)),
                then_block: exiting,
                else_block: next,
            });
            self.switch_to(next);
        }
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
                // The value comes first, so that the positions checked on the
                // way to the element are still valid when it is stored.
                let value_operand = self.value_temp(value);
                let target_ty = self.ty(target_ty);
                let base = self.hir_place_read(target);
                match target_ty {
                    Type::Dict(..) => {
                        let key = self.operand(index, Want::Read);
                        self.push(Inst::DictSet {
                            dict: base,
                            key,
                            value: value_operand,
                        });
                    }
                    _ => {
                        let index_operand = self.operand(index, Want::Owned);
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
                        self.assign(element, Rvalue::Use(value_operand));
                    }
                }
            }
            hir::Place::Field { .. } | hir::Place::Narrow { .. } => {
                let value_operand = self.value_temp(value);
                let target = self.hir_place_read(place);
                self.push(Inst::Drop(target.clone()));
                self.assign(target, Rvalue::Use(value_operand));
            }
        }
    }

    /// Evaluates a value into a temporary and returns the operand that
    /// transfers (or copies) it out.
    fn value_temp(&mut self, value: &'p hir::Expr) -> Operand {
        let ty = self.ty(&value.ty);
        let temp = self.temp(ty.clone());
        self.expr_into(value, Place::slot(temp));
        if ty.is_copy() {
            Operand::Copy(Place::slot(temp))
        } else {
            Operand::Move(Place::slot(temp))
        }
    }

    fn compound_assign(
        &mut self,
        place: &'p hir::Place,
        op: BinaryOp,
        value: &'p hir::Expr,
        span: Span,
    ) {
        let ty = self.place_type(place);
        let rhs = self.operand(value, Want::Read);
        let target = self.hir_place_read(place);
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
                Type::Array(element) | Type::View(element, _) => *element,
                Type::Dict(_, value) => *value,
                other => other,
            },
            hir::Place::Field {
                target_ty, name, ..
            } => self.field_of(&self.ty(target_ty), name).1,
            hir::Place::Narrow { ty, .. } => self.ty(ty),
        }
    }

    /// The projection reading `name` of a value: a structure's field, or
    /// the value of a `Shared` handle.
    fn field_projection(&self, ty: &Type, name: &str) -> Projection {
        match ty {
            Type::Wrapper(Wrapper::Shared, _) => Projection::Deref,
            _ => Projection::Field(self.field_of(ty, name).0),
        }
    }

    /// The position and type of a structure's field.
    fn field_of(&self, ty: &Type, name: &str) -> (u32, Type) {
        let Type::Named(named) = ty else {
            panic!("field `{name}` of {ty}");
        };
        let def = &self.l.program.types[named.id.0 as usize];
        let index = def.field_index(name).expect("checked field");
        (index as u32, def.fields()[index].ty.substitute(&named.args))
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
            hir::Place::Field {
                target,
                target_ty,
                name,
            } => {
                let base = self.hir_place_read(target);
                base.project(self.field_projection(&self.ty(target_ty), name))
            }
            hir::Place::Narrow {
                target,
                ty,
                variant,
            } => {
                let from = self.place_type(target);
                let base = self.hir_place_read(target);
                if let Some(variant) = variant {
                    return base.project(Projection::VariantField {
                        variant: *variant,
                        field: 0,
                    });
                }
                let ty = self.ty(ty);
                self.narrow(base, &from, &ty)
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
            hir::ExprKind::Index { target, index } if self.ty(&target.ty) != Type::String => {
                let target_ty = self.ty(&target.ty);
                let base = self.place_of(target, Want::Read);
                self.element_place(base, &target_ty, index, expr.span)
            }
            hir::ExprKind::Field { value, name } => {
                let value_ty = self.ty(&value.ty);
                let base = self.place_of(value, Want::Read);
                base.project(self.field_projection(&value_ty, name))
            }
            hir::ExprKind::VariantPayload { value, variant } => {
                let base = self.place_of(value, Want::Read);
                base.project(Projection::VariantField {
                    variant: *variant,
                    field: 0,
                })
            }
            _ => self.temp_place(expr, want),
        }
    }

    /// The place of a local narrowed to `ty`.
    fn narrowed_place(&mut self, local: Place, ty: &Type) -> Place {
        let local_ty = self.place_ty(&local);
        self.narrow(local, &local_ty, ty)
    }

    /// A place holding a value of type `from`, seen as the narrower type
    /// `to`: the payload for a single member of a union, or the union itself
    /// for a smaller union (tags are program-wide).
    fn narrow(&mut self, place: Place, from: &Type, to: &Type) -> Place {
        if from == to || matches!(to, Type::Union(_)) || !matches!(from, Type::Union(_)) {
            return place;
        }
        let tag = self.l.tag(to);
        place.project(Projection::Payload(tag))
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
            hir::ExprKind::Narrow(inner) | hir::ExprKind::VariantPayload { value: inner, .. } => {
                match &inner.kind {
                    hir::ExprKind::Local { usage, .. } => Some(*usage),
                    _ => None,
                }
            }
            _ => None,
        };
        let placeable = match &expr.kind {
            hir::ExprKind::Local { .. }
            | hir::ExprKind::Constant(_)
            | hir::ExprKind::Field { .. } => true,
            // A character of a string is computed, not stored.
            hir::ExprKind::Index { target, .. } => self.ty(&target.ty) != Type::String,
            hir::ExprKind::Narrow(inner) | hir::ExprKind::VariantPayload { value: inner, .. } => {
                matches!(inner.kind, hir::ExprKind::Local { .. })
            }
            _ => false,
        };
        if !placeable {
            let slot = self.temp(ty.clone());
            self.expr_into(expr, Place::slot(slot));
            let place = Place::slot(slot);
            return match want {
                _ if ty.is_copy() && want != Want::Mut => Operand::Copy(place),
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
        if ty.is_copy() && want != Want::Mut {
            return Operand::Copy(place);
        }
        // Using a counted handle again retains it: the owner gets a clone.
        if want == Want::Owned && ty.is_retained() && usage != Some(Usage::Move) {
            let slot = self.temp(ty.clone());
            self.assign(
                Place::slot(slot),
                Rvalue::Clone {
                    value: Operand::Borrow {
                        place,
                        mutable: false,
                    },
                    ty,
                },
            );
            return Operand::Move(Place::slot(slot));
        }
        match (usage, want) {
            (Some(Usage::Move), _) => Operand::Move(place),
            // The checker only allows taking a field of a temporary.
            (None, Want::Owned) if matches!(expr.kind, hir::ExprKind::Field { .. }) => {
                Operand::Move(place)
            }
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

    /// A generic function's instance can make a dictionary whose key type,
    /// filled in from the type arguments, cannot be a key.
    fn check_key_type(&mut self, key: &Type, span: Span) {
        if let Some(part) = sayon_check::types::non_key_part(&self.l.program.types, key) {
            self.report(
                Diagnostic::error("E_KEY_TYPE", format!("{part} cannot be a dictionary key"))
                    .primary(span, format!("in the instance `{}`", self.f.name)),
            );
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
            H::NullPointer => self.assign(dest, Rvalue::Use(Operand::Const(Constant::NullPointer))),
            H::CallForeign { function, args } => {
                let args = args
                    .iter()
                    .map(|arg| {
                        let ty = self.ty(&arg.expr.ty);
                        (self.operand(&arg.expr, Want::Read), ty)
                    })
                    .collect();
                self.op(
                    Op::CallForeign {
                        function: *function,
                        args,
                    },
                    Some(dest),
                    span,
                );
            }
            H::Index { target, index } if self.ty(&target.ty) == Type::String => {
                let text = self.operand(target, Want::Read);
                let index = self.operand(index, Want::Owned);
                self.op(Op::CharAt { text, index }, Some(dest), span);
            }
            H::Local { .. }
            | H::Constant(_)
            | H::Index { .. }
            | H::Narrow(_)
            | H::Field { .. }
            | H::VariantPayload { .. } => {
                let operand = self.operand(expr, Want::Owned);
                self.assign(dest, Rvalue::Use(operand));
            }
            H::Record(fields) => {
                // Fields are evaluated in the order written, then laid out in
                // declaration order.
                let mut operands: Vec<Option<Operand>> = vec![None; fields.len()];
                for (index, value) in fields {
                    operands[*index as usize] = Some(self.operand(value, Want::Owned));
                }
                let fields = operands
                    .into_iter()
                    .map(|operand| operand.expect("every field"))
                    .collect();
                self.assign(dest, Rvalue::MakeRecord { ty, fields });
            }
            H::Variant { variant, args } => {
                let fields = args
                    .iter()
                    .map(|arg| self.operand(arg, Want::Owned))
                    .collect();
                self.assign(
                    dest,
                    Rvalue::MakeVariant {
                        ty,
                        variant: *variant,
                        fields,
                    },
                );
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
                    let reference = by_reference(arg.mode, &param_ty);
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
            H::FunctionRef {
                function,
                type_args,
            } => {
                let args = type_args.iter().map(|t| self.ty(t)).collect();
                let code = self.l.thunk(*function, args);
                self.assign(
                    dest,
                    Rvalue::MakeClosure {
                        function: code,
                        env: None,
                        captures: Vec::new(),
                    },
                );
            }
            H::Closure { function, captures } => {
                // A closure shares the type parameters of the function it is in.
                let args = self.type_args.clone();
                let code = self.l.instance(*function, args.clone());
                let env = self.l.env_type(*function, &args);
                let captures = captures
                    .iter()
                    .map(|capture| self.operand(&capture.value, Want::Owned))
                    .collect();
                self.assign(
                    dest,
                    Rvalue::MakeClosure {
                        function: code,
                        env,
                        captures,
                    },
                );
            }
            H::CallValue { callee, args } => {
                let Type::Function(signature) = self.ty(&callee.ty) else {
                    panic!("call of a {} value", callee.ty)
                };
                let callee = self.place_of(callee, Want::Mut);
                let mut operands = Vec::new();
                for (arg, (mode, param_ty)) in args.iter().zip(&signature.params) {
                    let want = if !by_reference(*mode, param_ty) {
                        Want::Owned
                    } else if *mode == ParamMode::Update {
                        Want::Mut
                    } else {
                        Want::Read
                    };
                    operands.push(self.operand(&arg.expr, want));
                }
                self.op(
                    Op::CallValue {
                        callee,
                        args: operands,
                    },
                    Some(dest),
                    span,
                );
            }
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
                if let Type::Dict(key, value) = &ty {
                    self.check_key_type(key, span);
                    self.assign(
                        dest,
                        Rvalue::MakeDict {
                            key: (**key).clone(),
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
                let (key_ty, value_ty) = match &ty {
                    Type::Dict(key, value) => ((**key).clone(), (**value).clone()),
                    _ => (Type::Unknown, Type::Unknown),
                };
                self.check_key_type(&key_ty, span);
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
                        key: key_ty,
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
            H::TryExpr(inner) => self.try_expr(inner, &ty, dest),
            H::IsVariant { value, variant } => {
                let value = self.operand(value, Want::Read);
                self.assign(
                    dest,
                    Rvalue::VariantIs {
                        value,
                        variant: *variant,
                    },
                );
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

    /// `try value`: evaluates the value with a handler of its own, so that an
    /// error raised on the way becomes `failure(error)` instead of
    /// propagating; otherwise the result is `success(value)`.
    fn try_expr(&mut self, inner: &'p hir::Expr, ty: &Type, dest: Place) {
        let inner_ty = self.ty(&inner.ty);
        let on_error = self.new_block();
        let join = self.new_block();
        self.handlers.push(HandlerCtx {
            target: on_error,
            scope_depth: self.scopes.len(),
        });
        self.push_scope();
        let value = self.temp(inner_ty.clone());
        self.expr_into(inner, Place::slot(value));
        let operand = if inner_ty.is_copy() {
            Operand::Copy(Place::slot(value))
        } else {
            Operand::Move(Place::slot(value))
        };
        self.assign(
            dest.clone(),
            Rvalue::MakeVariant {
                ty: ty.clone(),
                variant: 0,
                fields: vec![operand],
            },
        );
        self.pop_scope();
        self.handlers.pop();
        self.goto(join);

        // The error edge has already destroyed the temporaries of the value.
        self.switch_to(on_error);
        let exiting = self.new_block();
        self.unless_exiting(exiting);
        let problem = Operand::Move(Place::slot(self.f.error_slot));
        self.assign(
            dest,
            Rvalue::MakeVariant {
                ty: ty.clone(),
                variant: 1,
                fields: vec![problem],
            },
        );
        self.goto(join);
        self.switch_to(exiting);
        let target = self.error_target();
        self.goto(target);
        self.switch_to(join);
    }

    /// `channel_send_result`: `sent` (variant 0) once the channel takes the
    /// message, or `unsent(message, problem)` (variant 1) when it is closed,
    /// which leaves the message with the sender. Any other error, such as
    /// a cancellation while it waits, goes on as for `send`.
    fn send_result(&mut self, args: &'p [hir::Arg], ty: &Type, dest: Place, span: Span) {
        let channel = self.operand(&args[0].expr, Want::Read);
        let message_ty = self.ty(&args[1].expr.ty);
        let message = match self.value_temp(&args[1].expr) {
            Operand::Copy(place) | Operand::Move(place) => place,
            other => panic!("message operand {other:?}"),
        };
        let on_error = self.new_block();
        let join = self.new_block();
        // The message lives in the scope around the handler, so the error
        // edge keeps it.
        self.handlers.push(HandlerCtx {
            target: on_error,
            scope_depth: self.scopes.len(),
        });
        self.op(
            Op::Send {
                channel,
                message: message.clone(),
                keep: true,
            },
            None,
            span,
        );
        self.handlers.pop();
        self.assign(
            dest.clone(),
            Rvalue::MakeVariant {
                ty: ty.clone(),
                variant: 0,
                fields: Vec::new(),
            },
        );
        self.goto(join);

        self.switch_to(on_error);
        let propagate = self.new_block();
        self.unless_code("ChannelClosedError", propagate);
        let message = if message_ty.is_copy() {
            Operand::Copy(message)
        } else {
            Operand::Move(message)
        };
        let problem = Operand::Move(Place::slot(self.f.error_slot));
        self.assign(
            dest,
            Rvalue::MakeVariant {
                ty: ty.clone(),
                variant: 1,
                fields: vec![message, problem],
            },
        );
        self.goto(join);
        self.switch_to(propagate);
        let target = self.error_target();
        self.goto(target);
        self.switch_to(join);
    }

    /// Goes on when the error in the error slot has the code `code`, and
    /// branches to `otherwise` when it has another.
    fn unless_code(&mut self, code: &str, otherwise: BlockId) {
        self.push_scope();
        let found = self.temp(Type::String);
        self.assign(
            Place::slot(found),
            Rvalue::ErrorCode(Operand::Borrow {
                place: Place::slot(self.f.error_slot),
                mutable: false,
            }),
        );
        let flag = self.new_slot(Type::Boolean, SlotKind::Value, None);
        self.assign(
            Place::slot(flag),
            Rvalue::Compare {
                op: CompareOp::Equal,
                ty: Type::String,
                lhs: Operand::Borrow {
                    place: Place::slot(found),
                    mutable: false,
                },
                rhs: Operand::Const(Constant::Str(code.to_string())),
            },
        );
        self.pop_scope();
        let next = self.new_block();
        self.terminate(Terminator::Branch {
            condition: Operand::Copy(Place::slot(flag)),
            then_block: next,
            else_block: otherwise,
        });
        self.switch_to(next);
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
            Builtin::View | Builtin::MutableView => {
                let want = if builtin == Builtin::MutableView {
                    Want::Mut
                } else {
                    Want::Read
                };
                let base = self.place_of(&args[0].expr, want);
                let start = args.get(1).map(|arg| self.operand(&arg.expr, Want::Owned));
                let end = args.get(2).map(|arg| self.operand(&arg.expr, Want::Owned));
                self.op(Op::MakeView { base, start, end }, Some(dest), span);
            }
            Builtin::TypedDecode => {
                let value = self.place_of(&args[0].expr, Want::Read);
                let path = self.temp(Type::String);
                self.assign(
                    Place::slot(path),
                    Rvalue::Use(Operand::Const(Constant::Str("$".to_string()))),
                );
                let function = self.l.decoder(ty, span);
                let args = vec![
                    Operand::Borrow {
                        place: value,
                        mutable: false,
                    },
                    Operand::Borrow {
                        place: Place::slot(path),
                        mutable: false,
                    },
                ];
                self.op(Op::Call { function, args }, Some(dest), span);
            }
            Builtin::TypedEncode => {
                let value_ty = self.ty(&args[0].expr.ty);
                let value = self.place_of(&args[0].expr, Want::Read);
                let function = self.l.encoder(&value_ty, span);
                let args = vec![Operand::Borrow {
                    place: value,
                    mutable: false,
                }];
                self.op(Op::Call { function, args }, Some(dest), span);
            }
            Builtin::Share => {
                let inner = self.ty(&args[0].expr.ty);
                let value = self.operand(&args[0].expr, Want::Owned);
                self.assign(dest, Rvalue::MakeShared { value, ty: inner });
            }
            // A cell is laid out like its value.
            Builtin::MakeCell => {
                let value = self.operand(&args[0].expr, Want::Owned);
                self.assign(dest, Rvalue::Use(value));
            }
            Builtin::Downgrade => {
                let shared = self.operand(&args[0].expr, Want::Read);
                self.assign(dest, Rvalue::Downgrade(shared));
            }
            Builtin::Upgrade => {
                let weak = self.operand(&args[0].expr, Want::Read);
                let Type::Wrapper(_, inner) = self.ty(&args[0].expr.ty) else {
                    panic!("upgrade of a non-weak value");
                };
                let shared_tag = self.l.tag(&Type::wrapper(Wrapper::Shared, *inner));
                let nothing_tag = self.l.tag(&Type::Nothing);
                self.assign(
                    dest,
                    Rvalue::Upgrade {
                        weak,
                        shared_tag,
                        nothing_tag,
                    },
                );
            }
            Builtin::CellGet => {
                let cell = self.place_of(&args[0].expr, Want::Read);
                let value = if ty.is_copy() {
                    Rvalue::Use(Operand::Copy(cell))
                } else {
                    Rvalue::Clone {
                        value: Operand::Borrow {
                            place: cell,
                            mutable: false,
                        },
                        ty: ty.clone(),
                    }
                };
                self.assign(dest, value);
            }
            Builtin::CellSet | Builtin::CellReplace => {
                // The new value first, in case it reads the cell.
                let value = self.value_temp(&args[1].expr);
                let cell = self.place_of(&args[0].expr, Want::Read);
                let inner = self.ty(&args[1].expr.ty);
                if builtin == Builtin::CellReplace {
                    let old = if inner.is_copy() {
                        Operand::Copy(cell.clone())
                    } else {
                        Operand::Move(cell.clone())
                    };
                    self.assign(dest, Rvalue::Use(old));
                } else {
                    self.push(Inst::Drop(cell.clone()));
                    self.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing)));
                }
                self.assign(cell, Rvalue::Use(value));
            }
            Builtin::TaskSpawn => {
                let Type::Wrapper(_, result) = ty.clone() else {
                    panic!("spawn without a task type")
                };
                let body = self.operand(&args[0].expr, Want::Owned);
                self.op(
                    Op::Spawn {
                        body,
                        result: *result,
                    },
                    Some(dest),
                    span,
                );
            }
            // A task waited for is read in place, so that waiting again
            // finds it; it is destroyed with its variable.
            Builtin::TaskJoin | Builtin::TaskDetach => {
                let task = self.place_of(&args[0].expr, Want::Read);
                if builtin == Builtin::TaskJoin {
                    self.op(Op::Join { task }, Some(dest), span);
                } else {
                    self.op(Op::Detach { task }, None, span);
                    self.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing)));
                }
            }
            Builtin::TaskYield => {
                self.op(Op::Yield, None, span);
                self.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing)));
            }
            Builtin::TaskSleep => {
                let nanoseconds = self.operand(&args[0].expr, Want::Read);
                self.op(Op::Sleep { nanoseconds }, None, span);
                self.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing)));
            }
            Builtin::RawRead => {
                let address = self.operand(&args[0].expr, Want::Read);
                let offset = self.operand(&args[1].expr, Want::Read);
                self.op(
                    Op::RawRead {
                        address,
                        offset,
                        ty: ty.clone(),
                    },
                    Some(dest),
                    span,
                );
            }
            Builtin::RawWrite => {
                let value_ty = self.ty(&args[2].expr.ty);
                let address = self.operand(&args[0].expr, Want::Read);
                let offset = self.operand(&args[1].expr, Want::Read);
                let value = self.operand(&args[2].expr, Want::Read);
                self.op(
                    Op::RawWrite {
                        address,
                        offset,
                        value,
                        ty: value_ty,
                    },
                    None,
                    span,
                );
                self.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing)));
            }
            Builtin::ChannelNew => {
                let Type::Wrapper(_, element) = ty.clone() else {
                    panic!("channel without a channel type")
                };
                let capacity = self.operand(&args[0].expr, Want::Read);
                self.op(
                    Op::ChannelNew {
                        capacity,
                        element: *element,
                    },
                    Some(dest),
                    span,
                );
            }
            Builtin::ChannelSend => {
                let channel = self.operand(&args[0].expr, Want::Read);
                // The message stays in place until the channel takes it.
                let message = self.value_temp(&args[1].expr);
                let message = match message {
                    Operand::Copy(place) | Operand::Move(place) => place,
                    other => panic!("message operand {other:?}"),
                };
                self.op(
                    Op::Send {
                        channel,
                        message,
                        keep: false,
                    },
                    None,
                    span,
                );
                self.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing)));
            }
            Builtin::ChannelSendResult => self.send_result(args, ty, dest, span),
            Builtin::ChannelReceive => {
                let channel = self.operand(&args[0].expr, Want::Read);
                self.op(Op::Receive { channel }, Some(dest), span);
            }
            Builtin::ChannelClose => {
                let channel = self.operand(&args[0].expr, Want::Read);
                self.op(Op::Close { channel }, None, span);
                self.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing)));
            }
            Builtin::ChannelSelect => {
                let channels = self.operand(&args[0].expr, Want::Read);
                self.op(Op::Select { channels }, Some(dest), span);
            }
            Builtin::SplitView => {
                let view = self.place_of(&args[0].expr, Want::Mut);
                let count = self.operand(&args[1].expr, Want::Owned);
                self.op(Op::SplitView { view, count }, Some(dest), span);
            }
            Builtin::Swap => {
                let array = self.place_of(&args[0].expr, Want::Mut);
                let first = self.operand(&args[1].expr, Want::Read);
                let second = self.operand(&args[2].expr, Want::Read);
                self.op(
                    Op::Swap {
                        array,
                        first,
                        second,
                    },
                    None,
                    span,
                );
                self.assign(dest, Rvalue::Use(Operand::Const(Constant::Nothing)));
            }
            Builtin::Remove => {
                let collection_ty = self.ty(&args[0].expr.ty);
                let collection = self.place_of(&args[0].expr, Want::Mut);
                let key = self.operand(&args[1].expr, Want::Read);
                match collection_ty {
                    Type::Dict(_, value) => {
                        let wraps = matches!(ty, Type::Union(_));
                        let value_tag = (wraps && !matches!(*value, Type::Union(_)))
                            .then(|| self.l.tag(&value));
                        let nothing_tag = wraps.then(|| self.l.tag(&Type::Nothing));
                        self.assign(
                            dest,
                            Rvalue::RemoveKey {
                                dict: collection,
                                key,
                                value_tag,
                                nothing_tag,
                            },
                        );
                    }
                    _ => self.op(
                        Op::RemoveAt {
                            array: collection,
                            index: key,
                        },
                        Some(dest),
                        span,
                    ),
                }
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
                if function.always_fails() {
                    self.terminate(Terminator::Unreachable);
                }
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
