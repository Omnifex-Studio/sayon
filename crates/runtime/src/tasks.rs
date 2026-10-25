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

/// Fibers on Linux for x86-64: each has a stack of its own, mapped with a
/// page below it that stops a stack that grows too far, and switching saves
/// the registers a function must keep (and the floating-point control
/// words) on the running fiber's stack, takes the other's stack, and
/// restores them from it.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod os {
    use std::cell::Cell;
    use std::ffi::c_void;

    /// What a fiber needs to run: its stack, and where its stack pointer
    /// was when it last switched away.
    struct Fiber {
        stack: *mut u8,
        size: usize,
        saved: usize,
    }

    /// The bytes of a fiber's stack, as Windows gives one by default.
    const STACK: usize = 1 << 20;
    const PAGE: usize = 4096;

    const PROT_NONE: i32 = 0;
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_ANONYMOUS: i32 = 0x20;
    const MAP_NORESERVE: i32 = 0x4000;
    const MAP_STACK: i32 = 0x20000;

    unsafe extern "C" {
        fn mmap(
            address: *mut c_void,
            length: usize,
            protection: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut c_void;
        fn mprotect(address: *mut c_void, length: usize, protection: i32) -> i32;
        fn munmap(address: *mut c_void, length: usize) -> i32;
        fn sayon_fiber_switch(save: *mut usize, to: usize);
        fn sayon_fiber_entry();
    }

    // Saves rbp, rbx, r12 to r15, and the SSE and x87 control words below
    // the return address, stores the stack pointer at `save` (rdi), and
    // restores the same from the stack at `to` (rsi). A new fiber's stack
    // is laid out so that the first switch to it returns into
    // `sayon_fiber_entry`, which calls its start function (in r12) with its
    // parameter (in r13).
    std::arch::global_asm!(
        ".text",
        ".p2align 4",
        ".globl sayon_fiber_switch",
        "sayon_fiber_switch:",
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "sub rsp, 8",
        "stmxcsr [rsp]",
        "fnstcw [rsp + 4]",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "ldmxcsr [rsp]",
        "fldcw [rsp + 4]",
        "add rsp, 8",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        ".p2align 4",
        ".globl sayon_fiber_entry",
        "sayon_fiber_entry:",
        "mov rdi, r13",
        "call r12",
        "ud2",
    );

    thread_local! {
        /// The fiber running on this thread.
        static CURRENT: Cell<*mut Fiber> = const { Cell::new(std::ptr::null_mut()) };
    }

    /// Makes the thread a fiber, so that it can switch to others.
    pub fn adopt_thread() -> *mut c_void {
        let fiber = Box::into_raw(Box::new(Fiber {
            stack: std::ptr::null_mut(),
            size: 0,
            saved: 0,
        }));
        CURRENT.with(|current| current.set(fiber));
        fiber.cast()
    }

    pub fn create(start: unsafe extern "system" fn(*mut c_void), parameter: usize) -> *mut c_void {
        let size = STACK + PAGE;
        // SAFETY: a fresh private mapping; the lowest page is made
        // inaccessible, and the frame written lies within the rest.
        unsafe {
            let stack = mmap(
                std::ptr::null_mut(),
                size,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE | MAP_STACK,
                -1,
                0,
            );
            if stack as isize == -1 {
                eprintln!("fatal error: no memory for a task's stack");
                std::process::abort();
            }
            mprotect(stack, PAGE, PROT_NONE);
            let stack = stack as *mut u8;
            // The frame the first switch restores, from its lowest word: the
            // control words, r15, r14, r13, r12, rbx, rbp, and the return
            // address. The stack is 16-byte aligned once it has returned.
            let top = stack.add(size) as usize;
            let saved = top - 64;
            let frame = saved as *mut u64;
            let control_words: u64 = 0x1F80 | (0x037F << 32);
            let words: [u64; 8] = [
                control_words,
                0,
                0,
                parameter as u64,
                start as *const () as usize as u64,
                0,
                0,
                sayon_fiber_entry as *const () as usize as u64,
            ];
            for (index, word) in words.iter().enumerate() {
                frame.add(index).write(*word);
            }
            Box::into_raw(Box::new(Fiber { stack, size, saved })).cast()
        }
    }

    pub fn switch(target: *mut c_void) {
        let target = target as *mut Fiber;
        let current = CURRENT.with(|current| current.replace(target));
        // SAFETY: both fibers are this thread's and alive; the target's
        // saved stack pointer was stored by a switch away from it, or laid
        // out by `create`.
        unsafe { sayon_fiber_switch(&mut (*current).saved, (*target).saved) }
    }

    pub fn delete(fiber: *mut c_void) {
        // SAFETY: a fiber that is not running, deleted once.
        unsafe {
            let fiber = Box::from_raw(fiber as *mut Fiber);
            if !fiber.stack.is_null() {
                munmap(fiber.stack.cast(), fiber.size);
            }
        }
    }
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
mod os {
    use std::ffi::c_void;

    fn unavailable() -> ! {
        eprintln!("fatal error: compiled programs can run tasks only on Windows and Linux so far");
        std::process::abort()
    }

    pub fn adopt_thread() -> *mut c_void {
        unavailable()
    }

    pub fn create(_: unsafe extern "system" fn(*mut c_void), _: usize) -> *mut c_void {
        unavailable()
    }

    pub fn switch(_: *mut c_void) {
        unavailable()
    }

    pub fn delete(_: *mut c_void) {}
}

// ----- the scheduler ----------------------------------------------------------------------------

/// Whether the tasks of a thread take turns by returning to the scheduler
/// rather than by switching stacks: WebAssembly has no fibers. There, a
/// waiting operation records what it waits for and gives [`SUSPENDED`]; the
/// code generated for WebAssembly suspends the task's functions, each an
/// LLVM coroutine, up to the scheduler, which resumes them once the task
/// can go on, and they call the operation again (see [`run`]).
const STACKLESS: bool = cfg!(target_family = "wasm");

/// The status of an operation, or of a function, that stopped to wait.
pub const SUSPENDED: i32 = 2;

/// The frame of the function that last stopped to wait, for its caller to
/// resume; null when a waiting operation of the runtime stopped. Only
/// WebAssembly programs use it.
#[unsafe(no_mangle)]
pub static mut sayon_frame: *mut u8 = std::ptr::null_mut();

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
    /// Where the task's function puts its error, which outlives the call
    /// that starts it when the function stops to wait.
    error: *mut SayonError,
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
    /// The finally functions the task runs, which cannot wait.
    finally: usize,
    /// Without fibers: the frame of the task's function while it waits.
    frame: *mut u8,
    /// Without fibers: what the task waited for when the scheduler resumed
    /// it, which its waiting operation goes on from.
    resumed: Option<Wait>,
    /// Without fibers: the waiting operation to call again on resuming.
    again: Option<Box<dyn FnOnce() -> i32>>,
}

impl Fiber {
    fn new(os: *mut c_void, task: *mut TaskBlock, started: bool) -> Fiber {
        Fiber {
            os,
            task,
            wait: Wait::None,
            deadlock: false,
            started,
            done: false,
            cursor: 0,
            locks: 0,
            callbacks: 0,
            finally: 0,
            frame: std::ptr::null_mut(),
            resumed: None,
            again: None,
        }
    }
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
            fibers: vec![Fiber::new(std::ptr::null_mut(), std::ptr::null_mut(), true)],
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

/// The running task's fiber; none while the tasks finish.
fn running() -> Option<&'static mut Fiber> {
    let s = scheduler();
    s.fibers.get_mut(s.current)
}

/// Counts a mutex's lock the running task takes (1) or frees (-1).
pub(crate) fn count_lock(change: isize) {
    if let Some(fiber) = running() {
        fiber.locks = fiber.locks.saturating_add_signed(change);
    }
}

/// Counts a call of a C function that can call Sayon functions back,
/// which the running task starts (1) or ends (-1).
pub(crate) fn count_callbacks(change: isize) {
    if let Some(fiber) = running() {
        fiber.callbacks = fiber.callbacks.saturating_add_signed(change);
    }
}

/// Counts a finally function the running task starts (1) or ends (-1).
pub(crate) fn count_finally(change: isize) {
    if let Some(fiber) = running() {
        fiber.finally = fiber.finally.saturating_add_signed(change);
    }
}

/// Why a task cannot wait at all.
#[derive(Clone, Copy)]
pub(crate) enum Refusal {
    /// It holds a mutex's lock.
    Locked,
    /// It runs a function that C called, whose caller cannot be left.
    Callback,
    /// It runs a finally function, which destroys a value where it is.
    Finally,
}

impl Refusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Refusal::Locked => "LockError",
            Refusal::Callback => "CallbackError",
            Refusal::Finally => "FinallyError",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Refusal::Locked => "a task cannot wait while it holds a mutex's lock",
            Refusal::Callback => "a function that C calls cannot wait",
            Refusal::Finally => "a finally function cannot wait",
        }
    }
}

fn refusal(index: usize) -> Option<Refusal> {
    let fiber = &scheduler().fibers[index];
    if fiber.locks > 0 {
        Some(Refusal::Locked)
    } else if fiber.finally > 0 {
        Some(Refusal::Finally)
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
    /// Without fibers: the task stops here, and the scheduler resumes it
    /// once it can go on.
    Suspended,
}

/// Lets the running task wait, switching to another task until this one
/// can go on. Without fibers, the wait is recorded and the task suspended.
fn wait(what: Wait) -> Result<(), Interrupted> {
    let current = scheduler().current;
    if let Some(refusal) = refusal(current) {
        return Err(Interrupted::Refused(refusal));
    }
    if is_cancelled(current) {
        return Err(Interrupted::Cancelled);
    }
    scheduler().fibers[current].wait = what;
    if STACKLESS {
        return Err(Interrupted::Suspended);
    }
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

/// Without fibers: what the running task's waiting operation waited for,
/// when the scheduler has just resumed it there, as `wait` would end: an
/// error when the task was cancelled or every task waits. `None` when the
/// operation starts afresh, and always with fibers.
fn resumed() -> Option<Result<Wait, Interrupted>> {
    let current = scheduler().current;
    let fiber = &mut scheduler().fibers[current];
    let wait = fiber.resumed.take()?;
    if std::mem::take(&mut fiber.deadlock) {
        return Some(Err(Interrupted::Deadlock));
    }
    if is_cancelled(current) {
        return Some(Err(Interrupted::Cancelled));
    }
    Some(Ok(wait))
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
    if STACKLESS {
        // Without fibers, the other tasks wait meanwhile.
        let timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        network::wait_any(std::slice::from_ref(&what), timeout);
    } else if let Some(index) = next_fiber() {
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

/// The status of a waiting operation whose wait ended without what it
/// waited for: an error in the caller's slot, or, when the task suspends,
/// [`SUSPENDED`], keeping `again` for the scheduler to call on resuming it.
unsafe fn interrupted(
    err: *mut *mut SayonError,
    why: Interrupted,
    location: *const c_char,
    again: impl FnOnce() -> i32 + 'static,
) -> i32 {
    // SAFETY: the caller's error slot.
    unsafe {
        match why {
            Interrupted::Suspended => {
                let s = scheduler();
                s.fibers[s.current].again = Some(Box::new(again));
                // No function's frame: the operation itself is to resume.
                *std::ptr::addr_of_mut!(sayon_frame) = std::ptr::null_mut();
                SUSPENDED
            }
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

/// Calls the function of the task of fiber `index`, which gives its status,
/// or [`SUSPENDED`] when it stops to wait.
unsafe fn start_task(index: usize) -> i32 {
    let task = scheduler().fibers[index].task;
    // SAFETY: the scheduler holds a reference to the task.
    unsafe {
        call_function(
            (*task).code,
            (*task).environment,
            (*task).result,
            &mut (*task).error,
        )
    }
}

/// Records how the task of fiber `index` ended, from its function's status.
fn finish_task(index: usize, status: i32) {
    let task = scheduler().fibers[index].task;
    // SAFETY: the scheduler holds a reference to the task.
    unsafe {
        let error = std::mem::replace(&mut (*task).error, std::ptr::null_mut());
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
}

/// The fiber to run after a task finished: the next that can go on; when
/// the tasks left wait for each other, one of them, which fails; with none
/// left, the main task's.
fn after_finish() -> usize {
    next_fiber().unwrap_or_else(|| {
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
    })
}

// ----- without fibers ---------------------------------------------------------------------------

/// How running a task's functions until they stopped ended.
enum Ran {
    /// They stopped to wait.
    Suspended,
    /// They finished, with this status.
    Finished(i32),
}

#[cfg(target_family = "wasm")]
unsafe extern "C" {
    /// Resumes the suspended frame of a function, or with null the running
    /// task's waiting operation, and gives its status, or [`SUSPENDED`]
    /// when it stopped again. The code generated for WebAssembly defines it.
    fn sayon_coroutine_resume(frame: *mut u8) -> i32;
}

/// Targets with fibers never suspend a frame.
#[cfg(not(target_family = "wasm"))]
unsafe extern "C" fn sayon_coroutine_resume(_frame: *mut u8) -> i32 {
    unreachable!("tasks switch fibers on this target")
}

/// Without fibers: starts or resumes the task of fiber `index`, which runs
/// until its functions stop to wait or finish.
fn run(index: usize) -> Ran {
    let s = scheduler();
    s.current = index;
    let fiber = &mut s.fibers[index];
    let status = if fiber.started {
        fiber.resumed = Some(std::mem::replace(&mut fiber.wait, Wait::None));
        let frame = fiber.frame;
        // SAFETY: the suspended frame of the task's first function.
        unsafe { sayon_coroutine_resume(frame) }
    } else {
        fiber.started = true;
        fiber.wait = Wait::None;
        // SAFETY: the task of this fiber.
        unsafe { start_task(index) }
    };
    // The task may have started others, so the fibers are looked up again.
    let fiber = &mut scheduler().fibers[index];
    if status == SUSPENDED {
        if fiber.frame.is_null() {
            // SAFETY: the frame the task's first function left on stopping.
            fiber.frame = unsafe { *std::ptr::addr_of!(sayon_frame) };
        }
        return Ran::Suspended;
    }
    fiber.frame = std::ptr::null_mut();
    fiber.resumed = None;
    fiber.again = None;
    if index != 0 {
        finish_task(index, status);
    }
    Ran::Finished(status)
}

/// Without fibers: the fiber to run after the running one stopped to wait:
/// the next that can go on, or, when none can, the one that stopped, which
/// then finds that every task waits.
fn after_wait() -> usize {
    next_fiber().unwrap_or_else(|| {
        let s = scheduler();
        s.fibers[s.current].deadlock = true;
        s.current
    })
}

/// Without fibers: called when the main task's functions stopped to wait.
/// Runs the tasks, resuming the main task's functions in their turn, until
/// those finish, and gives their status.
#[unsafe(no_mangle)]
pub extern "C" fn sayon_tasks_wait_main() -> i32 {
    // SAFETY: the frame the main task's first function left on stopping.
    scheduler().fibers[0].frame = unsafe { *std::ptr::addr_of!(sayon_frame) };
    let mut next = after_wait();
    loop {
        match run(next) {
            Ran::Finished(status) if next == 0 => return status,
            Ran::Finished(_) => next = after_finish(),
            Ran::Suspended => next = after_wait(),
        }
    }
}

/// Without fibers: calls the running task's waiting operation again, once
/// the scheduler has resumed the task.
#[unsafe(no_mangle)]
pub extern "C" fn sayon_task_retry() -> i32 {
    let s = scheduler();
    let again = s.fibers[s.current]
        .again
        .take()
        .expect("a waiting operation to call again");
    again()
}

/// The first function of every task's fiber. It never returns: when the
/// task finishes, it switches to another fiber for good.
unsafe extern "system" fn task_start(parameter: *mut c_void) {
    let index = parameter as usize;
    // SAFETY: the task of this fiber.
    let status = unsafe { start_task(index) };
    finish_task(index, status);
    let next = after_finish();
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
        if !STACKLESS && scheduler().fibers[0].os.is_null() {
            scheduler().fibers[0].os = os::adopt_thread();
        }
        let address = std::mem::size_of::<*const u8>();
        let code = *(body as *const *const u8);
        let environment = *(body.add(address) as *const *mut u8);
        std::ptr::write_bytes(body, 0, 2 * address);
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
            error: std::ptr::null_mut(),
        }));
        let index = scheduler().fibers.len();
        let os = if STACKLESS {
            std::ptr::null_mut()
        } else {
            os::create(task_start, index)
        };
        scheduler().fibers.push(Fiber::new(os, task, false));
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
        let again = move || sayon_task_join(err, slot, out, location);
        if let Some(Err(why)) = resumed() {
            return interrupted(err, why, location, again);
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
                return interrupted(err, why, location, again);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_task_yield(
    err: *mut *mut SayonError,
    location: *const c_char,
) -> i32 {
    // SAFETY: the caller's error slot.
    unsafe {
        let again = move || sayon_task_yield(err, location);
        let why = match resumed() {
            // Resumed, the task has let the others run.
            Some(Ok(_)) => return 0,
            Some(Err(why)) => why,
            None => match wait(Wait::Yield) {
                Ok(()) => return 0,
                Err(why) => why,
            },
        };
        interrupted(err, why, location, again)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_task_sleep(
    err: *mut *mut SayonError,
    nanoseconds: i64,
    location: *const c_char,
) -> i32 {
    // SAFETY: the caller's error slot.
    unsafe {
        let again = move || sayon_task_sleep(err, nanoseconds, location);
        // Resumed, the task goes on sleeping until the time it chose.
        let until = match resumed() {
            Some(Err(why)) => return interrupted(err, why, location, again),
            Some(Ok(Wait::Sleep(until))) => until,
            _ => {
                if nanoseconds <= 0 {
                    return 0;
                }
                Instant::now() + Duration::from_nanos(nanoseconds as u64)
            }
        };
        while Instant::now() < until {
            if let Err(why) = wait(Wait::Sleep(until)) {
                return interrupted(err, why, location, again);
            }
        }
        0
    }
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
        if STACKLESS {
            // The tasks run in turn until the main task's comes again.
            let mut next = index;
            while next != 0 {
                next = match run(next) {
                    Ran::Suspended => after_wait(),
                    Ran::Finished(_) => after_finish(),
                };
            }
        } else {
            switch_to(index);
        }
        scheduler().current = 0;
    }
    let _ = std::io::stdout().flush();
    let mut failed = false;
    // The main task's fiber stays, for the finally functions that destroy
    // the program's constants.
    let fibers = scheduler().fibers.split_off(1);
    for fiber in fibers {
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
        let again = move || sayon_channel_send(err, slot, message, keep, location);
        let number = match resumed() {
            Some(Err(why)) => return interrupted(err, why, location, again),
            // Resumed after sending, a rendezvous goes on waiting for its
            // message to be received.
            Some(Ok(Wait::Rendezvous(_, number))) => number,
            _ => {
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
                        std::ptr::copy_nonoverlapping(
                            message,
                            buffer.as_mut_ptr() as *mut u8,
                            size,
                        );
                        std::ptr::write_bytes(message, 0, size);
                        state.queue.push_back(buffer);
                        state.sent += 1;
                        break state.sent;
                    }
                    drop(state);
                    if let Err(why) = wait(Wait::Send(*slot)) {
                        return interrupted(err, why, location, again);
                    }
                };
                channel.changed();
                number
            }
        };
        // Other tasks receive while this one waits.
        if channel.capacity == 0 {
            while channel.state().received < number {
                if let Err(why) = wait(Wait::Rendezvous(*slot, number)) {
                    return interrupted(err, why, location, again);
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
        let again = move || sayon_channel_receive(err, slot, out, location);
        if let Some(Err(why)) = resumed() {
            return interrupted(err, why, location, again);
        }
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
                return interrupted(err, why, location, again);
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
        let again = move || sayon_channel_select(err, channels, position_out, out, location);
        if let Some(Err(why)) = resumed() {
            return interrupted(err, why, location, again);
        }
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
                return interrupted(err, why, location, again);
            }
        }
    }
}
