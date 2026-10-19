//! Executes IR with an explicit frame stack.
//!
//! A slot holds nothing, an owned value, or a reference (a root slot or global
//! plus a path into it). Moving a value takes it out of its slot; borrowing
//! creates a reference that the callee reads and writes through.

use crate::value::*;
use parallel::{JoinThreads, MutexCell, ParallelState, Program};
use sayon_check::types::{FloatType, IntType, Type};
use sayon_ir::*;
use sayon_runtime::host::{
    CANCELLED_CODE, EXIT_CODE, Host, HostArg, HostType, HostValue, exit_status, invoke,
};
use std::sync::{Arc, Mutex as Lock};
use tasks::{Fiber, Progress};

pub(crate) mod parallel;
mod tasks;

/// Where a program's output goes, shared by the machines of its threads.
pub type Output<'m> = Arc<Lock<&'m mut (dyn Write + Send)>>;
use sayon_runtime::text::{
    Number, parse_number, position_error, range_length, split_bounds_message, view_bounds_message,
};
use sayon_syntax::{SourceMap, Span};
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

/// The deepest call nesting allowed before execution stops.
pub const MAX_CALL_DEPTH: usize = 100_000;

#[derive(Debug)]
pub enum RunError {
    /// A recoverable error nothing caught.
    Uncaught(ErrorValue),
    /// Tasks failed and nothing joined them, though the main task finished.
    TaskFailures(Vec<ErrorValue>),
    /// A failure that ends execution without running cleanup.
    Fatal(String),
}

#[derive(Clone, Debug)]
enum SlotValue {
    Empty,
    Value(Value),
    Ref(Ref),
}

enum Source {
    Owned(Value),
    Ref(Ref),
}

struct Frame {
    function: FunctionId,
    slots: Vec<SlotValue>,
    block: BlockId,
    inst: usize,
    /// Destination, normal successor, and error successor of the call this
    /// frame is waiting on.
    waiting: Option<(Option<Place>, BlockId, BlockId)>,
}

pub struct Machine<'m> {
    module: &'m Module,
    sources: &'m SourceMap,
    globals: Vec<Option<Value>>,
    frames: Vec<Frame>,
    out: Output<'m>,
    /// Every task; the running one's frames are `frames`.
    fibers: Vec<Fiber>,
    current: usize,
    /// Set when every task is to stop where it waits: at the end of the
    /// main task, or when a task called `os.exit`.
    stopping: bool,
    /// The status a task's `os.exit` asked for.
    exit_status: Option<i32>,
    /// The C libraries and functions the program has called.
    foreign: crate::foreign::Foreign,
    /// What the machines of the program's threads share.
    program: Arc<Program>,
    /// For the machine of a task on a thread of its own: the task.
    thread_task: Option<Arc<ParallelState>>,
    /// The channels made on this thread that no task on another thread can
    /// hold yet (see `parallel`), and the length at which to forget the
    /// ones gone.
    local_channels: Vec<std::sync::Weak<crate::value::ChannelCell>>,
    prune_channels_at: usize,
}

/// The first step of `steps` through a view: its position, the viewed
/// array, and the element of the array it reaches.
fn view_step(current: &Value, steps: &[Step], index: usize) -> Option<(usize, Ref, usize)> {
    let (step, rest) = steps.split_first()?;
    match (step, current) {
        (Step::Index(i), Value::View(view)) => Some((index, view.array.clone(), view.start + i)),
        // A byte of a string, through a byte view, is the last step.
        (_, Value::Str(_)) => None,
        (Step::Deref, Value::Shared(shared)) => view_step(&shared.borrow(), rest, index + 1),
        _ => view_step(step_into(*step, current), rest, index + 1),
    }
}

/// Calls `f` on what `steps` reach from `current`. A step through a shared
/// value reads inside it while `f` runs.
fn read_steps<R>(current: &Value, steps: &[Step], f: impl FnOnce(&Value) -> R) -> R {
    let Some((step, rest)) = steps.split_first() else {
        return f(current);
    };
    match (step, current) {
        (Step::Index(i), Value::Str(text)) => f(&Value::Int(text.as_bytes()[*i] as i128)),
        (Step::Deref, Value::Shared(shared)) => read_steps(&shared.borrow(), rest, f),
        _ => read_steps(step_into(*step, current), rest, f),
    }
}

/// Calls `f` on what `steps` reach from `current`, for a change. Only a
/// cell changes inside a shared value.
fn write_steps<R>(current: &mut Value, steps: &[Step], f: impl FnOnce(&mut Value) -> R) -> R {
    let Some((step, rest)) = steps.split_first() else {
        return f(current);
    };
    let next = match (step, current) {
        (Step::Deref, Value::Shared(shared)) => {
            return write_steps(&mut shared.borrow_mut(), rest, f);
        }
        (Step::Index(i), Value::Array(items)) => &mut items[*i],
        (Step::DictValue(i), Value::Dict(dict)) => &mut dict.values[*i],
        (Step::Payload, Value::Union(_, payload)) => payload,
        (Step::Field(i), Value::Record(_, fields) | Value::Variant(_, _, fields)) => {
            &mut fields[*i]
        }
        (Step::Env, Value::Function(_, env)) => env,
        (step, value) => panic!("invalid mutable path step {step:?} into {value:?}"),
    };
    write_steps(next, rest, f)
}

/// The part of a value that one step leads to.
fn step_into(step: Step, value: &Value) -> &Value {
    match (step, value) {
        (Step::Index(i), Value::Array(items)) => &items[i],
        (Step::DictValue(i), Value::Dict(dict)) => &dict.values[i],
        (Step::DictKey(i), Value::Dict(dict)) => &dict.keys[i],
        (Step::Payload, Value::Union(_, payload)) => payload,
        (Step::Field(i), Value::Record(_, fields) | Value::Variant(_, _, fields)) => &fields[i],
        (Step::Env, Value::Function(_, env)) => env,
        (step, value) => panic!("invalid path step {step:?} into {value:?}"),
    }
}

fn constant_value(constant: &Constant) -> Value {
    match constant {
        Constant::Int(value, _) => Value::Int(*value),
        Constant::Float(value, FloatType::F64) => Value::F64(*value),
        Constant::Float(value, FloatType::F32) => Value::F32(*value as f32),
        Constant::Bool(value) => Value::Bool(*value),
        Constant::Nothing => Value::Nothing,
        Constant::Str(text) => Value::str(text),
        Constant::NullPointer => Value::Pointer(0),
    }
}

impl<'m> Machine<'m> {
    pub fn new(
        module: &'m Module,
        sources: &'m SourceMap,
        out: &'m mut (dyn Write + Send),
    ) -> Machine<'m> {
        Machine::with_output(
            module,
            sources,
            Arc::new(Lock::new(out)),
            Default::default(),
        )
    }

    fn with_output(
        module: &'m Module,
        sources: &'m SourceMap,
        out: Output<'m>,
        program: Arc<Program>,
    ) -> Machine<'m> {
        Machine {
            module,
            sources,
            globals: vec![None; module.globals.len()],
            frames: Vec::new(),
            out,
            fibers: vec![Fiber::main()],
            current: 0,
            stopping: false,
            exit_status: None,
            foreign: Default::default(),
            program,
            thread_task: None,
            local_channels: Vec::new(),
            prune_channels_at: 64,
        }
    }

    /// The machine of a task on a thread of its own.
    fn for_thread(
        module: &'m Module,
        sources: &'m SourceMap,
        out: Output<'m>,
        program: Arc<Program>,
        task: Arc<ParallelState>,
    ) -> Machine<'m> {
        let mut machine = Machine::with_output(module, sources, out, program);
        machine.thread_task = Some(task);
        machine
    }

    /// The program's output, for one write.
    fn output(&self) -> std::sync::MutexGuard<'_, &'m mut (dyn Write + Send)> {
        parallel::lock(&self.out)
    }

    /// Initializes globals, runs the entry function, and returns its exit
    /// status, or the one `os.exit` gave.
    pub fn run(&mut self) -> Result<i32, RunError> {
        let _threads = JoinThreads(self.program.clone());
        let result = self.run_program();
        let result = self.finish(result);
        let _ = self.output().flush();
        // Files kept by constants stay open until the program ends.
        sayon_runtime::streams::flush_all();
        result
    }

    fn run_program(&mut self) -> Result<i32, RunError> {
        let module = self.module;
        for (global, initializer) in &module.initializers {
            let value = self.call(*initializer)?;
            self.globals[global.0 as usize] = Some(value);
        }
        match self.call(module.entry)? {
            Value::Int(code) => Ok(code.clamp(i32::MIN as i128, i32::MAX as i128) as i32),
            _ => Ok(0),
        }
    }

    /// Ends the tasks still running, then gives the program's result: the
    /// status `os.exit` asked for, the main task's own, or the failures of
    /// tasks nothing joined.
    fn finish<T>(&mut self, result: Result<T, RunError>) -> Result<T, RunError>
    where
        T: From<i32>,
    {
        self.shutdown();
        let thread_failures = self.finish_threads();
        let result = match result {
            Err(RunError::Uncaught(error)) if error.code == EXIT_CODE => {
                Ok(T::from(exit_status(&error.message)))
            }
            other => other,
        };
        if let Some(status) = self.exit_status.or(self.program.exit_status()) {
            return Ok(T::from(status));
        }
        let mut failures = self.task_failures();
        failures.extend(thread_failures);
        match result {
            Ok(_) if !failures.is_empty() => Err(RunError::TaskFailures(failures)),
            other => other,
        }
    }

    /// Initializes globals and runs one test function.
    pub fn run_test(&mut self, test: FunctionId) -> Result<(), RunError> {
        let _threads = JoinThreads(self.program.clone());
        let module = self.module;
        for (global, initializer) in &module.initializers {
            let value = self.call(*initializer)?;
            self.globals[global.0 as usize] = Some(value);
        }
        let result = self.call(test).map(|_| 0);
        let result = self.finish(result).map(|_| ());
        let _ = self.output().flush();
        result
    }

    fn call(&mut self, function: FunctionId) -> Result<Value, RunError> {
        let base = self.frames.len();
        self.push_frame(function, Vec::new())?;
        self.execute(base)
    }

    fn push_frame(&mut self, function: FunctionId, args: Vec<SlotValue>) -> Result<(), RunError> {
        let frame = self.new_frame(function, args)?;
        self.frames.push(frame);
        Ok(())
    }

    fn new_frame(&self, function: FunctionId, args: Vec<SlotValue>) -> Result<Frame, RunError> {
        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(RunError::Fatal(format!(
                "the call depth limit of {MAX_CALL_DEPTH} nested calls was exceeded (runaway recursion?)"
            )));
        }
        let definition = self.module.function(function);
        let mut slots = vec![SlotValue::Empty; definition.slots.len()];
        let count = args.len();
        for (slot, value) in definition.params.iter().zip(args) {
            slots[slot.0 as usize] = value;
        }
        slots[definition.argc_slot.0 as usize] = SlotValue::Value(Value::Int(count as i128));
        Ok(Frame {
            function,
            slots,
            block: BlockId(0),
            inst: 0,
            waiting: None,
        })
    }

    fn top(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("frame")
    }

    fn jump(&mut self, block: BlockId) {
        let frame = self.top();
        frame.block = block;
        frame.inst = 0;
    }

    fn execute(&mut self, base: usize) -> Result<Value, RunError> {
        let module = self.module;
        loop {
            let frame = self.frames.last().expect("frame");
            let function = module.function(frame.function);
            let block = function.block(frame.block);
            if frame.inst < block.insts.len() {
                let inst = &block.insts[frame.inst];
                self.top().inst += 1;
                self.instruction(inst);
                continue;
            }
            match &block.term {
                Terminator::Goto(target) => self.jump(*target),
                Terminator::Branch {
                    condition,
                    then_block,
                    else_block,
                } => {
                    let source = self.source(condition);
                    let flag = self.with_source(&source, |v| v.as_bool());
                    self.jump(if flag { *then_block } else { *else_block });
                }
                Terminator::Return => {
                    let mut frame = self.frames.pop().expect("frame");
                    let value = match std::mem::replace(
                        &mut frame.slots[function.ret_slot.0 as usize],
                        SlotValue::Empty,
                    ) {
                        SlotValue::Value(value) => value,
                        _ => Value::Nothing,
                    };
                    if self.frames.len() == base {
                        if self.current == 0 {
                            return Ok(value);
                        }
                        // A task finished; another goes on.
                        if !self.finish_task(Ok(value)) {
                            return Ok(Value::Nothing);
                        }
                        continue;
                    }
                    let (dest, normal, _) = self.top().waiting.take().expect("waiting call");
                    if let Some(dest) = dest {
                        self.write(&dest, value);
                    }
                    self.jump(normal);
                }
                Terminator::ReturnError => {
                    let mut frame = self.frames.pop().expect("frame");
                    let error = match std::mem::replace(
                        &mut frame.slots[function.error_slot.0 as usize],
                        SlotValue::Empty,
                    ) {
                        SlotValue::Value(Value::Error(error)) => *error,
                        _ => ErrorValue {
                            code: "InternalError".into(),
                            message: "missing error value".into(),
                            location: None,
                        },
                    };
                    if self.frames.len() == base {
                        if self.current == 0 {
                            return Err(RunError::Uncaught(error));
                        }
                        if !self.finish_task(Err(error)) {
                            return Ok(Value::Nothing);
                        }
                        continue;
                    }
                    let (_, _, error_block) = self.top().waiting.take().expect("waiting call");
                    let caller = module.function(self.top().function);
                    self.top().slots[caller.error_slot.0 as usize] =
                        SlotValue::Value(Value::Error(Box::new(error)));
                    self.jump(error_block);
                }
                Terminator::Op {
                    op:
                        Op::Call {
                            function: callee,
                            args,
                        },
                    dest,
                    normal,
                    error,
                    ..
                } => {
                    let values: Vec<SlotValue> = args.iter().map(|arg| self.pass(arg)).collect();
                    self.top().waiting = Some((dest.clone(), *normal, *error));
                    self.push_frame(*callee, values)?;
                }
                Terminator::Op {
                    op: Op::CallValue { callee, args },
                    dest,
                    normal,
                    error,
                    ..
                } => {
                    let mut env = self.resolve(callee);
                    let code = self.read(&env, |value| match value {
                        Value::Function(code, _) => *code,
                        other => panic!("call of {other:?}"),
                    });
                    env.path.push(Step::Env);
                    let mut values = vec![SlotValue::Ref(env)];
                    values.extend(args.iter().map(|arg| self.pass(arg)));
                    self.top().waiting = Some((dest.clone(), *normal, *error));
                    self.push_frame(code, values)?;
                }
                Terminator::Op {
                    op,
                    dest,
                    normal,
                    error,
                    span,
                } if Self::is_task_op(op) => match self.task_op(op, *span) {
                    Progress::Done(value) => {
                        if let Some(dest) = dest {
                            self.write(dest, value);
                        }
                        self.jump(*normal);
                    }
                    Progress::Failed(problem) => {
                        let slot = function.error_slot.0 as usize;
                        self.top().slots[slot] = SlotValue::Value(Value::Error(Box::new(problem)));
                        self.jump(*error);
                    }
                    // Another task runs now; this one runs the operation
                    // again when it is resumed.
                    Progress::Wait(_) if self.fibers[self.current].locks > 0 => {
                        let problem = self.error(
                            "LockError",
                            "a task cannot wait while it holds a mutex's lock",
                            *span,
                        );
                        let slot = function.error_slot.0 as usize;
                        self.top().slots[slot] = SlotValue::Value(Value::Error(Box::new(problem)));
                        self.jump(*error);
                    }
                    Progress::Wait(wait) => {
                        if !self.suspend(wait) {
                            let problem = self.error(
                                "DeadlockError",
                                "every task is waiting, so none can go on",
                                *span,
                            );
                            let slot = function.error_slot.0 as usize;
                            self.top().slots[slot] =
                                SlotValue::Value(Value::Error(Box::new(problem)));
                            self.jump(*error);
                        }
                    }
                },
                Terminator::Op {
                    op,
                    dest,
                    normal,
                    error,
                    span,
                } => match self.operation(op, *span) {
                    Ok(value) => {
                        if let Some(dest) = dest {
                            self.write(dest, value);
                        }
                        self.jump(*normal);
                    }
                    Err(problem) => {
                        let slot = function.error_slot.0 as usize;
                        self.top().slots[slot] = SlotValue::Value(Value::Error(Box::new(problem)));
                        self.jump(*error);
                    }
                },
                Terminator::Unreachable => {
                    return Err(RunError::Fatal(format!(
                        "internal error: unreachable code reached in `{}`",
                        function.name
                    )));
                }
            }
        }
    }

    // ----- places --------------------------------------------------------------

    fn slot_int(&self, frame: usize, slot: SlotId) -> usize {
        match &self.frames[frame].slots[slot.0 as usize] {
            SlotValue::Value(Value::Int(value)) => *value as usize,
            other => panic!("expected an index in slot {slot:?}, found {other:?}"),
        }
    }

    fn resolve(&self, place: &Place) -> Ref {
        let frame = self.frames.len() - 1;
        let (root, mut path) = match &place.base {
            Base::Slot(slot) => match &self.frames[frame].slots[slot.0 as usize] {
                SlotValue::Ref(reference) => (reference.root, reference.path.clone()),
                _ => (
                    Root::Slot {
                        frame,
                        slot: slot.0 as usize,
                    },
                    Vec::new(),
                ),
            },
            Base::Global(global) => (Root::Global(global.0 as usize), Vec::new()),
        };
        for projection in &place.projection {
            path.push(match projection {
                Projection::Index(slot) => Step::Index(self.slot_int(frame, *slot)),
                Projection::DictValue(slot) => Step::DictValue(self.slot_int(frame, *slot)),
                Projection::DictKey(slot) => Step::DictKey(self.slot_int(frame, *slot)),
                Projection::Payload(_) => Step::Payload,
                Projection::Field(field) | Projection::VariantField { field, .. } => {
                    Step::Field(*field as usize)
                }
                Projection::Deref => Step::Deref,
            });
        }
        Ref { root, path }
    }

    fn root_value(&self, root: Root) -> &Value {
        match root {
            Root::Slot { frame, slot } => match &self.frames[frame].slots[slot] {
                SlotValue::Value(value) => value,
                other => panic!("read of a slot without a value: {other:?}"),
            },
            Root::Global(global) => self.globals[global].as_ref().expect("initialized global"),
        }
    }

    /// The reference with every step through a view replaced by the step
    /// into the viewed array's element.
    fn flatten(&self, reference: &Ref) -> Ref {
        match view_step(self.root_value(reference.root), &reference.path, 0) {
            Some((index, array, element)) => {
                let mut path = array.path.clone();
                path.push(Step::Index(element));
                path.extend_from_slice(&reference.path[index + 1..]);
                self.flatten(&Ref {
                    root: array.root,
                    path,
                })
            }
            None => reference.clone(),
        }
    }

    fn read<R>(&self, reference: &Ref, f: impl FnOnce(&Value) -> R) -> R {
        let reference = self.flatten(reference);
        read_steps(self.root_value(reference.root), &reference.path, f)
    }

    /// The elements a view shows, as an array.
    fn view_array(&self, view: &ViewValue) -> Value {
        self.read(&view.array, |value| match value {
            Value::Array(items) => {
                Value::Array(items[view.start..view.start + view.length].to_vec())
            }
            Value::Str(text) => Value::Array(
                text.as_bytes()[view.start..view.start + view.length]
                    .iter()
                    .map(|&byte| Value::Int(byte as i128))
                    .collect(),
            ),
            other => panic!("view of {other:?}"),
        })
    }

    /// Reads a source, showing a view as the array of its elements.
    fn with_shown<R>(&self, source: &Source, f: impl FnOnce(&Value) -> R) -> R {
        self.with_source(source, |value| match value {
            Value::View(view) => f(&self.view_array(view)),
            other => f(other),
        })
    }

    fn with_mut<R>(&mut self, reference: &Ref, f: impl FnOnce(&mut Value) -> R) -> R {
        let reference = &self.flatten(reference);
        let root: &mut Value = match reference.root {
            Root::Slot { frame, slot } => match &mut self.frames[frame].slots[slot] {
                SlotValue::Value(value) => value,
                other => panic!("write through a slot without a value: {other:?}"),
            },
            Root::Global(global) => self.globals[global].as_mut().expect("initialized global"),
        };
        write_steps(root, &reference.path, f)
    }

    fn write(&mut self, place: &Place, value: Value) {
        let reference = self.resolve(place);
        if reference.path.is_empty() {
            match reference.root {
                Root::Slot { frame, slot } => {
                    self.frames[frame].slots[slot] = SlotValue::Value(value)
                }
                Root::Global(global) => self.globals[global] = Some(value),
            }
        } else {
            self.with_mut(&reference, |target| *target = value);
        }
    }

    fn take_place(&mut self, place: &Place) -> Value {
        let mut reference = self.resolve(place);
        let payload_only = matches!(reference.path.as_slice(), [Step::Payload]);
        if !reference.path.is_empty() && !payload_only {
            // Taking part of a value (a binding of a match that owns it, or
            // a field of a temporary). Moving a union's payload empties the
            // union; the rest is never read again.
            let last = reference.path.pop().expect("path");
            return self.with_mut(&reference, |parent| {
                if let Step::Payload = last {
                    return match std::mem::replace(parent, Value::Nothing) {
                        Value::Union(_, payload) => *payload,
                        other => panic!("payload of {other:?}"),
                    };
                }
                match (last, parent) {
                    (Step::Field(i), Value::Record(_, fields) | Value::Variant(_, _, fields)) => {
                        std::mem::replace(&mut fields[i], Value::Nothing)
                    }
                    (step, value) => panic!("cannot move out of {step:?} in {value:?}"),
                }
            });
        }
        let taken = match reference.root {
            Root::Slot { frame, slot } => {
                std::mem::replace(&mut self.frames[frame].slots[slot], SlotValue::Empty)
            }
            Root::Global(_) => panic!("cannot move out of a global"),
        };
        let value = match taken {
            SlotValue::Value(value) => value,
            other => panic!("move out of a slot without a value: {other:?}"),
        };
        match (payload_only, value) {
            (true, Value::Union(_, payload)) => *payload,
            (_, value) => value,
        }
    }

    fn drop_place(&mut self, place: &Place) {
        let reference = self.resolve(place);
        if !reference.path.is_empty() {
            // Elements are replaced by the assignment that follows.
            return;
        }
        match reference.root {
            Root::Slot { frame, slot } => {
                if let SlotValue::Value(_) = self.frames[frame].slots[slot] {
                    self.frames[frame].slots[slot] = SlotValue::Empty;
                }
            }
            Root::Global(global) => self.globals[global] = None,
        }
    }

    // ----- operands ----------------------------------------------------------------

    fn source(&mut self, operand: &Operand) -> Source {
        match operand {
            Operand::Const(constant) => Source::Owned(constant_value(constant)),
            Operand::Move(place) => Source::Owned(self.take_place(place)),
            Operand::Copy(place) | Operand::Borrow { place, .. } => {
                Source::Ref(self.resolve(place))
            }
        }
    }

    fn with_source<R>(&self, source: &Source, f: impl FnOnce(&Value) -> R) -> R {
        match source {
            Source::Owned(value) => f(value),
            Source::Ref(reference) => self.read(reference, f),
        }
    }

    /// An owned value from an operand; borrowed operands are copied, which the
    /// lowering only produces for Copy types and display.
    fn take(&mut self, operand: &Operand) -> Value {
        match self.source(operand) {
            Source::Owned(value) => value,
            Source::Ref(reference) => self.read(&reference, Value::clone),
        }
    }

    fn pass(&mut self, operand: &Operand) -> SlotValue {
        match operand {
            Operand::Borrow { place, .. } => SlotValue::Ref(self.resolve(place)),
            other => SlotValue::Value(self.take(other)),
        }
    }

    fn int(&mut self, operand: &Operand) -> i128 {
        let source = self.source(operand);
        self.with_source(&source, |v| v.as_int())
    }

    fn string(&mut self, operand: &Operand) -> Rc<str> {
        let source = self.source(operand);
        self.with_source(&source, |v| match v {
            Value::Str(text) => text.clone(),
            other => panic!("expected a string, found {other:?}"),
        })
    }

    // ----- instructions --------------------------------------------------------

    fn instruction(&mut self, inst: &Inst) {
        match inst {
            Inst::Assign { dest, value } => {
                let value = self.rvalue(value);
                self.write(dest, value);
            }
            Inst::Bind { slot, place, .. } => {
                let reference = self.resolve(place);
                self.top().slots[slot.0 as usize] = SlotValue::Ref(reference);
            }
            Inst::Drop(place) => self.drop_place(place),
            Inst::Push { array, value } => {
                let value = self.take(value);
                let reference = self.resolve(array);
                self.with_mut(&reference, |target| match target {
                    Value::Array(items) => items.push(value),
                    other => panic!("push onto {other:?}"),
                });
            }
            Inst::DictSet { dict, key, value } => {
                let key = self.take(key);
                let value = self.take(value);
                let reference = self.resolve(dict);
                self.with_mut(&reference, |target| match target {
                    Value::Dict(dict) => dict.set(key, value),
                    other => panic!("dictionary assignment into {other:?}"),
                });
            }
            Inst::MutexPut { mutex, value } => self.mutex_put(mutex, value),
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue) -> Value {
        match rvalue {
            Rvalue::Use(operand) => self.take(operand),
            Rvalue::Not(operand) => {
                let source = self.source(operand);
                Value::Bool(!self.with_source(&source, |v| v.as_bool()))
            }
            Rvalue::FloatNegate(operand) => match self.take(operand) {
                Value::F64(x) => Value::F64(-x),
                Value::F32(x) => Value::F32(-x),
                other => panic!("negate {other:?}"),
            },
            Rvalue::FloatArith(op, lhs, rhs) => {
                let (a, b) = (self.take(lhs), self.take(rhs));
                match (a, b) {
                    (Value::F64(x), Value::F64(y)) => Value::F64(float_op(*op, x, y)),
                    (Value::F32(x), Value::F32(y)) => {
                        Value::F32(float_op(*op, x as f64, y as f64) as f32)
                    }
                    (a, b) => panic!("float arithmetic on {a:?} and {b:?}"),
                }
            }
            Rvalue::Compare { op, lhs, rhs, .. } => {
                let (a, b) = (self.source(lhs), self.source(rhs));
                let result = self.with_source(&a, |x| {
                    self.with_source(&b, |y| match op {
                        CompareOp::Equal => values_equal(x, y),
                        CompareOp::NotEqual => !values_equal(x, y),
                        CompareOp::Less => compare_values(x, y) == Some(std::cmp::Ordering::Less),
                        CompareOp::Greater => {
                            compare_values(x, y) == Some(std::cmp::Ordering::Greater)
                        }
                        CompareOp::LessEqual => {
                            matches!(
                                compare_values(x, y),
                                Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                            )
                        }
                        CompareOp::GreaterEqual => {
                            matches!(
                                compare_values(x, y),
                                Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
                            )
                        }
                    })
                });
                Value::Bool(result)
            }
            Rvalue::Format(parts) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        FormatPart::Text(literal) => text.push_str(literal),
                        FormatPart::Value(operand, _) => {
                            let source = self.source(operand);
                            self.with_shown(&source, |v| {
                                display(v, &mut text, false, &self.module.types)
                            });
                        }
                    }
                }
                Value::str(&text)
            }
            Rvalue::Concat(lhs, rhs) => {
                let (a, b) = (self.string(lhs), self.string(rhs));
                let mut text = String::with_capacity(a.len() + b.len());
                text.push_str(&a);
                text.push_str(&b);
                Value::str(&text)
            }
            Rvalue::MakeArray { items, .. } => {
                Value::Array(items.iter().map(|item| self.take(item)).collect())
            }
            Rvalue::MakeDict { entries, .. } => {
                let mut dict = Dict::default();
                for (key, value) in entries {
                    let key = self.take(key);
                    let value = self.take(value);
                    dict.set(key, value);
                }
                Value::Dict(Box::new(dict))
            }
            Rvalue::MakeUnion { tag, value } => Value::Union(*tag, Box::new(self.take(value))),
            Rvalue::MakeRecord { ty, fields } => {
                let id = ty.as_named().expect("structure type").id;
                Value::Record(id, fields.iter().map(|field| self.take(field)).collect())
            }
            Rvalue::MakeVariant {
                ty,
                variant,
                fields,
            } => {
                let id = ty.as_named().expect("enum type").id;
                Value::Variant(
                    id,
                    *variant,
                    fields.iter().map(|field| self.take(field)).collect(),
                )
            }
            Rvalue::VariantIs { value, variant } => {
                let source = self.source(value);
                Value::Bool(self.with_source(
                    &source,
                    |v| matches!(v, Value::Variant(_, current, _) if current == variant),
                ))
            }
            Rvalue::TagIs { value, tag } => {
                let source = self.source(value);
                Value::Bool(
                    self.with_source(&source, |v| matches!(v, Value::Union(t, _) if t == tag)),
                )
            }
            Rvalue::UnionTypeName(value) => {
                let source = self.source(value);
                let tag = self.with_source(&source, |v| match v {
                    Value::Union(tag, _) => *tag,
                    other => panic!("type name of a non-union value {other:?}"),
                });
                let name = self
                    .module
                    .union_member(tag)
                    .map(Type::type_name)
                    .unwrap_or_default();
                Value::str(&name)
            }
            Rvalue::Length { value, .. } => {
                let source = self.source(value);
                let length = self.with_source(&source, |v| match v {
                    Value::Str(text) => text.chars().count(),
                    Value::Array(items) => items.len(),
                    Value::Dict(dict) => dict.len(),
                    Value::View(view) => view.length,
                    other => panic!("length of {other:?}"),
                });
                Value::Int(length as i128)
            }
            Rvalue::ByteLength(value) => Value::Int(self.string(value).len() as i128),
            Rvalue::HasKey { dict, key } => {
                let key = self.take(key);
                let source = self.source(dict);
                Value::Bool(self.with_source(&source, |v| match v {
                    Value::Dict(dict) => dict.find(&key).is_some(),
                    other => panic!("haskey on {other:?}"),
                }))
            }
            Rvalue::Clone { value, .. } => {
                let source = self.source(value);
                self.with_shown(&source, Value::clone)
            }
            Rvalue::ErrorMessage(value) | Rvalue::ErrorCode(value) => {
                let message = matches!(rvalue, Rvalue::ErrorMessage(_));
                let source = self.source(value);
                let text = self.with_source(&source, |v| match v {
                    Value::Error(error) => {
                        if message {
                            error.message.clone()
                        } else {
                            error.code.clone()
                        }
                    }
                    other => panic!("error field of {other:?}"),
                });
                Value::str(&text)
            }
            Rvalue::RangeLength(range) => {
                let source = self.source(range);
                let (start, end, step) = self.with_source(&source, range_parts);
                Value::Int(range_length(start, end, step) as i128)
            }
            Rvalue::RangeAt { range, index } => {
                let index = self.int(index);
                let source = self.source(range);
                let (start, _, step) = self.with_source(&source, range_parts);
                Value::Int(start as i128 + index * step as i128)
            }
            Rvalue::Increment(value) => Value::Int(self.int(value) + 1),
            Rvalue::RemoveKey {
                dict,
                key,
                value_tag,
                nothing_tag,
            } => {
                let key = self.take(key);
                let reference = self.resolve(dict);
                let removed = self.with_mut(&reference, |target| match target {
                    Value::Dict(dict) => dict.remove(&key),
                    other => panic!("remove from {other:?}"),
                });
                let (value, tag) = match removed {
                    Some(value) => (value, *value_tag),
                    None => (Value::Nothing, *nothing_tag),
                };
                match tag {
                    Some(tag) => Value::Union(tag, Box::new(value)),
                    None => value,
                }
            }
            Rvalue::NextChar { text, offset } => {
                let text = self.string(text);
                let reference = self.resolve(offset);
                let start = self.read(&reference, |v| v.as_int()) as usize;
                let ch = text[start..].chars().next().expect("a character");
                self.with_mut(&reference, |v| {
                    *v = Value::Int((start + ch.len_utf8()) as i128);
                });
                Value::str(ch.encode_utf8(&mut [0; 4]))
            }
            Rvalue::MakeShared { value, .. } => {
                Value::Shared(Rc::new(RefCell::new(self.take(value))))
            }
            Rvalue::MakeMutex { value, .. } => {
                let value = self.take(value);
                // Tasks on other threads may take the value out.
                self.cross_channels();
                Value::Mutex(MutexCell::new(&value))
            }
            Rvalue::Downgrade(shared) => {
                let source = self.source(shared);
                self.with_source(&source, |value| match value {
                    Value::Shared(shared) => Value::Weak(Rc::downgrade(shared)),
                    other => panic!("weak handle of {other:?}"),
                })
            }
            Rvalue::Upgrade {
                weak,
                shared_tag,
                nothing_tag,
            } => {
                let source = self.source(weak);
                let upgraded = self.with_source(&source, |value| match value {
                    Value::Weak(weak) => weak.upgrade(),
                    other => panic!("upgrade of {other:?}"),
                });
                match upgraded {
                    Some(shared) => Value::Union(*shared_tag, Box::new(Value::Shared(shared))),
                    None => Value::Union(*nothing_tag, Box::new(Value::Nothing)),
                }
            }
            Rvalue::MakeClosure {
                function,
                env,
                captures,
            } => {
                let env = match env {
                    Some(ty) => {
                        let id = ty.as_named().expect("environment structure").id;
                        Value::Record(id, captures.iter().map(|value| self.take(value)).collect())
                    }
                    None => Value::Nothing,
                };
                Value::Function(*function, Box::new(env))
            }
            Rvalue::Pop {
                array,
                element_tag,
                nothing_tag,
            } => {
                let reference = self.resolve(array);
                let popped = self.with_mut(&reference, |target| match target {
                    Value::Array(items) => items.pop(),
                    other => panic!("pop from {other:?}"),
                });
                let (value, tag) = match popped {
                    Some(value) => (value, *element_tag),
                    None => (Value::Nothing, *nothing_tag),
                };
                match tag {
                    Some(tag) => Value::Union(tag, Box::new(value)),
                    None => value,
                }
            }
        }
    }

    // ----- fallible operations ---------------------------------------------------

    fn location(&self, span: Span) -> String {
        let file = self.sources.get(span.file);
        let (line, column) = file.line_col(span.start);
        format!("{}:{line}:{column}", file.name)
    }

    fn error(&self, code: &str, message: impl Into<String>, span: Span) -> ErrorValue {
        ErrorValue {
            code: code.to_string(),
            message: message.into(),
            location: Some(self.location(span)),
        }
    }

    fn operation(&mut self, op: &Op, span: Span) -> Result<Value, ErrorValue> {
        match op {
            Op::Call { .. } | Op::CallValue { .. } => {
                unreachable!("calls are handled by the frame loop")
            }
            Op::Spawn { .. }
            | Op::Join { .. }
            | Op::Detach { .. }
            | Op::Yield
            | Op::Sleep { .. }
            | Op::ChannelNew { .. }
            | Op::Send { .. }
            | Op::Receive { .. }
            | Op::Close { .. }
            | Op::Select { .. } => unreachable!("task operations are handled by the frame loop"),
            Op::IntArith { op, ty, lhs, rhs } => {
                let (a, b) = (self.int(lhs), self.int(rhs));
                let symbol = match op {
                    ArithOp::Add => "+",
                    ArithOp::Subtract => "-",
                    ArithOp::Multiply => "*",
                    ArithOp::Divide => "/",
                    ArithOp::Remainder => "%",
                };
                if matches!(op, ArithOp::Divide | ArithOp::Remainder) && b == 0 {
                    return Err(self.error(
                        "DivisionByZeroError",
                        "integer division by zero",
                        span,
                    ));
                }
                let result = match op {
                    ArithOp::Add => a.checked_add(b),
                    ArithOp::Subtract => a.checked_sub(b),
                    ArithOp::Multiply => a.checked_mul(b),
                    ArithOp::Divide => a.checked_div(b),
                    ArithOp::Remainder => a.checked_rem(b),
                };
                match result {
                    Some(value) if value >= ty.min_value() && value <= ty.max_value() => {
                        Ok(Value::Int(value))
                    }
                    _ => Err(self.error(
                        "OverflowError",
                        format!("{a} {symbol} {b} does not fit in {}", ty.name()),
                        span,
                    )),
                }
            }
            Op::IntNegate { ty, value } => {
                let a = self.int(value);
                let result = -a;
                if result < ty.min_value() || result > ty.max_value() {
                    return Err(self.error(
                        "OverflowError",
                        format!("-({a}) does not fit in {}", ty.name()),
                        span,
                    ));
                }
                Ok(Value::Int(result))
            }
            Op::Convert { from, to, value } => {
                let value = self.take(value);
                self.convert(value, from, to, span)
            }
            Op::CheckIndex { array, index } => {
                let index = self.int(index);
                let source = self.source(array);
                let length = self.with_source(&source, |v| match v {
                    Value::Array(items) => items.len(),
                    Value::View(view) => view.length,
                    other => panic!("index into {other:?}"),
                });
                if index < 0 || index >= length as i128 {
                    let message = if length == 0 {
                        format!("index {index} is out of bounds: the array is empty")
                    } else {
                        format!("index {index} is out of bounds for an array of length {length}")
                    };
                    return Err(self.error("BoundsError", message, span));
                }
                Ok(Value::Int(index))
            }
            Op::MakeView { base, start, end } => {
                let start = start.as_ref().map(|s| self.int(s));
                let end = end.as_ref().map(|e| self.int(e));
                let reference = self.flatten(&self.resolve(base));
                let (array, offset, length) = self.read(&reference, |v| match v {
                    Value::Array(items) => (reference.clone(), 0, items.len()),
                    Value::Str(text) => (reference.clone(), 0, text.len()),
                    Value::View(view) => (view.array.clone(), view.start, view.length),
                    other => panic!("view of {other:?}"),
                });
                let start = start.unwrap_or(0);
                let end = end.unwrap_or(length as i128);
                if start < 0 || start > end || end > length as i128 {
                    return Err(self.error(
                        "BoundsError",
                        view_bounds_message(start, end, length),
                        span,
                    ));
                }
                Ok(Value::View(Box::new(ViewValue {
                    array,
                    start: offset + start as usize,
                    length: (end - start) as usize,
                })))
            }
            Op::SplitView { view, count } => {
                let count = self.int(count);
                let reference = self.resolve(view);
                let length = self.read(&reference, |value| match value {
                    Value::View(view) => view.length,
                    other => panic!("split of {other:?}"),
                });
                if count < 0 || count > length as i128 {
                    return Err(self.error(
                        "BoundsError",
                        split_bounds_message(count, length),
                        span,
                    ));
                }
                let count = count as usize;
                self.with_mut(&reference, |value| match value {
                    Value::View(rest) => {
                        let part = ViewValue {
                            array: rest.array.clone(),
                            start: rest.start,
                            length: count,
                        };
                        rest.start += count;
                        rest.length -= count;
                        Ok(Value::View(Box::new(part)))
                    }
                    other => panic!("split of {other:?}"),
                })
            }
            Op::Swap {
                array,
                first,
                second,
            } => {
                let (first, second) = (self.int(first), self.int(second));
                let reference = self.resolve(array);
                let length = self.read(&reference, |v| match v {
                    Value::Array(items) => items.len(),
                    other => panic!("swap in {other:?}"),
                });
                for index in [first, second] {
                    if index < 0 || index >= length as i128 {
                        let message = if length == 0 {
                            format!("index {index} is out of bounds: the array is empty")
                        } else {
                            format!(
                                "index {index} is out of bounds for an array of length {length}"
                            )
                        };
                        return Err(self.error("BoundsError", message, span));
                    }
                }
                self.with_mut(&reference, |target| match target {
                    Value::Array(items) => items.swap(first as usize, second as usize),
                    other => panic!("swap in {other:?}"),
                });
                Ok(Value::Nothing)
            }
            Op::RemoveAt { array, index } => {
                let index = self.int(index);
                let reference = self.resolve(array);
                let length = self.read(&reference, |v| match v {
                    Value::Array(items) => items.len(),
                    other => panic!("remove from {other:?}"),
                });
                if index < 0 || index >= length as i128 {
                    let message = if length == 0 {
                        format!("index {index} is out of bounds: the array is empty")
                    } else {
                        format!("index {index} is out of bounds for an array of length {length}")
                    };
                    return Err(self.error("BoundsError", message, span));
                }
                Ok(self.with_mut(&reference, |target| match target {
                    Value::Array(items) => items.remove(index as usize),
                    other => panic!("remove from {other:?}"),
                }))
            }
            Op::CharAt { text, index } => {
                let text = self.string(text);
                let position = self.int(index);
                let found = usize::try_from(position)
                    .ok()
                    .and_then(|position| text.chars().nth(position));
                match found {
                    Some(ch) => Ok(Value::str(ch.encode_utf8(&mut [0; 4]))),
                    None => {
                        let message = position_error(position, text.chars().count());
                        Err(self.error("BoundsError", message, span))
                    }
                }
            }
            Op::DictFind { dict, key } => {
                let key = self.take(key);
                let source = self.source(dict);
                let position = self.with_source(&source, |v| match v {
                    Value::Dict(dict) => dict.find(&key),
                    other => panic!("lookup in {other:?}"),
                });
                match position {
                    Some(position) => Ok(Value::Int(position as i128)),
                    None => {
                        let mut shown = String::new();
                        display(&key, &mut shown, true, &self.module.types);
                        Err(self.error(
                            "KeyError",
                            format!("the key {shown} is not in the dictionary"),
                            span,
                        ))
                    }
                }
            }
            Op::Print(values) => {
                let mut line = String::new();
                for (index, (operand, _)) in values.iter().enumerate() {
                    if index > 0 {
                        line.push(' ');
                    }
                    let source = self.source(operand);
                    self.with_shown(&source, |v| {
                        display(v, &mut line, false, &self.module.types)
                    });
                }
                line.push('\n');
                self.output().write_all(line.as_bytes()).map_err(|error| {
                    self.error("IOError", format!("could not write output: {error}"), span)
                })?;
                Ok(Value::Nothing)
            }
            Op::Assert { condition, message } => {
                let holds = {
                    let source = self.source(condition);
                    self.with_source(&source, |v| v.as_bool())
                };
                if holds {
                    return Ok(Value::Nothing);
                }
                let text = match message {
                    Some(message) => self.string(message).to_string(),
                    None => "assertion failed".to_string(),
                };
                Err(self.error("AssertionError", text, span))
            }
            Op::MakeRange { start, end, step } => {
                let start = self.int(start);
                let end = self.int(end);
                let step = match step {
                    Some(step) => self.int(step),
                    None => 1,
                };
                if step == 0 {
                    return Err(self.error("RangeError", "a range step cannot be zero", span));
                }
                Ok(Value::Range {
                    start: start as i64,
                    end: end as i64,
                    step: step as i64,
                })
            }
            Op::ToNumber {
                text,
                integer_tag,
                float_tag,
            } => {
                let text = self.string(text);
                match parse_number(&text) {
                    Ok(Number::Integer(value)) => Ok(Value::Union(
                        *integer_tag,
                        Box::new(Value::Int(value as i128)),
                    )),
                    Ok(Number::Float(value)) => {
                        Ok(Value::Union(*float_tag, Box::new(Value::F64(value))))
                    }
                    Err((code, message)) => Err(self.error(code, message, span)),
                }
            }
            Op::ReadFile(path) => {
                let path = self.string(path);
                let bytes = std::fs::read(&*path).map_err(|error| {
                    self.error(
                        "IOError",
                        format!("could not read \"{path}\": {}", io_message(&error)),
                        span,
                    )
                })?;
                let text = String::from_utf8(bytes).map_err(|error| {
                    self.error(
                        "EncodingError",
                        format!(
                            "\"{path}\" is not valid UTF-8 (byte {})",
                            error.utf8_error().valid_up_to()
                        ),
                        span,
                    )
                })?;
                Ok(Value::str(&text))
            }
            Op::WriteFile { path, content } => {
                let path = self.string(path);
                let content = self.string(content);
                std::fs::write(&*path, content.as_bytes()).map_err(|error| {
                    self.error(
                        "IOError",
                        format!("could not write \"{path}\": {}", io_message(&error)),
                        span,
                    )
                })?;
                Ok(Value::Nothing)
            }
            Op::Fail(message) => {
                let message = self.string(message);
                Err(self.error("UserError", message.to_string(), span))
            }
            Op::Host {
                function,
                args,
                tags,
            } => self.host(*function, args, *tags, span),
            Op::CallForeign { function, args } => {
                let values: Vec<(Value, Type)> = args
                    .iter()
                    .map(|(arg, ty)| {
                        let source = self.source(arg);
                        (self.with_source(&source, Value::clone), ty.clone())
                    })
                    .collect();
                // What the program printed comes before what C prints.
                let _ = self.output().flush();
                let declaration = &self.module.foreign[function.0 as usize];
                self.foreign
                    .call(function.0, declaration, &values)
                    .map_err(|message| self.error("ForeignError", message, span))
            }
            Op::RawRead {
                address,
                offset,
                ty,
            } => {
                let address = self.address(address, offset, "read from", span)?;
                // SAFETY: unsafe code vouched for the memory.
                Ok(unsafe { crate::foreign::read(address, ty) })
            }
            Op::RawWrite {
                address,
                offset,
                value,
                ty,
            } => {
                let address = self.address(address, offset, "write to", span)?;
                let source = self.source(value);
                let value = self.with_source(&source, Value::clone);
                // SAFETY: unsafe code vouched for the memory.
                unsafe { crate::foreign::write(address, &value, ty) };
                Ok(Value::Nothing)
            }
            Op::Raise(problem) => match self.take(problem) {
                Value::Error(error) => Err(*error),
                other => panic!("raise of {other:?}"),
            },
            Op::MutexTake { mutex } => self.mutex_take(mutex, span),
            Op::SpawnParallel { .. } => {
                unreachable!("task operations are handled by the frame loop")
            }
        }
    }

    /// The address `offset` bytes after a pointer, or the error for
    /// `null_pointer`.
    fn address(
        &mut self,
        address: &Operand,
        offset: &Operand,
        action: &str,
        span: Span,
    ) -> Result<usize, ErrorValue> {
        let base = match self.take(address) {
            Value::Pointer(address) => address,
            other => panic!("address {other:?}"),
        };
        if base == 0 {
            return Err(self.error(
                "NullPointerError",
                format!("cannot {action} null_pointer"),
                span,
            ));
        }
        let offset = self.int(offset) as i64;
        Ok(base.wrapping_add_signed(offset as isize))
    }

    /// Runs a host primitive through the runtime's shared implementation.
    /// Output goes through the interpreter's own writer so it stays in order
    /// with `print`.
    fn host(
        &mut self,
        function: Host,
        args: &[Operand],
        tags: Option<(u32, u32)>,
        span: Span,
    ) -> Result<Value, ErrorValue> {
        let values: Vec<Value> = args
            .iter()
            .map(|arg| {
                let source = self.source(arg);
                self.with_source(&source, Value::clone)
            })
            .collect();
        match function {
            Host::Write => {
                let text = values[0].as_str().to_string();
                let written = {
                    let mut out = self.output();
                    out.write_all(text.as_bytes()).and_then(|_| out.flush())
                };
                return written.map(|_| Value::Nothing).map_err(|error| {
                    self.error("IOError", format!("could not write output: {error}"), span)
                });
            }
            // A file of standard output writes where `print` does.
            Host::StreamWrite | Host::StreamWriteBytes | Host::StreamFlush
                if matches!(&values[0], Value::Stream(stream)
                    if sayon_runtime::streams::is_standard_output(stream.id())) =>
            {
                let written = match &values[1..] {
                    [Value::Str(text)] => self.output().write_all(text.as_bytes()),
                    [Value::Array(bytes)] => {
                        let bytes: Vec<u8> = bytes.iter().map(|b| b.as_int() as u8).collect();
                        self.output().write_all(&bytes)
                    }
                    _ => self.output().flush(),
                };
                return written.map(|_| Value::Nothing).map_err(|error| {
                    self.error("IOError", format!("could not write output: {error}"), span)
                });
            }
            // What the program printed shows before it waits for input or
            // another program writes.
            Host::WriteError
            | Host::ReadLine
            | Host::Start
            | Host::StreamReadLine
            | Host::StreamRead
            | Host::StreamReadBytes
            | Host::StreamReadText
            | Host::StreamCopy => {
                let _ = self.output().flush();
            }
            Host::StreamWrite | Host::StreamWriteBytes
                if matches!(&values[0], Value::Stream(stream)
                    if sayon_runtime::streams::is_standard_error(stream.id())) =>
            {
                let _ = self.output().flush();
            }
            _ => {}
        }
        let host_args: Vec<HostArg> = values
            .iter()
            .zip(function.params())
            .map(|(value, ty)| match (ty, value) {
                (HostType::String, Value::Str(text)) => HostArg::String(text),
                (HostType::Integer, Value::Int(n)) => HostArg::Integer(*n as i64),
                (HostType::Float, Value::F64(x)) => HostArg::Float(*x),
                (HostType::Boolean, Value::Bool(b)) => HostArg::Boolean(*b),
                (HostType::StringArray, Value::Array(items)) => {
                    HostArg::Strings(items.iter().map(Value::as_str).collect())
                }
                (HostType::Stream, Value::Stream(stream)) => HostArg::Stream(stream.id()),
                (HostType::Bytes, Value::Array(items)) => {
                    HostArg::Bytes(items.iter().map(|item| item.as_int() as u8).collect())
                }
                (HostType::Pointer, Value::Pointer(address)) => HostArg::Pointer(*address),
                (HostType::CString, Value::CString(bytes)) => {
                    HostArg::Pointer(bytes.as_ptr() as usize)
                }
                (ty, value) => panic!("host argument {value:?} for {ty:?}"),
            })
            .collect();
        match invoke(function, &host_args) {
            Ok(HostValue::String(text)) => Ok(Value::str(&text)),
            Ok(HostValue::Integer(n)) => Ok(Value::Int(n as i128)),
            Ok(HostValue::Float(x)) => Ok(Value::F64(x)),
            Ok(HostValue::Boolean(b)) => Ok(Value::Bool(b)),
            Ok(HostValue::Nothing) => Ok(Value::Nothing),
            Ok(HostValue::Strings(items)) => {
                Ok(Value::Array(items.iter().map(|s| Value::str(s)).collect()))
            }
            Ok(HostValue::Stream(id)) => Ok(Value::Stream(StreamRef::adopt(id))),
            Ok(HostValue::Unsigned(value)) => Ok(Value::Int(value as i128)),
            Ok(HostValue::Pointer(address)) => Ok(Value::Pointer(address)),
            Ok(HostValue::CString(mut bytes)) => {
                bytes.push(0);
                Ok(Value::CString(bytes.into()))
            }
            Ok(HostValue::Bytes(bytes)) => Ok(Value::Array(
                bytes
                    .into_iter()
                    .map(|byte| Value::Int(byte as i128))
                    .collect(),
            )),
            Ok(HostValue::OptionalString(text)) => {
                let (string_tag, nothing_tag) = tags.expect("tags for an optional string");
                Ok(match text {
                    Some(text) => Value::Union(string_tag, Box::new(Value::str(&text))),
                    None => Value::Union(nothing_tag, Box::new(Value::Nothing)),
                })
            }
            Err((code, message)) => Err(self.error(code, message, span)),
        }
    }

    fn convert(
        &self,
        value: Value,
        from: &Type,
        to: &Type,
        span: Span,
    ) -> Result<Value, ErrorValue> {
        let fail = |message: String| Err(self.error("ConversionError", message, span));
        let float_to_int = |x: f64, int: IntType| {
            if !x.is_finite() {
                return fail(format!(
                    "{} is not a finite number, so it has no {} value",
                    format_f64(x),
                    int.name()
                ));
            }
            let truncated = x.trunc();
            let lower = int.min_value() as f64;
            let upper_exclusive = (int.max_value() + 1) as f64;
            if truncated < lower || truncated >= upper_exclusive {
                return fail(format!("{} does not fit in {}", format_f64(x), int.name()));
            }
            Ok(Value::Int(truncated as i128))
        };
        match (value, to) {
            (Value::Int(x), Type::Int(int)) => {
                if x < int.min_value() || x > int.max_value() {
                    return fail(format!("{x} does not fit in {}", int.name()));
                }
                Ok(Value::Int(x))
            }
            (Value::Int(x), Type::Float(FloatType::F64)) => Ok(Value::F64(x as f64)),
            (Value::Int(x), Type::Float(FloatType::F32)) => Ok(Value::F32(x as f32)),
            (Value::F64(x), Type::Int(int)) => float_to_int(x, *int),
            (Value::F32(x), Type::Int(int)) => float_to_int(x as f64, *int),
            (Value::F64(x), Type::Float(FloatType::F64)) => Ok(Value::F64(x)),
            (Value::F64(x), Type::Float(FloatType::F32)) => {
                let narrowed = x as f32;
                if x.is_finite() && narrowed.is_infinite() {
                    return fail(format!("{} is too large for float32", format_f64(x)));
                }
                Ok(Value::F32(narrowed))
            }
            (Value::F32(x), Type::Float(FloatType::F64)) => Ok(Value::F64(x as f64)),
            (Value::F32(x), Type::Float(FloatType::F32)) => Ok(Value::F32(x)),
            (_, _) => fail(format!("cannot convert {from} to {to}")),
        }
    }
}

fn float_op(op: ArithOp, x: f64, y: f64) -> f64 {
    match op {
        ArithOp::Add => x + y,
        ArithOp::Subtract => x - y,
        ArithOp::Multiply => x * y,
        ArithOp::Divide => x / y,
        ArithOp::Remainder => x % y,
    }
}

fn range_parts(value: &Value) -> (i64, i64, i64) {
    match value {
        Value::Range { start, end, step } => (*start, *end, *step),
        other => panic!("expected a range, found {other:?}"),
    }
}

fn io_message(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "the file does not exist".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => error.to_string(),
    }
}
