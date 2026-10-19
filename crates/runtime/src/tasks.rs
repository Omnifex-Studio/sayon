//! Tasks and channels of compiled programs.
//!
//! Each task is an operating-system fiber with a stack of its own, and the
//! tasks of a thread take turns on it. A task waits inside one of
//! the functions here, such as `sayon_channel_receive`: it records what it
//! waits for and switches to the next task that can go on, round the tasks
//! in order, and when it is switched back to it looks again. The rules are
//! the interpreter's (see its `machine/tasks.rs`), so both execution modes
//! run a program's tasks in the same order.
//!
//! Each thread that runs Sayon code has a scheduler of its own: the main
//! thread's, and those of tasks started on threads of their own (see
//! `parallel`). No reference into a scheduler is held across a switch
//! between fibers. Channels can be shared by tasks of several threads, so
//! their state is behind a lock.

// The functions called by generated code rely on the same conventions as
// those of `ffi`, where their contracts are described.
#![allow(clippy::missing_safety_doc)]

use crate::ffi::{DropFn, SayonArray, SayonError, allocated, released, set_error};
use crate::host::{CANCELLED_CODE, EXIT_CODE, exit_status};
use crate::network::{self, IoWait};
use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

// ----- fibers of the operating system ---------------------------------------------------------

#[cfg(windows)]
mod os {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ConvertThreadToFiber(parameter: *mut c_void) -> *mut c_void;
        fn CreateFiber(
            stack: usize,
            start: unsafe extern "system" fn(*mut c_void),
            parameter: *mut c_void,
        ) -> *mut c_void;
        fn SwitchToFiber(fiber: *mut c_void);
        fn DeleteFiber(fiber: *mut c_void);
    }

    /// Makes the program's thread a fiber, so that it can switch to others.
    pub fn adopt_thread() -> *mut c_void {
        // SAFETY: called once, on the program's thread.
        unsafe { ConvertThreadToFiber(std::ptr::null_mut()) }
    }

    pub fn create(start: unsafe extern "system" fn(*mut c_void), parameter: usize) -> *mut c_void {
        // SAFETY: the start function never returns; it switches away.
        unsafe { CreateFiber(0, start, parameter as *mut c_void) }
    }

    pub fn switch(fiber: *mut c_void) {
        // SAFETY: a live fiber of this thread.
        unsafe { SwitchToFiber(fiber) }
    }

    pub fn delete(fiber: *mut c_void) {
        // SAFETY: a fiber that is not running.
        unsafe { DeleteFiber(fiber) }
    }
}

#[cfg(not(windows))]
mod os {
    use std::ffi::c_void;

    fn unavailable() -> ! {
        eprintln!("fatal error: compiled programs can run tasks only on Windows so far");
        std::process::abort()
    }

    pub fn adopt_thread() -> *mut c_void {
        unavailable()
    }

    pub fn create(_: unsafe extern "C" fn(*mut c_void), _: usize) -> *mut c_void {
        unavailable()
    }

    pub fn switch(_: *mut c_void) {
        unavailable()
    }

    pub fn delete(_: *mut c_void) {}
}

// ----- the scheduler ----------------------------------------------------------------------------

/// What a task's handle and the scheduler know of the task. The handle and
/// the scheduler each hold a reference.
#[repr(C)]
pub struct TaskBlock {
    /// 0, which tells it from a task on a thread of its own.
    kind: u32,
    references: usize,
    /// The function value the task runs: its code and environment block.
    code: *const u8,
    environment: *mut u8,
    result: *mut u8,
    result_size: i64,
    result_drop: DropFn,
    outcome: Outcome,
    cancel: bool,
    joined: bool,
    detached: bool,
}

enum Outcome {
    Running,
    Succeeded,
    Failed(*mut SayonError),
    /// Joined, or cancelled before it started.
    Taken,
}

/// A channel: its messages, each in a buffer of whole words so that it can
/// be dropped in place, and the count of handles to it.
pub struct ChannelBlock {
    references: AtomicUsize,
    capacity: usize,
    element_size: usize,
    drop: DropFn,
    /// Set once a task on another thread may hold the channel: from then on
    /// its changes wake the schedulers of the other threads, and waiting
    /// for it is no deadlock while tasks run on them.
    crossed: AtomicBool,
    state: Mutex<ChannelState>,
}

struct ChannelState {
    queue: VecDeque<Vec<u64>>,
    closed: bool,
    sent: u64,
    received: u64,
}

/// What taking a message from a channel found.
enum Taken {
    Message,
    Empty,
    /// The channel is closed and has no messages left.
    Closed,
}

impl ChannelBlock {
    fn state(&self) -> MutexGuard<'_, ChannelState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn receivable(&self) -> bool {
        let state = self.state();
        !state.queue.is_empty() || state.closed
    }

    fn has_room(&self) -> bool {
        let state = self.state();
        state.closed || state.queue.len() < self.capacity.max(1)
    }

    fn crossed(&self) -> bool {
        self.crossed.load(Ordering::Acquire)
    }

    /// Wakes the schedulers of other threads, when a task there may wait
    /// for this channel.
    fn changed(&self) {
        if self.crossed() {
            network::wake();
        }
    }

    /// Moves the next message into `out`, if there is one.
    unsafe fn take(&self, out: *mut u8) -> Taken {
        let mut state = self.state();
        match state.queue.pop_front() {
            Some(buffer) => {
                state.received += 1;
                drop(state);
                // SAFETY: `out` has room for a message.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        buffer.as_ptr() as *const u8,
                        out,
                        self.element_size,
                    );
                }
                self.changed();
                Taken::Message
            }
            None if state.closed => Taken::Closed,
            None => Taken::Empty,
        }
    }
}

thread_local! {
    /// The channels made on this thread that no task on another thread can
    /// hold yet.
    static LOCAL_CHANNELS: RefCell<Vec<*mut ChannelBlock>> = const { RefCell::new(Vec::new()) };
}

/// Counts every channel of this thread as one that tasks on other threads
/// may hold. A channel handle can leave its thread only with a value that
/// does: what a task on another thread captures or returns, the value of a
/// mutex, or a message on a channel that crossed already. The runtime does
/// not see inside those values, so each of them crosses all of the
/// thread's channels.
pub(crate) fn cross_channels() {
    network::prepare_waking();
    let _ = LOCAL_CHANNELS.try_with(|local| {
        for channel in local.borrow_mut().drain(..) {
            // SAFETY: a channel is in the list while it lives.
            unsafe { (*channel).crossed.store(true, Ordering::Release) };
        }
    });
}

/// Whether a wait could end through a task on another thread: one waits
/// for a channel that crossed, and tasks run on other threads.
fn waits_elsewhere(wait: &Wait) -> bool {
    // SAFETY: what a fiber waits for is kept alive while it waits.
    unsafe {
        match wait {
            Wait::Receive(channel) | Wait::Send(channel) | Wait::Rendezvous(channel, _) => {
                (**channel).crossed()
            }
            Wait::Select(channels) => channels.iter().any(|channel| (**channel).crossed()),
            _ => false,
        }
    }
}

#[derive(Clone)]
enum Wait {
    None,
    Receive(*mut ChannelBlock),
    Send(*mut ChannelBlock),
    Rendezvous(*mut ChannelBlock, u64),
    Join(*mut TaskBlock),
    Select(Vec<*mut ChannelBlock>),
    Sleep(Instant),
    Yield,
    /// A socket, or a connection being made, until a deadline.
    Io(IoWait, Option<Instant>),
}

impl Wait {
    fn ready(&self, now: Instant) -> bool {
        // SAFETY: what a fiber waits for is kept alive while it waits.
        unsafe {
            match self {
                Wait::None => false,
                Wait::Receive(channel) => (**channel).receivable(),
                Wait::Send(channel) => (**channel).has_room(),
                Wait::Rendezvous(channel, number) => (**channel).state().received >= *number,
                Wait::Join(task) => !matches!((**task).outcome, Outcome::Running),
                Wait::Select(channels) => channels.iter().any(|c| (**c).receivable()),
                Wait::Sleep(until) => now >= *until,
                Wait::Yield => true,
                Wait::Io(wait, deadline) => {
                    deadline.is_some_and(|deadline| now >= deadline) || network::ready(wait)
                }
            }
        }
    }
}

struct Fiber {
    os: *mut c_void,
    /// Null for the main task.
    task: *mut TaskBlock,
    wait: Wait,
    /// Set to resume a waiting fiber when no task can ever go on.
    deadlock: bool,
    started: bool,
    done: bool,
    cursor: usize,
    /// The mutexes whose locks the task holds; it cannot wait meanwhile.
    pub(crate) locks: usize,
    /// The calls of C functions that can call back, which the task is in;
    /// it cannot wait meanwhile either.
    callbacks: usize,
}

struct Scheduler {
    fibers: Vec<Fiber>,
    current: usize,
    stopping: bool,
    exit_status: Option<i32>,
}

thread_local! {
    static SCHEDULER: UnsafeCell<Option<Scheduler>> = const { UnsafeCell::new(None) };
}

/// This thread's scheduler, if it has made one.
fn scheduler_slot() -> *mut Option<Scheduler> {
    SCHEDULER.with(|cell| cell.get())
}

/// This thread's scheduler, made with its first task when first needed.
fn scheduler() -> &'static mut Scheduler {
    // SAFETY: the scheduler lives as long as its thread, which is the only
    // one using it, and no caller keeps the reference across a switch.
    unsafe {
        (*scheduler_slot()).get_or_insert_with(|| Scheduler {
            fibers: vec![Fiber {
                os: std::ptr::null_mut(),
                task: std::ptr::null_mut(),
                wait: Wait::None,
                deadlock: false,
                started: true,
                done: false,
                cursor: 0,
                locks: 0,
                callbacks: 0,
            }],
            current: 0,
            stopping: false,
            exit_status: None,
        })
    }
}

/// The running task's place in its thread's scheduler.
pub(crate) fn current_fiber() -> usize {
    scheduler().current
}

/// Counts a mutex's lock the running task takes (1) or frees (-1).
pub(crate) fn count_lock(change: isize) {
    let s = scheduler();
    let fiber = &mut s.fibers[s.current];
    fiber.locks = fiber.locks.saturating_add_signed(change);
}

/// Counts a call of a C function that can call Sayon functions back,
/// which the running task starts (1) or ends (-1).
pub(crate) fn count_callbacks(change: isize) {
    let s = scheduler();
    let fiber = &mut s.fibers[s.current];
    fiber.callbacks = fiber.callbacks.saturating_add_signed(change);
}

/// Why a task cannot wait at all.
#[derive(Clone, Copy)]
pub(crate) enum Refusal {
    /// It holds a mutex's lock.
    Locked,
    /// It runs a function that C called, whose caller cannot be left.
    Callback,
}

impl Refusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Refusal::Locked => "LockError",
            Refusal::Callback => "CallbackError",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Refusal::Locked => "a task cannot wait while it holds a mutex's lock",
            Refusal::Callback => "a function that C calls cannot wait",
        }
    }
}

fn refusal(index: usize) -> Option<Refusal> {
    let fiber = &scheduler().fibers[index];
    if fiber.locks > 0 {
        Some(Refusal::Locked)
    } else if fiber.callbacks > 0 {
        Some(Refusal::Callback)
    } else {
        None
    }
}

fn is_cancelled(index: usize) -> bool {
    let s = scheduler();
    let fiber = &s.fibers[index];
    // SAFETY: the scheduler holds a reference to each task.
    s.stopping
        || crate::parallel::exiting()
        || (index == 0 && crate::parallel::thread_cancelled())
        || (!fiber.task.is_null() && unsafe { (*fiber.task).cancel })
}

fn can_go_on(index: usize, now: Instant) -> bool {
    let fiber = &scheduler().fibers[index];
    if fiber.done {
        return false;
    }
    let cancelled = is_cancelled(index);
    if !fiber.started {
        return !cancelled;
    }
    cancelled || fiber.deadlock || fiber.wait.ready(now)
}

/// Destroys what a task's function captured.
unsafe fn destroy_environment(task: *mut TaskBlock) {
    // SAFETY: the task's environment, which this destroys only once.
    unsafe {
        destroy_function_environment((*task).environment);
        (*task).environment = std::ptr::null_mut();
    }
}

/// Finishes the tasks cancelled before they started: they never run.
fn retire_unstarted() {
    for index in 1..scheduler().fibers.len() {
        let fiber = &scheduler().fibers[index];
        if !fiber.started && !fiber.done && is_cancelled(index) {
            let (task, os) = (fiber.task, fiber.os);
            let fiber = &mut scheduler().fibers[index];
            fiber.done = true;
            // SAFETY: the task never ran, so its environment is intact.
            unsafe {
                destroy_environment(task);
                (*task).outcome = Outcome::Taken;
            }
            os::delete(os);
        }
    }
}

/// The next fiber after the running one, round, that can go on; when none
/// can yet but one sleeps or waits for a socket, after waiting for the
/// earliest to be able to.
fn next_fiber() -> Option<usize> {
    retire_unstarted();
    loop {
        let now = Instant::now();
        let count = scheduler().fibers.len();
        let current = scheduler().current;
        for offset in 1..=count {
            let index = (current + offset) % count;
            if can_go_on(index, now) {
                return Some(index);
            }
        }
        let waiting = scheduler().fibers.iter().filter(|fiber| !fiber.done);
        let mut wake: Option<Instant> = None;
        let mut sockets = Vec::new();
        let mut elsewhere = false;
        for fiber in waiting {
            elsewhere |= waits_elsewhere(&fiber.wait);
            let until = match &fiber.wait {
                Wait::Sleep(until) => Some(*until),
                Wait::Io(wait, deadline) => {
                    sockets.push(wait.clone());
                    *deadline
                }
                _ => None,
            };
            if let Some(until) = until {
                wake = Some(wake.map_or(until, |w| w.min(until)));
            }
        }
        let _ = std::io::stdout().flush();
        let timeout = wake.map(|w| w.saturating_duration_since(now));
        if !sockets.is_empty() {
            network::wait_any(&sockets, timeout);
        } else if elsewhere && crate::parallel::others_run() {
            // A task on another thread can let one go on, and wakes this
            // thread when it may have.
            network::wait_any(&[], timeout);
        } else if network::has_waker() {
            // Another thread can end the wait early, to cancel a task.
            network::wait_any(&[], Some(timeout?));
        } else {
            std::thread::sleep(timeout?);
        }
    }
}

fn switch_to(index: usize) {
    let s = scheduler();
    let fiber = &mut s.fibers[index];
    fiber.wait = Wait::None;
    fiber.started = true;
    if index == s.current {
        return;
    }
    let target = fiber.os;
    s.current = index;
    os::switch(target);
}

/// Why a wait ended without what was waited for.
enum Interrupted {
    Cancelled,
    Deadlock,
    /// The task cannot wait at all.
    Refused(Refusal),
}

/// Lets the running task wait, switching to another task until this one
/// can go on.
fn wait(what: Wait) -> Result<(), Interrupted> {
    let current = scheduler().current;
    if let Some(refusal) = refusal(current) {
        return Err(Interrupted::Refused(refusal));
    }
    if is_cancelled(current) {
        return Err(Interrupted::Cancelled);
    }
    scheduler().fibers[current].wait = what;
    match next_fiber() {
        Some(index) => switch_to(index),
        None => {
            scheduler().fibers[current].wait = Wait::None;
            return Err(Interrupted::Deadlock);
        }
    }
    let fiber = &mut scheduler().fibers[current];
    if std::mem::take(&mut fiber.deadlock) {
        return Err(Interrupted::Deadlock);
    }
    if is_cancelled(current) {
        return Err(Interrupted::Cancelled);
    }
    Ok(())
}

/// Why a wait for a socket ended without it being ready.
pub(crate) enum IoStop {
    Cancelled,
    TimedOut,
    /// The task cannot wait at all.
    Refused(Refusal),
}

/// Lets the running task wait for a socket, or for a connection being
/// made, while the other tasks run.
pub(crate) fn wait_io(what: IoWait, deadline: Option<Instant>) -> Result<(), IoStop> {
    let current = scheduler().current;
    if let Some(refusal) = refusal(current) {
        return Err(IoStop::Refused(refusal));
    }
    if is_cancelled(current) {
        return Err(IoStop::Cancelled);
    }
    scheduler().fibers[current].wait = Wait::Io(what.clone(), deadline);
    if let Some(index) = next_fiber() {
        switch_to(index);
    }
    scheduler().fibers[current].wait = Wait::None;
    if is_cancelled(current) {
        return Err(IoStop::Cancelled);
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) && !network::ready(&what) {
        return Err(IoStop::TimedOut);
    }
    Ok(())
}

unsafe fn interrupted(err: *mut *mut SayonError, why: Interrupted, location: *const c_char) -> i32 {
    // SAFETY: the caller's error slot.
    unsafe {
        match why {
            Interrupted::Cancelled => set_error(
                err,
                CANCELLED_CODE,
                "the task was cancelled".to_string(),
                location,
            ),
            Interrupted::Deadlock => set_error(
                err,
                "DeadlockError",
                "every task is waiting, so none can go on".to_string(),
                location,
            ),
            Interrupted::Refused(refusal) => {
                set_error(err, refusal.code(), refusal.message().to_string(), location)
            }
        }
    }
}

/// Runs a function value on the running fiber: its code with its
/// environment. Gives the status, after storing the result or the error.
pub(crate) unsafe fn call_function(
    code: *const u8,
    environment: *mut u8,
    result: *mut u8,
    error: *mut *mut SayonError,
) -> i32 {
    type Code = unsafe extern "C" fn(*mut u8, *mut *mut SayonError, i64, *mut u8) -> i32;
    // SAFETY: the code of a function value of no parameters.
    unsafe {
        let code: Code = std::mem::transmute(code);
        // A named function's value has no environment; its code never reads it.
        let structure = environment.wrapping_add(16);
        code(result, error, 1, structure)
    }
}

/// Destroys a function value's environment, if it has one.
pub(crate) unsafe fn destroy_function_environment(environment: *mut u8) {
    // SAFETY: the environment block starts with its destroy function.
    unsafe {
        if !environment.is_null() {
            let destroy: unsafe extern "C" fn(*mut u8) = *(environment as *const _);
            destroy(environment);
        }
    }
}

/// The first function of every task's fiber. It never returns: when the
/// task finishes, it switches to another fiber for good.
unsafe extern "system" fn task_start(parameter: *mut c_void) {
    let index = parameter as usize;
    let task = scheduler().fibers[index].task;
    // SAFETY: the scheduler holds a reference to the task.
    unsafe {
        let mut error: *mut SayonError = std::ptr::null_mut();
        let status = call_function(
            (*task).code,
            (*task).environment,
            (*task).result,
            &mut error,
        );
        (*task).outcome = if status == 0 {
            Outcome::Succeeded
        } else {
            Outcome::Failed(error)
        };
        destroy_environment(task);
        if status != 0 && (*error).code == EXIT_CODE {
            // `os.exit` in a task ends the whole program.
            let requested = exit_status(&(*error).message);
            scheduler().exit_status.get_or_insert(requested);
            scheduler().stopping = true;
        }
    }
    scheduler().fibers[index].done = true;
    let next = next_fiber().unwrap_or_else(|| {
        // The tasks left wait for each other: one of them fails; with none
        // left, the main task carries on.
        let s = scheduler();
        match (0..s.fibers.len())
            .find(|&i| !s.fibers[i].done && !matches!(s.fibers[i].wait, Wait::None))
        {
            Some(stuck) => {
                s.fibers[stuck].deadlock = true;
                stuck
            }
            None => 0,
        }
    });
    let target = scheduler().fibers[next].os;
    let s = scheduler();
    s.fibers[next].started = true;
    s.current = next;
    os::switch(target);
    unreachable!("a finished task was resumed");
}

// ----- tasks ------------------------------------------------------------------------------------

/// Starts a task running the function value at `body`, which it takes
/// over, and stores its handle in `out`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_task_spawn(
    body: *mut u8,
    result_size: i64,
    result_drop: DropFn,
    out: *mut *mut TaskBlock,
) {
    // SAFETY: `body` is a function value: its code and environment block.
    unsafe {
        if scheduler().fibers[0].os.is_null() {
            scheduler().fibers[0].os = os::adopt_thread();
        }
        let code = *(body as *const *const u8);
        let environment = *(body.add(8) as *const *mut u8);
        std::ptr::write_bytes(body, 0, 16);
        let result = crate::ffi::sayon_alloc(result_size.max(1));
        allocated();
        let task = Box::into_raw(Box::new(TaskBlock {
            kind: 0,
            references: 2,
            code,
            environment,
            result,
            result_size,
            result_drop,
            outcome: Outcome::Running,
            cancel: false,
            joined: false,
            detached: false,
        }));
        let index = scheduler().fibers.len();
        let os = os::create(task_start, index);
        scheduler().fibers.push(Fiber {
            os,
            task,
            wait: Wait::None,
            deadlock: false,
            started: false,
            done: false,
            cursor: 0,
            locks: 0,
            callbacks: 0,
        });
        *out = task;
    }
}

/// Removes a reference to a task; the last frees it, destroying a result
/// nobody joined.
unsafe fn release_task(task: *mut TaskBlock) {
    // SAFETY: a live task block.
    unsafe {
        (*task).references -= 1;
        if (*task).references > 0 {
            return;
        }
        match std::mem::replace(&mut (*task).outcome, Outcome::Taken) {
            Outcome::Succeeded => {
                if let Some(drop) = (*task).result_drop {
                    drop((*task).result);
                }
            }
            Outcome::Failed(mut error) => crate::ffi::sayon_error_drop(&mut error),
            Outcome::Running | Outcome::Taken => {}
        }
        crate::ffi::sayon_free((*task).result, (*task).result_size.max(1));
        drop(Box::from_raw(task));
        released();
    }
}

/// Drops a task's handle: without a join or a detach, the task is cancelled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_task_drop(slot: *mut *mut TaskBlock) {
    // SAFETY: the slot holds null or a live task.
    unsafe {
        let task = *slot;
        if task.is_null() {
            return;
        }
        if (*task).kind != 0 {
            crate::parallel::drop_task(slot as *mut *mut crate::parallel::ParallelBlock);
            return;
        }
        *slot = std::ptr::null_mut();
        if (*task).references == 2 && !(*task).joined && !(*task).detached {
            (*task).cancel = true;
        }
        release_task(task);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_task_detach(slot: *const *mut TaskBlock) {
    // SAFETY: the slot holds a live task.
    unsafe {
        if (**slot).kind != 0 {
            crate::parallel::detach_task(*slot as *const crate::parallel::ParallelBlock);
            return;
        }
        (**slot).detached = true
    }
}

/// Waits for a task and moves its result into `out`, or its error into the
/// error slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_task_join(
    err: *mut *mut SayonError,
    slot: *const *mut TaskBlock,
    out: *mut u8,
    location: *const c_char,
) -> i32 {
    // SAFETY: the slot holds a live task.
    unsafe {
        let task = *slot;
        if (*task).kind != 0 {
            let block = task as *const crate::parallel::ParallelBlock;
            return crate::parallel::join_task(err, block, out, location);
        }
        loop {
            match std::mem::replace(&mut (*task).outcome, Outcome::Taken) {
                Outcome::Succeeded => {
                    (*task).joined = true;
                    std::ptr::copy_nonoverlapping(
                        (*task).result,
                        out,
                        (*task).result_size as usize,
                    );
                    return 0;
                }
                Outcome::Failed(error) => {
                    (*task).joined = true;
                    if !(*err).is_null() {
                        crate::ffi::sayon_error_drop(err);
                    }
                    *err = error;
                    return 1;
                }
                outcome => (*task).outcome = outcome,
            }
            if let Err(why) = wait(Wait::Join(task)) {
                return interrupted(err, why, location);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_task_yield(
    err: *mut *mut SayonError,
    location: *const c_char,
) -> i32 {
    match wait(Wait::Yield) {
        Ok(()) => 0,
        // SAFETY: the caller's error slot.
        Err(why) => unsafe { interrupted(err, why, location) },
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_task_sleep(
    err: *mut *mut SayonError,
    nanoseconds: i64,
    location: *const c_char,
) -> i32 {
    if nanoseconds <= 0 {
        return 0;
    }
    let until = Instant::now() + Duration::from_nanos(nanoseconds as u64);
    while Instant::now() < until {
        if let Err(why) = wait(Wait::Sleep(until)) {
            // SAFETY: the caller's error slot.
            return unsafe { interrupted(err, why, location) };
        }
    }
    0
}

/// When the main task has finished: cancels the tasks still running, runs
/// each until it has stopped and cleaned up, and reports the ones that
/// failed without being joined. Gives the exit status the program must end
/// with, or -1 when the main task's own stands.
#[unsafe(no_mangle)]
pub extern "C" fn sayon_tasks_finish(main_failed: bool) -> i32 {
    let local = finish_local_tasks(main_failed);
    // Then the tasks on threads of their own, which may end the program.
    let threads = crate::parallel::finish_threads(main_failed);
    match (local, threads) {
        (_, Some(status)) => status,
        (status, None) if status >= 0 => status,
        (status, None) if crate::parallel::failed() && !main_failed => status.max(1),
        (status, None) => status,
    }
}

/// Finishes the tasks of this thread's scheduler: the status a task's
/// `os.exit` asked for, 1 when a task failed unjoined, or -1.
pub(crate) fn finish_local_tasks(main_failed: bool) -> i32 {
    // SAFETY: no task runs while this is the running fiber.
    unsafe {
        if (*scheduler_slot()).is_none() {
            return -1;
        }
    }
    scheduler().stopping = true;
    loop {
        retire_unstarted();
        let s = scheduler();
        let Some(index) = (1..s.fibers.len()).find(|&i| !s.fibers[i].done) else {
            break;
        };
        switch_to(index);
        scheduler().current = 0;
    }
    let _ = std::io::stdout().flush();
    let mut failed = false;
    let fibers = std::mem::take(&mut scheduler().fibers);
    for fiber in fibers.into_iter().skip(1) {
        if !fiber.os.is_null() && fiber.started {
            os::delete(fiber.os);
        }
        let task = fiber.task;
        // SAFETY: the scheduler's reference to the task.
        unsafe {
            if let Outcome::Failed(error) = (*task).outcome {
                let error = &*error;
                // When the main task failed, its error is reported instead.
                if error.code != CANCELLED_CODE
                    && error.code != EXIT_CODE
                    && !(*task).joined
                    && !main_failed
                {
                    failed = true;
                    eprintln!("error: a task failed: {}: {}", error.code, error.message);
                    if let Some(location) = &error.location {
                        eprintln!("  --> {location}");
                    }
                }
            }
            release_task(task);
        }
    }
    match scheduler().exit_status {
        Some(status) => status,
        None if failed => 1,
        None => -1,
    }
}

/// Whether a task asked the program to exit, so that the main task's
/// cancellation is not reported.
pub fn exit_requested() -> Option<i32> {
    // SAFETY: read on the thread the scheduler belongs to.
    unsafe {
        (*scheduler_slot())
            .as_ref()
            .and_then(|s| s.exit_status)
            .or(crate::parallel::exit_status())
    }
}

// ----- channels ---------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_channel_new(
    err: *mut *mut SayonError,
    capacity: i64,
    element_size: i64,
    drop: DropFn,
    out: *mut *mut ChannelBlock,
    location: *const c_char,
) -> i32 {
    // SAFETY: the caller's error slot and destination.
    unsafe {
        if capacity < 0 {
            return set_error(
                err,
                "ValueError",
                format!("a channel cannot have a capacity of {capacity}"),
                location,
            );
        }
        allocated();
        let channel = Box::into_raw(Box::new(ChannelBlock {
            references: AtomicUsize::new(1),
            capacity: capacity as usize,
            element_size: element_size as usize,
            drop,
            crossed: AtomicBool::new(false),
            state: Mutex::new(ChannelState {
                queue: VecDeque::new(),
                closed: false,
                sent: 0,
                received: 0,
            }),
        }));
        LOCAL_CHANNELS.with(|local| local.borrow_mut().push(channel));
        *out = channel;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_channel_clone(
    src: *const *mut ChannelBlock,
    dst: *mut *mut ChannelBlock,
) {
    // SAFETY: the source holds a live channel.
    unsafe {
        (**src).references.fetch_add(1, Ordering::Relaxed);
        *dst = *src;
    }
}

/// Drops a channel handle; the last destroys the messages left in it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_channel_release(slot: *mut *mut ChannelBlock) {
    // SAFETY: the slot holds null or a live channel.
    unsafe {
        let channel = *slot;
        if channel.is_null() {
            return;
        }
        *slot = std::ptr::null_mut();
        if (*channel).references.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        // A channel no other thread can hold is in this thread's list.
        if !(*channel).crossed() {
            let _ = LOCAL_CHANNELS.try_with(|local| local.borrow_mut().retain(|c| *c != channel));
        }
        let mut channel = Box::from_raw(channel);
        if let Some(drop) = channel.drop {
            let state = channel.state.get_mut().unwrap_or_else(|e| e.into_inner());
            for mut message in state.queue.drain(..) {
                drop(message.as_mut_ptr() as *mut u8);
            }
        }
        released();
    }
}

/// Moves the message at `message` into the channel once it has room,
/// leaving the place empty; a rendezvous send then waits for it to be
/// received. On a closed channel the message is destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_channel_send(
    err: *mut *mut SayonError,
    slot: *const *mut ChannelBlock,
    message: *mut u8,
    keep: bool,
    location: *const c_char,
) -> i32 {
    // SAFETY: the slot holds a live channel; the message has its size.
    unsafe {
        let channel = &**slot;
        let size = channel.element_size;
        // What the message holds may go to another thread with it.
        if channel.crossed() {
            cross_channels();
        }
        let number = loop {
            let mut state = channel.state();
            if state.closed {
                drop(state);
                // The message is destroyed, unless the sender keeps it.
                if !keep {
                    if let Some(drop) = channel.drop {
                        drop(message);
                    }
                    std::ptr::write_bytes(message, 0, size);
                }
                return set_error(
                    err,
                    "ChannelClosedError",
                    "cannot send on a closed channel".to_string(),
                    location,
                );
            }
            if state.queue.len() < channel.capacity.max(1) {
                let mut buffer = vec![0u64; size.div_ceil(8).max(1)];
                std::ptr::copy_nonoverlapping(message, buffer.as_mut_ptr() as *mut u8, size);
                std::ptr::write_bytes(message, 0, size);
                state.queue.push_back(buffer);
                state.sent += 1;
                break state.sent;
            }
            drop(state);
            if let Err(why) = wait(Wait::Send(*slot)) {
                return interrupted(err, why, location);
            }
        };
        channel.changed();
        // Other tasks receive while this one waits.
        if channel.capacity == 0 {
            while channel.state().received < number {
                if let Err(why) = wait(Wait::Rendezvous(*slot, number)) {
                    return interrupted(err, why, location);
                }
            }
        }
        0
    }
}

/// Moves the next message into `out`, waiting while the channel is empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_channel_receive(
    err: *mut *mut SayonError,
    slot: *const *mut ChannelBlock,
    out: *mut u8,
    location: *const c_char,
) -> i32 {
    // SAFETY: the slot holds a live channel; `out` has room for a message.
    unsafe {
        let channel = &**slot;
        loop {
            match channel.take(out) {
                Taken::Message => return 0,
                Taken::Closed => {
                    return set_error(
                        err,
                        "ChannelClosedError",
                        "the channel is closed and has no more messages".to_string(),
                        location,
                    );
                }
                Taken::Empty => {}
            }
            if let Err(why) = wait(Wait::Receive(*slot)) {
                return interrupted(err, why, location);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_channel_close(slot: *const *mut ChannelBlock) {
    // SAFETY: the slot holds a live channel.
    unsafe {
        let channel = &**slot;
        channel.state().closed = true;
        channel.changed();
    }
}

/// Receives from a channel of an array of channel handles that has a
/// message or is closed, waiting until one is, and stores its position;
/// the search starts after the one this task chose last. The message goes
/// to `out`; a closed, empty channel fails.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_channel_select(
    err: *mut *mut SayonError,
    channels: *const SayonArray,
    position_out: *mut i64,
    out: *mut u8,
    location: *const c_char,
) -> i32 {
    // SAFETY: an array of channel handles.
    unsafe {
        let array = &*channels;
        let count = array.len as usize;
        if count == 0 {
            return set_error(
                err,
                "ValueError",
                "select needs at least one channel".to_string(),
                location,
            );
        }
        let handles: Vec<*mut ChannelBlock> = (0..count)
            .map(|i| *(array.ptr as *const *mut ChannelBlock).add(i))
            .collect();
        loop {
            let current = scheduler().current;
            let start = scheduler().fibers[current].cursor;
            for offset in 0..count {
                let position = (start + offset) % count;
                // Taking the message is the choice, so that a task on
                // another thread cannot take it in between.
                let taken = (*handles[position]).take(out);
                if matches!(taken, Taken::Empty) {
                    continue;
                }
                scheduler().fibers[current].cursor = position + 1;
                *position_out = position as i64;
                if matches!(taken, Taken::Closed) {
                    return set_error(
                        err,
                        "ChannelClosedError",
                        "the channel is closed and has no more messages".to_string(),
                        location,
                    );
                }
                return 0;
            }
            if let Err(why) = wait(Wait::Select(handles.clone())) {
                return interrupted(err, why, location);
            }
        }
    }
}
