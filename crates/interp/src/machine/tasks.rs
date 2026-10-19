//! Tasks and channels: fibers that take turns on the machine.
//!
//! Each task is a fiber with a stack of frames of its own; the running one's
//! frames are the machine's. An operation that must wait, such as receiving
//! from an empty channel, leaves its task waiting at that operation and
//! switches to the next task that can go on, round the tasks in order. When
//! the waiting task is resumed it runs the operation again, which then
//! finds what it waited for; the few operations that wait after acting (a
//! rendezvous send, `pause`, and sleeping) are told how they were resumed
//! so that they finish instead of acting twice.

use super::*;
use sayon_runtime::network::{self, BLOCKED_CODE, IoWait};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(super) struct Fiber {
    pub(super) frames: Vec<Frame>,
    /// `None` for the main task, which has no handle.
    pub(super) task: Option<Rc<RefCell<TaskState>>>,
    pub(super) wait: Option<Wait>,
    pub(super) resume: Option<Resume>,
    pub(super) started: bool,
    pub(super) done: bool,
    /// Where `select` starts looking, after the channel it chose last.
    pub(super) cursor: usize,
    /// When the operation the task runs first waited for a socket, which
    /// its timeout counts from.
    pub(super) io_started: Option<Instant>,
    /// The mutexes whose locks the task holds, which it cannot wait while
    /// it holds.
    pub(super) locks: usize,
}

impl Fiber {
    pub(super) fn main() -> Fiber {
        Fiber {
            frames: Vec::new(),
            task: None,
            wait: None,
            resume: None,
            started: true,
            done: false,
            cursor: 0,
            io_started: None,
            locks: 0,
        }
    }
}

/// What a waiting task waits for.
pub(super) enum Wait {
    Receive(Rc<RefCell<ChannelState>>),
    Send(Rc<RefCell<ChannelState>>),
    /// The message sent as number `.1` to have been received.
    Rendezvous(Rc<RefCell<ChannelState>>, u64),
    Join(Rc<RefCell<TaskState>>),
    Select(Vec<Rc<RefCell<ChannelState>>>),
    Sleep(Instant),
    Yield,
    /// A socket, or a connection being made, until a deadline after which
    /// the operation fails, having waited for `timeout`.
    Io {
        wait: IoWait,
        deadline: Option<Instant>,
        timeout: Option<Duration>,
    },
}

impl Wait {
    fn ready(&self, now: Instant) -> bool {
        match self {
            Wait::Receive(channel) => channel.borrow().receivable(),
            Wait::Send(channel) => channel.borrow().has_room(),
            Wait::Rendezvous(channel, number) => channel.borrow().received >= *number,
            Wait::Join(task) => task.borrow().result.is_some(),
            Wait::Select(channels) => channels.iter().any(|c| c.borrow().receivable()),
            Wait::Sleep(until) => now >= *until,
            Wait::Yield => true,
            Wait::Io { wait, deadline, .. } => {
                deadline.is_some_and(|deadline| now >= deadline) || network::ready(wait)
            }
        }
    }
}

/// How a task that waited was resumed, for an operation that acts first and
/// then waits.
pub(super) enum Resume {
    Rendezvous(u64),
    Slept,
    Yielded,
    /// No task could ever go on; the waiting operation fails.
    Deadlock,
    /// A socket was not ready within its timeout.
    TimedOut(Duration),
}

/// The outcome of a task operation.
pub(super) enum Progress {
    Done(Value),
    Failed(ErrorValue),
    Wait(Wait),
}

fn channel_of(value: &Value) -> Rc<RefCell<ChannelState>> {
    match value {
        Value::Channel(channel) => channel.clone(),
        other => panic!("channel operation on {other:?}"),
    }
}

impl<'m> Machine<'m> {
    pub(super) fn is_task_op(op: &Op) -> bool {
        if let Op::Host { function, .. } = op {
            return function.may_wait();
        }
        matches!(
            op,
            Op::Spawn { .. }
                | Op::SpawnParallel { .. }
                | Op::Join { .. }
                | Op::Detach { .. }
                | Op::Yield
                | Op::Sleep { .. }
                | Op::ChannelNew { .. }
                | Op::Send { .. }
                | Op::Receive { .. }
                | Op::Close { .. }
                | Op::Select { .. }
        )
    }

    /// Whether the running task is to stop where it waits.
    fn cancelled(&self) -> bool {
        self.fiber_cancelled(self.current)
    }

    /// Whether fiber `index` is to stop where it waits: the program is
    /// ending, its task was cancelled, or, for the first fiber of a task's
    /// own thread, that task was.
    fn fiber_cancelled(&self, index: usize) -> bool {
        self.stopping
            || self.program.exiting()
            || self.fibers[index]
                .task
                .as_ref()
                .is_some_and(|task| task.borrow().cancel)
            || (index == 0
                && self
                    .thread_task
                    .as_ref()
                    .is_some_and(|task| task.cancel.load(std::sync::atomic::Ordering::Relaxed)))
    }

    fn task_error(&self, code: &str, message: String, span: Span) -> ErrorValue {
        self.error(code, message, span)
    }

    pub(super) fn task_op(&mut self, op: &Op, span: Span) -> Progress {
        let resume = self.fibers[self.current].resume.take();
        if let Op::Host {
            function,
            args,
            tags,
        } = op
        {
            return self.host_waiting(*function, args, *tags, resume, span);
        }
        if let Some(Resume::Deadlock) = resume {
            return Progress::Failed(self.task_error(
                "DeadlockError",
                "every task is waiting, so none can go on".to_string(),
                span,
            ));
        }
        let waits = !matches!(
            op,
            Op::Spawn { .. }
                | Op::SpawnParallel { .. }
                | Op::ChannelNew { .. }
                | Op::Close { .. }
                | Op::Detach { .. }
        );
        if waits && self.cancelled() {
            return Progress::Failed(self.task_error(
                CANCELLED_CODE,
                "the task was cancelled".to_string(),
                span,
            ));
        }
        match op {
            Op::Spawn { body, .. } => {
                let (code, env) = match self.take(body) {
                    Value::Function(code, env) => (code, *env),
                    other => panic!("spawn of {other:?}"),
                };
                // The function value's environment belongs to the task's
                // first frame, which reads its captures there.
                let frame = match self.new_frame(code, vec![SlotValue::Value(env)]) {
                    Ok(frame) => frame,
                    Err(_) => unreachable!("a new task's first frame"),
                };
                let state = Rc::new(RefCell::new(TaskState::default()));
                self.fibers.push(Fiber {
                    frames: vec![frame],
                    task: Some(state.clone()),
                    wait: None,
                    resume: None,
                    started: false,
                    done: false,
                    cursor: 0,
                    io_started: None,
                    locks: 0,
                });
                Progress::Done(Value::Task(TaskHandle(state)))
            }
            Op::SpawnParallel { body, .. } => self.spawn_parallel(body, span),
            Op::Join { task } => {
                let reference = self.resolve(task);
                let parallel = self.read(&reference, |value| match value {
                    Value::ParallelTask(handle) => Some(handle.state().clone()),
                    _ => None,
                });
                if let Some(state) = parallel {
                    return self.join_parallel(&state);
                }
                let state = self.read(&reference, |value| match value {
                    Value::Task(handle) => handle.0.clone(),
                    other => panic!("join of {other:?}"),
                });
                let result = {
                    let mut state = state.borrow_mut();
                    match state.result.take() {
                        Some(result) => {
                            state.joined = true;
                            Some(result)
                        }
                        None => None,
                    }
                };
                match result {
                    Some(Ok(value)) => Progress::Done(value),
                    Some(Err(problem)) => Progress::Failed(problem),
                    None => Progress::Wait(Wait::Join(state)),
                }
            }
            Op::Detach { task } => {
                let reference = self.resolve(task);
                self.read(&reference, |value| match value {
                    Value::Task(handle) => handle.0.borrow_mut().detached = true,
                    Value::ParallelTask(handle) => Self::detach_parallel(handle.state()),
                    other => panic!("detach of {other:?}"),
                });
                Progress::Done(Value::Nothing)
            }
            Op::Yield => match resume {
                Some(Resume::Yielded) => Progress::Done(Value::Nothing),
                _ => Progress::Wait(Wait::Yield),
            },
            Op::Sleep { nanoseconds } => {
                let nanoseconds = self.int(nanoseconds);
                match resume {
                    Some(Resume::Slept) => Progress::Done(Value::Nothing),
                    _ if nanoseconds <= 0 => Progress::Done(Value::Nothing),
                    _ => Progress::Wait(Wait::Sleep(
                        Instant::now() + Duration::from_nanos(nanoseconds as u64),
                    )),
                }
            }
            Op::ChannelNew { capacity, .. } => {
                let capacity = self.int(capacity);
                if capacity < 0 {
                    return Progress::Failed(self.task_error(
                        "ValueError",
                        format!("a channel cannot have a capacity of {capacity}"),
                        span,
                    ));
                }
                Progress::Done(Value::Channel(Rc::new(RefCell::new(ChannelState {
                    queue: VecDeque::new(),
                    capacity: capacity as usize,
                    closed: false,
                    sent: 0,
                    received: 0,
                }))))
            }
            Op::Send {
                channel,
                message,
                keep,
            } => {
                let source = self.source(channel);
                let channel = self.with_source(&source, channel_of);
                if let Some(Resume::Rendezvous(number)) = resume {
                    return if channel.borrow().received >= number {
                        Progress::Done(Value::Nothing)
                    } else {
                        Progress::Wait(Wait::Rendezvous(channel, number))
                    };
                }
                let (closed, room, capacity) = {
                    let state = channel.borrow();
                    (state.closed, state.has_room(), state.capacity)
                };
                if closed {
                    // The message is destroyed with the failure, unless the
                    // sender keeps it.
                    if !keep {
                        drop(self.take_place(message));
                    }
                    return Progress::Failed(self.task_error(
                        "ChannelClosedError",
                        "cannot send on a closed channel".to_string(),
                        span,
                    ));
                }
                if !room {
                    return Progress::Wait(Wait::Send(channel));
                }
                let value = self.take_place(message);
                let number = {
                    let mut state = channel.borrow_mut();
                    state.queue.push_back(value);
                    state.sent += 1;
                    state.sent
                };
                if capacity == 0 {
                    Progress::Wait(Wait::Rendezvous(channel, number))
                } else {
                    Progress::Done(Value::Nothing)
                }
            }
            Op::Receive { channel } => {
                let source = self.source(channel);
                let channel = self.with_source(&source, channel_of);
                let mut state = channel.borrow_mut();
                if let Some(value) = state.queue.pop_front() {
                    state.received += 1;
                    return Progress::Done(value);
                }
                if state.closed {
                    drop(state);
                    return Progress::Failed(self.task_error(
                        "ChannelClosedError",
                        "the channel is closed and has no more messages".to_string(),
                        span,
                    ));
                }
                drop(state);
                Progress::Wait(Wait::Receive(channel))
            }
            Op::Close { channel } => {
                let source = self.source(channel);
                let channel = self.with_source(&source, channel_of);
                channel.borrow_mut().closed = true;
                Progress::Done(Value::Nothing)
            }
            Op::Select { channels } => {
                let source = self.source(channels);
                let channels: Vec<Rc<RefCell<ChannelState>>> =
                    self.with_source(&source, |value| match value {
                        Value::Array(items) => items.iter().map(channel_of).collect(),
                        other => panic!("select over {other:?}"),
                    });
                if channels.is_empty() {
                    return Progress::Failed(self.task_error(
                        "ValueError",
                        "select needs at least one channel".to_string(),
                        span,
                    ));
                }
                let count = channels.len();
                let start = self.fibers[self.current].cursor;
                for offset in 0..count {
                    let position = (start + offset) % count;
                    if channels[position].borrow().receivable() {
                        self.fibers[self.current].cursor = position + 1;
                        return Progress::Done(Value::Int(position as i128));
                    }
                }
                Progress::Wait(Wait::Select(channels))
            }
            other => unreachable!("{other:?} is not a task operation"),
        }
    }

    /// Whether a fiber can go on: it has not started, what it waits for has
    /// come, or it is to stop.
    fn can_go_on(&self, index: usize, now: Instant) -> bool {
        let fiber = &self.fibers[index];
        if fiber.done {
            return false;
        }
        let stopping = self.fiber_cancelled(index);
        // A task cancelled before it started never runs.
        if stopping && !fiber.started {
            return false;
        }
        match &fiber.wait {
            None => true,
            Some(wait) => stopping || wait.ready(now),
        }
    }

    /// The next fiber, after the running one and round, that can go on,
    /// waiting for the earliest sleeper when none can yet.
    fn next_fiber(&mut self) -> Option<usize> {
        self.retire_unstarted();
        let count = self.fibers.len();
        loop {
            let now = Instant::now();
            for offset in 1..=count {
                let index = (self.current + offset) % count;
                if self.can_go_on(index, now) {
                    return Some(index);
                }
            }
            let mut wake: Option<Instant> = None;
            let mut sockets = Vec::new();
            for fiber in self.fibers.iter().filter(|fiber| !fiber.done) {
                let until = match &fiber.wait {
                    Some(Wait::Sleep(until)) => Some(*until),
                    Some(Wait::Io { wait, deadline, .. }) => {
                        sockets.push(wait.clone());
                        *deadline
                    }
                    _ => None,
                };
                if let Some(until) = until {
                    wake = Some(wake.map_or(until, |w| w.min(until)));
                }
            }
            let _ = self.output().flush();
            let timeout = wake.map(|w| w.saturating_duration_since(now));
            if !sockets.is_empty() {
                network::wait_any(&sockets, timeout);
            } else if network::has_waker() {
                // Another thread can end the wait early, to cancel a task.
                network::wait_any(&[], Some(timeout?));
            } else {
                std::thread::sleep(timeout?);
            }
        }
    }

    /// Finishes the tasks cancelled before they started: they never run, and
    /// what their functions captured is destroyed.
    fn retire_unstarted(&mut self) {
        let stopping = self.stopping || self.program.exiting();
        for fiber in &mut self.fibers {
            let cancelled = stopping || fiber.task.as_ref().is_some_and(|t| t.borrow().cancel);
            if !fiber.started && !fiber.done && cancelled {
                fiber.done = true;
                fiber.frames.clear();
            }
        }
    }

    /// Makes fiber `index` the running one, telling it how it was resumed.
    fn switch_to(&mut self, index: usize) {
        if index != self.current {
            let frames = std::mem::take(&mut self.frames);
            self.fibers[self.current].frames = frames;
            self.frames = std::mem::take(&mut self.fibers[index].frames);
            self.current = index;
        }
        let fiber = &mut self.fibers[index];
        fiber.started = true;
        fiber.resume = match fiber.wait.take() {
            Some(Wait::Rendezvous(_, number)) => Some(Resume::Rendezvous(number)),
            Some(Wait::Sleep(_)) => Some(Resume::Slept),
            Some(Wait::Yield) => Some(Resume::Yielded),
            Some(Wait::Io {
                wait,
                deadline: Some(deadline),
                timeout,
            }) if Instant::now() >= deadline && !network::ready(&wait) => {
                Some(Resume::TimedOut(timeout.unwrap_or_default()))
            }
            _ => None,
        };
    }

    /// Runs a primitive that can wait for a socket: when it must, the task
    /// waits while others run, and runs it again once the socket may be
    /// ready, until it finishes or the socket's timeout passes.
    fn host_waiting(
        &mut self,
        function: Host,
        args: &[Operand],
        tags: Option<(u32, u32)>,
        resume: Option<Resume>,
        span: Span,
    ) -> Progress {
        if let Some(Resume::TimedOut(timeout)) = resume {
            self.fibers[self.current].io_started = None;
            return Progress::Failed(self.task_error(
                "TimeoutError",
                network::timeout_message(timeout),
                span,
            ));
        }
        match self.host(function, args, tags, span) {
            Err(problem) if problem.code == BLOCKED_CODE => {
                let blocked = network::take_blocked().expect("what the primitive waits for");
                if self.cancelled() {
                    self.fibers[self.current].io_started = None;
                    return Progress::Failed(self.task_error(
                        CANCELLED_CODE,
                        "the task was cancelled".to_string(),
                        span,
                    ));
                }
                let started = *self.fibers[self.current]
                    .io_started
                    .get_or_insert_with(Instant::now);
                Progress::Wait(Wait::Io {
                    wait: blocked.wait,
                    deadline: blocked.timeout.map(|timeout| started + timeout),
                    timeout: blocked.timeout,
                })
            }
            result => {
                self.fibers[self.current].io_started = None;
                match result {
                    Ok(value) => Progress::Done(value),
                    Err(problem) => Progress::Failed(problem),
                }
            }
        }
    }

    /// Leaves the running task waiting and switches to one that can go on.
    /// False, with nothing changed, when no task ever could: a deadlock.
    pub(super) fn suspend(&mut self, wait: Wait) -> bool {
        self.fibers[self.current].wait = Some(wait);
        match self.next_fiber() {
            Some(index) => {
                self.switch_to(index);
                true
            }
            None => {
                self.fibers[self.current].wait = None;
                false
            }
        }
    }

    /// Records the result of the running task, which is not the main one,
    /// and switches to another. False when every task has finished.
    pub(super) fn finish_task(&mut self, result: Result<Value, ErrorValue>) -> bool {
        if let Err(problem) = &result
            && problem.code == EXIT_CODE
        {
            // `os.exit` in a task ends the whole program.
            self.exit_status
                .get_or_insert(exit_status(&problem.message));
            self.stopping = true;
        }
        let fiber = &mut self.fibers[self.current];
        fiber.done = true;
        if let Some(task) = &fiber.task {
            task.borrow_mut().result = Some(result);
        }
        if let Some(index) = self.next_fiber() {
            self.switch_to(index);
            return true;
        }
        // The tasks left all wait for each other: one of them fails.
        let Some(stuck) =
            (0..self.fibers.len()).find(|&i| !self.fibers[i].done && self.fibers[i].wait.is_some())
        else {
            return false;
        };
        self.switch_to(stuck);
        self.fibers[stuck].resume = Some(Resume::Deadlock);
        true
    }

    /// After the main task: cancels the tasks still running and runs each
    /// until it has stopped and cleaned up. A task that never started is
    /// destroyed with what it captured.
    pub(super) fn shutdown(&mut self) {
        self.stopping = true;
        self.fibers[0].done = true;
        while let Some(index) = (1..self.fibers.len()).find(|&i| !self.fibers[i].done) {
            if !self.fibers[index].started {
                let fiber = &mut self.fibers[index];
                fiber.done = true;
                fiber.frames.clear();
                continue;
            }
            self.switch_to(index);
            // Runs tasks until every one has finished.
            let _ = self.execute(0);
        }
        self.frames.clear();
    }

    /// The errors of tasks that failed without being joined, except for
    /// cancellation, which is how they were asked to stop.
    pub(super) fn task_failures(&self) -> Vec<ErrorValue> {
        self.fibers
            .iter()
            .filter_map(|fiber| fiber.task.as_ref())
            .filter_map(|task| match &task.borrow().result {
                Some(Err(problem))
                    if problem.code != CANCELLED_CODE && problem.code != EXIT_CODE =>
                {
                    Some(problem.clone())
                }
                _ => None,
            })
            .collect()
    }
}
