//! Tasks and channels of compiled programs.
//!
//! Each task is an operating-system fiber with a stack of its own, and the
//! tasks take turns on the program's one thread. A task waits inside one of
//! the functions here, such as `sayon_channel_receive`: it records what it
//! waits for and switches to the next task that can go on, round the tasks
//! in order, and when it is switched back to it looks again. The rules are
//! the interpreter's (see its `machine/tasks.rs`), so both execution modes
//! run a program's tasks in the same order.
//!
//! Everything here runs on one thread, and no reference into the scheduler
//! is held across a switch between fibers.

// The functions called by generated code rely on the same conventions as
// those of `ffi`, where their contracts are described.
#![allow(clippy::missing_safety_doc)]

use crate::ffi::{DropFn, SayonArray, SayonError, allocated, released, set_error};
use crate::host::{CANCELLED_CODE, EXIT_CODE, exit_status};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::io::Write;
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
pub struct TaskBlock {
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
    references: usize,
    capacity: usize,
    closed: bool,
    element_size: usize,
    drop: DropFn,
    queue: VecDeque<Vec<u64>>,
    sent: u64,
    received: u64,
}

impl ChannelBlock {
    fn receivable(&self) -> bool {
        !self.queue.is_empty() || self.closed
    }

    fn has_room(&self) -> bool {
        self.closed || self.queue.len() < self.capacity.max(1)
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
}

impl Wait {
    fn ready(&self, now: Instant) -> bool {
        // SAFETY: what a fiber waits for is kept alive while it waits.
        unsafe {
            match self {
                Wait::None => false,
                Wait::Receive(channel) => (**channel).receivable(),
                Wait::Send(channel) => (**channel).has_room(),
                Wait::Rendezvous(channel, number) => (**channel).received >= *number,
                Wait::Join(task) => !matches!((**task).outcome, Outcome::Running),
                Wait::Select(channels) => channels.iter().any(|c| (**c).receivable()),
                Wait::Sleep(until) => now >= *until,
                Wait::Yield => true,
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
}

struct Scheduler {
    fibers: Vec<Fiber>,
    current: usize,
    stopping: bool,
    exit_status: Option<i32>,
}

struct Global(UnsafeCell<Option<Scheduler>>);

// SAFETY: compiled programs run their tasks on one thread.
unsafe impl Sync for Global {}

static SCHEDULER: Global = Global(UnsafeCell::new(None));

/// The scheduler, made with the main task when first needed.
fn scheduler() -> &'static mut Scheduler {
    // SAFETY: one thread, and no caller keeps the reference across a switch.
    unsafe {
        (*SCHEDULER.0.get()).get_or_insert_with(|| Scheduler {
            fibers: vec![Fiber {
                os: std::ptr::null_mut(),
                task: std::ptr::null_mut(),
                wait: Wait::None,
                deadlock: false,
                started: true,
                done: false,
                cursor: 0,
            }],
            current: 0,
            stopping: false,
            exit_status: None,
        })
    }
}

fn is_cancelled(index: usize) -> bool {
    let s = scheduler();
    let fiber = &s.fibers[index];
    // SAFETY: the scheduler holds a reference to each task.
    s.stopping || (!fiber.task.is_null() && unsafe { (*fiber.task).cancel })
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
    // SAFETY: the environment block starts with its destroy function.
    unsafe {
        let environment = (*task).environment;
        if !environment.is_null() {
            let destroy: unsafe extern "C" fn(*mut u8) = *(environment as *const _);
            destroy(environment);
            (*task).environment = std::ptr::null_mut();
        }
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
/// can yet but one sleeps, after waiting for the earliest to wake.
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
        let wake = scheduler()
            .fibers
            .iter()
            .filter(|fiber| !fiber.done)
            .filter_map(|fiber| match fiber.wait {
                Wait::Sleep(until) => Some(until),
                _ => None,
            })
            .min()?;
        let _ = std::io::stdout().flush();
        std::thread::sleep(wake.saturating_duration_since(now));
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
}

/// Lets the running task wait, switching to another task until this one
/// can go on.
fn wait(what: Wait) -> Result<(), Interrupted> {
    let current = scheduler().current;
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
        type Code = unsafe extern "C" fn(*mut u8, *mut *mut SayonError, i64, *mut u8) -> i32;
        let code: Code = std::mem::transmute((*task).code);
        let environment = (*task).environment;
        // A named function's value has no environment; its code never reads it.
        let structure = environment.wrapping_add(16);
        let mut error: *mut SayonError = std::ptr::null_mut();
        let status = code((*task).result, &mut error, 1, structure);
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
    unsafe { (**slot).detached = true }
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
    // SAFETY: one thread; no task runs while this is the running fiber.
    unsafe {
        if (*SCHEDULER.0.get()).is_none() {
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
    // SAFETY: read on the program's thread.
    unsafe { (*SCHEDULER.0.get()).as_ref().and_then(|s| s.exit_status) }
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
        *out = Box::into_raw(Box::new(ChannelBlock {
            references: 1,
            capacity: capacity as usize,
            closed: false,
            element_size: element_size as usize,
            drop,
            queue: VecDeque::new(),
            sent: 0,
            received: 0,
        }));
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
        (**src).references += 1;
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
        (*channel).references -= 1;
        if (*channel).references > 0 {
            return;
        }
        let channel = Box::from_raw(channel);
        if let Some(drop) = channel.drop {
            for mut message in channel.queue {
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
        let channel = *slot;
        loop {
            if (*channel).closed {
                // The message is destroyed, unless the sender keeps it.
                if !keep {
                    if let Some(drop) = (*channel).drop {
                        drop(message);
                    }
                    std::ptr::write_bytes(message, 0, (*channel).element_size);
                }
                return set_error(
                    err,
                    "ChannelClosedError",
                    "cannot send on a closed channel".to_string(),
                    location,
                );
            }
            if (*channel).has_room() {
                break;
            }
            if let Err(why) = wait(Wait::Send(channel)) {
                return interrupted(err, why, location);
            }
        }
        let size = (*channel).element_size;
        let mut buffer = vec![0u64; size.div_ceil(8).max(1)];
        std::ptr::copy_nonoverlapping(message, buffer.as_mut_ptr() as *mut u8, size);
        std::ptr::write_bytes(message, 0, size);
        (*channel).queue.push_back(buffer);
        (*channel).sent += 1;
        let number = (*channel).sent;
        // Other tasks receive while this one waits.
        if (*channel).capacity == 0 {
            loop {
                if (*channel).received >= number {
                    break;
                }
                if let Err(why) = wait(Wait::Rendezvous(channel, number)) {
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
        let channel = *slot;
        loop {
            if let Some(buffer) = (*channel).queue.pop_front() {
                (*channel).received += 1;
                std::ptr::copy_nonoverlapping(
                    buffer.as_ptr() as *const u8,
                    out,
                    (*channel).element_size,
                );
                return 0;
            }
            if (*channel).closed {
                return set_error(
                    err,
                    "ChannelClosedError",
                    "the channel is closed and has no more messages".to_string(),
                    location,
                );
            }
            if let Err(why) = wait(Wait::Receive(channel)) {
                return interrupted(err, why, location);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_channel_close(slot: *const *mut ChannelBlock) {
    // SAFETY: the slot holds a live channel.
    unsafe { (**slot).closed = true }
}

/// The position, in an array of channel handles, of one that has a message
/// or is closed, waiting until one is; the search starts after the one this
/// task chose last.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_channel_select(
    err: *mut *mut SayonError,
    channels: *const SayonArray,
    out: *mut i64,
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
                if (*handles[position]).receivable() {
                    scheduler().fibers[current].cursor = position + 1;
                    *out = position as i64;
                    return 0;
                }
            }
            if let Err(why) = wait(Wait::Select(handles.clone())) {
                return interrupted(err, why, location);
            }
        }
    }
}
