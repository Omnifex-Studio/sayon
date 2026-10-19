//! Tasks on threads of their own, and the mutexes they share.
//!
//! A task on another thread runs in a machine of its own, over the same
//! module and output. What crosses to it, the values its function captured
//! and the program's constants, and its result coming back, is copied
//! first, so that it shares nothing with the thread it left: the
//! interpreter's values count their references without atomics. A mutex
//! keeps its value the same way, copied in and out under its lock.
//!
//! Every thread is joined before the program's run returns, which is what
//! lets the threads borrow the module and the output of the machine that
//! started them.

use super::*;
use sayon_runtime::network::{self, Background};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex as Lock, MutexGuard};
use tasks::Wait;

pub(super) fn lock<T>(mutex: &Lock<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A value that shares nothing with any other, so that it can go to
/// another thread.
pub struct Sent(Value);

// SAFETY: made only by `Sent::new`, which copies every part of the value
// that counts references, so no count is shared with a value of another
// thread; a mutex's or a parallel task's state is shared through an `Arc`.
unsafe impl Send for Sent {}

impl Sent {
    pub fn new(value: &Value) -> Sent {
        Sent(isolate(value))
    }

    /// The value, for the thread that receives it.
    pub fn take(self) -> Value {
        self.0
    }
}

/// A copy of a value with no part counted with the original.
pub(super) fn isolate(value: &Value) -> Value {
    match value {
        Value::Int(_)
        | Value::F64(_)
        | Value::F32(_)
        | Value::Bool(_)
        | Value::Nothing
        | Value::Range { .. }
        | Value::Pointer(_)
        | Value::Error(_)
        | Value::Stream(_)
        | Value::Mutex(_) => value.clone(),
        Value::Channel(channel) => {
            cross(channel);
            value.clone()
        }
        Value::Str(text) => Value::str(text),
        Value::CString(bytes) => Value::CString(bytes.to_vec().into()),
        Value::Array(items) => Value::Array(items.iter().map(isolate).collect()),
        Value::Dict(dict) => {
            let mut copy = Dict::default();
            for (key, value) in dict.keys.iter().zip(&dict.values) {
                copy.set(isolate(key), isolate(value));
            }
            Value::Dict(Box::new(copy))
        }
        Value::Union(tag, payload) => Value::Union(*tag, Box::new(isolate(payload))),
        Value::Record(id, fields) => Value::Record(*id, fields.iter().map(isolate).collect()),
        Value::Variant(id, variant, fields) => {
            Value::Variant(*id, *variant, fields.iter().map(isolate).collect())
        }
        Value::Function(code, environment) => {
            Value::Function(*code, Box::new(isolate(environment)))
        }
        Value::View(_)
        | Value::Shared(_)
        | Value::Weak(_)
        | Value::Task(_)
        | Value::ParallelTask(_) => {
            panic!("the checker lets only values that can be sent cross threads, not {value:?}")
        }
    }
}

/// Lets a channel's handles go to other threads: from now on its messages
/// are copied as they are sent, and the ones waiting in it are copied now,
/// on the one thread that can reach it so far.
fn cross(channel: &Channel) {
    if channel.0.crossed.swap(true, Ordering::AcqRel) {
        return;
    }
    // Copied outside the lock, since a message may hold the channel itself.
    let waiting = std::mem::take(&mut channel.state().queue);
    let copied: std::collections::VecDeque<Value> = waiting.iter().map(isolate).collect();
    channel.state().queue = copied;
}

impl<'m> Machine<'m> {
    /// Records a channel made on this thread, which no task on another
    /// thread can hold yet.
    pub(super) fn local_channel(&mut self, channel: &Channel) {
        if self.local_channels.len() >= self.prune_channels_at {
            self.local_channels.retain(|weak| weak.strong_count() > 0);
            self.prune_channels_at = (self.local_channels.len() * 2).max(64);
        }
        self.local_channels.push(Arc::downgrade(&channel.0));
    }

    /// Counts every channel of this thread as one that tasks on other
    /// threads may hold. A channel handle can leave its thread only with a
    /// value that does: what a task on another thread captures or returns,
    /// the value of a mutex, or a message on a channel that crossed
    /// already. As in compiled programs, which do not see inside those
    /// values, each of them crosses all of the thread's channels.
    pub(super) fn cross_channels(&mut self) {
        network::prepare_waking();
        for weak in std::mem::take(&mut self.local_channels) {
            if let Some(cell) = weak.upgrade() {
                cross(&Channel(cell));
            }
        }
    }

    /// Whether tasks run on threads other than this one.
    pub(super) fn others_run(&self) -> bool {
        self.thread_task.is_some() || self.program.running.load(Ordering::Acquire) > 0
    }
}

// ----- mutexes -------------------------------------------------------------------------------

/// A mutex: its value, and which task holds its lock.
pub struct MutexCell {
    state: Lock<MutexState>,
    free: Condvar,
}

struct MutexState {
    /// Taken out while a task holds the lock.
    value: Option<Sent>,
    holder: Option<(std::thread::ThreadId, usize)>,
}

impl std::fmt::Debug for MutexCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MutexCell")
    }
}

impl MutexCell {
    pub fn new(value: &Value) -> Arc<MutexCell> {
        Arc::new(MutexCell {
            state: Lock::new(MutexState {
                value: Some(Sent::new(value)),
                holder: None,
            }),
            free: Condvar::new(),
        })
    }
}

impl<'m> Machine<'m> {
    /// Takes a mutex's lock, waiting for another thread to free it, and its
    /// value. A task holds a lock only between operations that never wait,
    /// so no task of this thread can hold it meanwhile.
    pub(super) fn mutex_take(&mut self, mutex: &Operand, span: Span) -> Result<Value, ErrorValue> {
        let source = self.source(mutex);
        let cell = self.with_source(&source, |value| match value {
            Value::Mutex(cell) => cell.clone(),
            other => panic!("lock of {other:?}"),
        });
        let me = (std::thread::current().id(), self.current);
        let mut state = lock(&cell.state);
        while let Some(holder) = state.holder {
            if holder == me {
                return Err(self.error(
                    "LockError",
                    "the task holds the mutex's lock already",
                    span,
                ));
            }
            state = cell.free.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.holder = Some(me);
        let value = state
            .value
            .take()
            .expect("the value of a free mutex")
            .take();
        self.fibers[self.current].locks += 1;
        Ok(value)
    }

    /// Puts a mutex's value back and frees its lock.
    pub(super) fn mutex_put(&mut self, mutex: &Operand, value: &Operand) {
        let value = self.take(value);
        self.cross_channels();
        let source = self.source(mutex);
        let cell = self.with_source(&source, |value| match value {
            Value::Mutex(cell) => cell.clone(),
            other => panic!("unlock of {other:?}"),
        });
        let mut state = lock(&cell.state);
        state.value = Some(Sent::new(&value));
        state.holder = None;
        drop(state);
        cell.free.notify_one();
        let fiber = &mut self.fibers[self.current];
        fiber.locks = fiber.locks.saturating_sub(1);
    }
}

// ----- tasks on threads of their own ---------------------------------------------------------

/// What a parallel task's handle and the program know of it.
#[derive(Default)]
pub struct ParallelState {
    result: Lock<Option<Result<Sent, ErrorValue>>>,
    pub(super) cancel: AtomicBool,
    joined: AtomicBool,
    detached: AtomicBool,
}

impl Background for ParallelState {
    fn ready(&self) -> bool {
        lock(&self.result).is_some()
    }
}

/// The handle of a task on another thread. When the last copy of the handle
/// goes without a join or a detach, the task is cancelled.
#[derive(Clone)]
pub struct ParallelHandle(Arc<HandleState>);

/// What the copies of a handle share; the task's thread and the program
/// hold the state itself.
struct HandleState(Arc<ParallelState>);

impl ParallelHandle {
    pub(super) fn state(&self) -> &Arc<ParallelState> {
        &self.0.0
    }
}

impl std::fmt::Debug for ParallelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ParallelHandle")
    }
}

impl Drop for HandleState {
    fn drop(&mut self) {
        let state = &self.0;
        if !state.joined.load(Ordering::Relaxed) && !state.detached.load(Ordering::Relaxed) {
            state.cancel.store(true, Ordering::Relaxed);
            network::wake();
        }
    }
}

/// What the machines of a program's threads share.
#[derive(Default)]
pub struct Program {
    threads: Lock<Vec<(Arc<ParallelState>, std::thread::JoinHandle<()>)>>,
    /// Set by `os.exit` in any task, which ends the whole program.
    exit: Lock<Option<i32>>,
    exiting: AtomicBool,
    /// Failures of tasks nothing joined, from the threads' own tasks.
    failures: Lock<Vec<ErrorValue>>,
    /// The parallel tasks that have not finished.
    pub(super) running: std::sync::atomic::AtomicUsize,
}

impl Program {
    pub(super) fn exiting(&self) -> bool {
        self.exiting.load(Ordering::Relaxed)
    }

    pub(super) fn exit_with(&self, status: i32) {
        lock(&self.exit).get_or_insert(status);
        self.exiting.store(true, Ordering::Relaxed);
        network::wake();
    }

    pub(super) fn exit_status(&self) -> Option<i32> {
        *lock(&self.exit)
    }
}

/// Joins every thread of a program when dropped, also when the machine
/// that started them unwinds, so that no thread outlives what it borrows.
pub(super) struct JoinThreads(pub(super) Arc<Program>);

impl Drop for JoinThreads {
    fn drop(&mut self) {
        loop {
            let next = lock(&self.0.threads).pop();
            let Some((state, handle)) = next else {
                break;
            };
            state.cancel.store(true, Ordering::Relaxed);
            network::wake();
            let _ = handle.join();
        }
    }
}

impl<'m> Machine<'m> {
    /// Starts a task on a thread of its own, running a function value with
    /// copies of what it captured and of the program's constants.
    pub(super) fn spawn_parallel(&mut self, body: &Operand, span: Span) -> Progress {
        let (code, environment) = match self.take(body) {
            Value::Function(code, environment) => (code, *environment),
            other => panic!("spawn of {other:?}"),
        };
        let environment = Sent::new(&environment);
        let globals: Vec<Option<Sent>> = self
            .globals
            .iter()
            .map(|global| global.as_ref().map(Sent::new))
            .collect();
        // The channels of this thread may go with what the task captured.
        self.cross_channels();
        let state = Arc::new(ParallelState::default());
        let shared = state.clone();
        let module = self.module;
        let sources = self.sources;
        let out = self.out.clone();
        let program = self.program.clone();
        network::prepare_waking();
        let body = move || {
            network::prepare_waking();
            let mut machine = Machine::for_thread(module, sources, out, program, shared.clone());
            machine.globals = globals.into_iter().map(|g| g.map(Sent::take)).collect();
            let result = machine.run_thread(code, environment.take());
            // The result may hold channels made on this thread.
            machine.cross_channels();
            *lock(&shared.result) = Some(result.map(|value| Sent::new(&value)));
            machine.program.running.fetch_sub(1, Ordering::AcqRel);
            network::wake();
        };
        // Counted before the thread can finish.
        self.program.running.fetch_add(1, Ordering::AcqRel);
        // SAFETY: the thread borrows the module, the sources, and the
        // output of this machine's run, and every thread of the program is
        // joined before that run returns (see `JoinThreads`).
        let started = unsafe {
            std::thread::Builder::new()
                .name("task".to_string())
                .spawn_unchecked(body)
        };
        match started {
            Ok(handle) => {
                lock(&self.program.threads).push((state.clone(), handle));
                let handle = ParallelHandle(Arc::new(HandleState(state)));
                Progress::Done(Value::ParallelTask(handle))
            }
            Err(error) => {
                self.program.running.fetch_sub(1, Ordering::AcqRel);
                Progress::Failed(self.error(
                    "ThreadError",
                    format!("no thread could be started for the task: {error}"),
                    span,
                ))
            }
        }
    }

    /// Runs the function of a task on this machine's thread, then ends the
    /// tasks it started here.
    fn run_thread(&mut self, code: FunctionId, environment: Value) -> Result<Value, ErrorValue> {
        let result = match self.new_frame(code, vec![SlotValue::Value(environment)]) {
            Ok(frame) => {
                self.frames.push(frame);
                self.execute(0)
            }
            Err(error) => Err(error),
        };
        self.shutdown();
        let failures = self.task_failures();
        lock(&self.program.failures).extend(failures);
        if let Some(status) = self.exit_status {
            self.program.exit_with(status);
        }
        match result {
            Ok(value) => Ok(value),
            Err(RunError::Uncaught(error)) => {
                if error.code == EXIT_CODE {
                    self.program.exit_with(exit_status(&error.message));
                }
                Err(error)
            }
            Err(RunError::TaskFailures(mut failures)) => Err(failures.remove(0)),
            Err(RunError::Fatal(message)) => Err(ErrorValue {
                code: "InternalError".to_string(),
                message,
                location: None,
            }),
        }
    }

    /// The result of a parallel task, once it has one: its value, copied
    /// for this thread, or its error.
    pub(super) fn join_parallel(&mut self, state: &Arc<ParallelState>) -> Progress {
        let result = lock(&state.result).take();
        match result {
            Some(Ok(value)) => {
                state.joined.store(true, Ordering::Relaxed);
                Progress::Done(value.take())
            }
            Some(Err(problem)) => {
                state.joined.store(true, Ordering::Relaxed);
                Progress::Failed(problem)
            }
            None => {
                network::prepare_waking();
                Progress::Wait(Wait::Io {
                    wait: sayon_runtime::network::IoWait::Background(state.clone()),
                    deadline: None,
                    timeout: None,
                })
            }
        }
    }

    pub(super) fn detach_parallel(state: &Arc<ParallelState>) {
        state.detached.store(true, Ordering::Relaxed);
    }

    /// At the end of the program: cancels the tasks on other threads, waits
    /// for every thread to finish, and gives the failures nothing joined.
    pub(super) fn finish_threads(&mut self) -> Vec<ErrorValue> {
        let states: Vec<Arc<ParallelState>> = lock(&self.program.threads)
            .iter()
            .map(|(state, _)| state.clone())
            .collect();
        for state in &states {
            state.cancel.store(true, Ordering::Relaxed);
        }
        network::wake();
        let mut finished = Vec::new();
        loop {
            let next = lock(&self.program.threads).pop();
            let Some((state, handle)) = next else {
                break;
            };
            // Also a task started on another thread meanwhile.
            state.cancel.store(true, Ordering::Relaxed);
            network::wake();
            let _ = handle.join();
            finished.push(state);
        }
        let mut failures = std::mem::take(&mut *lock(&self.program.failures));
        for state in finished {
            if state.joined.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(Err(problem)) = lock(&state.result).take()
                && problem.code != CANCELLED_CODE
                && problem.code != EXIT_CODE
            {
                failures.push(problem);
            }
        }
        failures
    }
}
