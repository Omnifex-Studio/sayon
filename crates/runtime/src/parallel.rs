//! Tasks of compiled programs on threads of their own, and the mutexes
//! they share.
//!
//! A parallel task runs a function value on a new thread, which has a
//! scheduler of its own for the tasks it starts. The checker lets only
//! values that share nothing with their thread cross to another (no
//! `Shared`, `Weak`, task, or channel handle, whose counts are not atomic),
//! so the function's environment and the task's result move between
//! threads as they are. A mutex is a counted block, shared by every thread
//! holding a handle to it, with its value and the task holding its lock.
//!
//! At the end of the program the main thread cancels the tasks still
//! running on other threads, waits for each thread to finish, and reports
//! the failures nothing joined.

// The functions called by generated code rely on the same conventions as
// those of `ffi`, where their contracts are described.
#![allow(clippy::missing_safety_doc)]

use crate::ffi::{DropFn, SayonError, allocated, released, set_error};
use crate::host::{self, CANCELLED_CODE, EXIT_CODE};
use crate::network::{self, Background, IoWait};
use crate::tasks::IoStop;
use std::cell::{RefCell, UnsafeCell};
use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// An error made on one thread and taken over by another.
struct SentError(*mut SayonError);

// SAFETY: an error is plain text, owned by one thread at a time.
unsafe impl Send for SentError {}

/// What a parallel task's handle, its thread, and the program share.
pub struct Shared {
    /// The result's words, or the error, once the task has finished.
    outcome: Mutex<Option<Result<Vec<u64>, SentError>>>,
    cancel: AtomicBool,
    joined: AtomicBool,
    detached: AtomicBool,
    result_size: usize,
    result_drop: DropFn,
}

impl Background for Shared {
    fn ready(&self) -> bool {
        lock(&self.outcome).is_some()
    }
}

impl Drop for Shared {
    /// Destroys a result or error that nothing joined.
    fn drop(&mut self) {
        match self
            .outcome
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            Some(Ok(mut words)) => {
                if let Some(drop) = self.result_drop {
                    // SAFETY: the words hold a result of the task's type.
                    unsafe { drop(words.as_mut_ptr() as *mut u8) };
                }
            }
            Some(Err(mut error)) => {
                // SAFETY: an error owned by the outcome.
                unsafe { crate::ffi::sayon_error_drop(&mut error.0) };
            }
            None => {}
        }
    }
}

/// A parallel task's handle; it starts like a `TaskBlock` with its kind.
#[repr(C)]
pub struct ParallelBlock {
    /// 1, which tells it from a task of the same thread.
    kind: u32,
    shared: Arc<Shared>,
}

/// The program's threads, joined at its end.
static THREADS: Mutex<Vec<(Arc<Shared>, std::thread::JoinHandle<()>)>> = Mutex::new(Vec::new());
/// The status a task's `os.exit` asked for, on any thread.
static EXIT: Mutex<Option<i32>> = Mutex::new(None);
static EXITING: AtomicBool = AtomicBool::new(false);
/// Whether a task failed without being joined, on another thread.
static FAILED: AtomicBool = AtomicBool::new(false);
/// The parallel tasks that have not finished.
static RUNNING: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// On a parallel task's thread: the task.
    static THREAD_TASK: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
}

/// Whether a task asked the whole program to end.
pub(crate) fn exiting() -> bool {
    EXITING.load(Ordering::Relaxed)
}

pub(crate) fn exit_status() -> Option<i32> {
    *lock(&EXIT)
}

pub(crate) fn failed() -> bool {
    FAILED.load(Ordering::Relaxed)
}

fn exit_with(status: i32) {
    lock(&EXIT).get_or_insert(status);
    EXITING.store(true, Ordering::Relaxed);
    network::wake();
}

/// Whether tasks run on threads other than this one.
pub(crate) fn others_run() -> bool {
    THREAD_TASK.with(|task| task.borrow().is_some()) || RUNNING.load(Ordering::Acquire) > 0
}

/// Whether the parallel task this thread runs is to stop.
pub(crate) fn thread_cancelled() -> bool {
    THREAD_TASK.with(|task| {
        task.borrow()
            .as_ref()
            .is_some_and(|task| task.cancel.load(Ordering::Relaxed))
    })
}

/// Starts a task on a thread of its own, running the function value at
/// `body`, which it takes over, and stores its handle in `out`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_task_spawn_parallel(
    err: *mut *mut SayonError,
    body: *mut u8,
    result_size: i64,
    result_drop: DropFn,
    out: *mut *mut ParallelBlock,
    location: *const c_char,
) -> i32 {
    // SAFETY: `body` is a function value: its code and environment block.
    unsafe {
        let code = *(body as *const *const u8) as usize;
        let environment = *(body.add(8) as *const *mut u8) as usize;
        std::ptr::write_bytes(body, 0, 16);
        let shared = Arc::new(Shared {
            outcome: Mutex::new(None),
            cancel: AtomicBool::new(false),
            joined: AtomicBool::new(false),
            detached: AtomicBool::new(false),
            result_size: result_size.max(0) as usize,
            result_drop,
        });
        let worker = shared.clone();
        // The channels of this thread may go with what the task captured.
        crate::tasks::cross_channels();
        // Counted before the thread can finish.
        RUNNING.fetch_add(1, Ordering::AcqRel);
        let started = std::thread::Builder::new()
            .name("task".to_string())
            .spawn(move || run_thread(worker, code, environment));
        match started {
            Ok(handle) => {
                lock(&THREADS).push((shared.clone(), handle));
                allocated();
                *out = Box::into_raw(Box::new(ParallelBlock { kind: 1, shared }));
                0
            }
            Err(error) => {
                RUNNING.fetch_sub(1, Ordering::AcqRel);
                crate::tasks::destroy_function_environment(environment as *mut u8);
                set_error(
                    err,
                    "ThreadError",
                    format!("no thread could be started for the task: {error}"),
                    location,
                )
            }
        }
    }
}

/// The body of a parallel task's thread.
fn run_thread(shared: Arc<Shared>, code: usize, environment: usize) {
    network::prepare_waking();
    THREAD_TASK.with(|task| *task.borrow_mut() = Some(shared.clone()));
    let mut words = vec![0u64; shared.result_size.div_ceil(8).max(1)];
    let mut error: *mut SayonError = std::ptr::null_mut();
    // SAFETY: the function value the task took over, whose captures the
    // checker let cross threads.
    let status = unsafe {
        let status = crate::tasks::call_function(
            code as *const u8,
            environment as *mut u8,
            words.as_mut_ptr() as *mut u8,
            &mut error,
        );
        crate::tasks::destroy_function_environment(environment as *mut u8);
        status
    };
    // The tasks it started on this thread end with it.
    if crate::tasks::finish_local_tasks(status != 0) > 0 {
        FAILED.store(true, Ordering::Relaxed);
    }
    // The result may hold channels made here.
    crate::tasks::cross_channels();
    if let Some(status) = crate::tasks::exit_requested() {
        exit_with(status);
    }
    let outcome = if status == 0 {
        Ok(words)
    } else {
        // SAFETY: the error the function stored.
        let code = unsafe { &(*error).code };
        if code == EXIT_CODE {
            // SAFETY: as above.
            exit_with(host::exit_status(unsafe { &(*error).message }));
        }
        Err(SentError(error))
    };
    *lock(&shared.outcome) = Some(outcome);
    RUNNING.fetch_sub(1, Ordering::AcqRel);
    network::wake();
}

/// Waits for a parallel task and moves its result into `out`, or its error
/// into the error slot.
pub(crate) unsafe fn join_task(
    err: *mut *mut SayonError,
    block: *const ParallelBlock,
    out: *mut u8,
    location: *const c_char,
) -> i32 {
    // SAFETY: a live handle, and the caller's error slot and destination.
    unsafe {
        let shared = &(*block).shared;
        loop {
            let taken = lock(&shared.outcome).take();
            match taken {
                Some(Ok(words)) => {
                    shared.joined.store(true, Ordering::Relaxed);
                    std::ptr::copy_nonoverlapping(
                        words.as_ptr() as *const u8,
                        out,
                        shared.result_size,
                    );
                    return 0;
                }
                Some(Err(error)) => {
                    shared.joined.store(true, Ordering::Relaxed);
                    if !(*err).is_null() {
                        crate::ffi::sayon_error_drop(err);
                    }
                    *err = error.0;
                    return 1;
                }
                None => {}
            }
            match crate::tasks::wait_io(IoWait::Background(shared.clone()), None) {
                Ok(()) => {}
                Err(IoStop::Refused(refusal)) => {
                    return set_error(err, refusal.code(), refusal.message().to_string(), location);
                }
                Err(_) => {
                    return set_error(
                        err,
                        CANCELLED_CODE,
                        "the task was cancelled".to_string(),
                        location,
                    );
                }
            }
        }
    }
}

/// Drops a parallel task's handle: without a join or a detach, the task is
/// cancelled.
pub(crate) unsafe fn drop_task(slot: *mut *mut ParallelBlock) {
    // SAFETY: the slot holds a live handle, which the caller checked.
    unsafe {
        let block = *slot;
        *slot = std::ptr::null_mut();
        let shared = &(*block).shared;
        if !shared.joined.load(Ordering::Relaxed) && !shared.detached.load(Ordering::Relaxed) {
            shared.cancel.store(true, Ordering::Relaxed);
            network::wake();
        }
        drop(Box::from_raw(block));
        released();
    }
}

pub(crate) unsafe fn detach_task(block: *const ParallelBlock) {
    // SAFETY: a live handle.
    let shared = unsafe { &(*block).shared };
    shared.detached.store(true, Ordering::Relaxed);
}

/// At the end of the program: cancels the tasks on other threads, waits for
/// each thread to finish, and reports the failures nothing joined. Gives
/// the exit status a task's `os.exit` asked for, if one did.
pub(crate) fn finish_threads(main_failed: bool) -> Option<i32> {
    let running: Vec<Arc<Shared>> = lock(&THREADS).iter().map(|(s, _)| s.clone()).collect();
    for shared in &running {
        shared.cancel.store(true, Ordering::Relaxed);
    }
    network::wake();
    let mut finished = Vec::new();
    loop {
        let next = lock(&THREADS).pop();
        let Some((shared, handle)) = next else {
            break;
        };
        // Also a task started on another thread meanwhile.
        shared.cancel.store(true, Ordering::Relaxed);
        network::wake();
        let _ = handle.join();
        finished.push(shared);
    }
    for shared in finished {
        if shared.joined.load(Ordering::Relaxed) {
            continue;
        }
        let outcome = lock(&shared.outcome).take();
        if let Some(Err(mut error)) = outcome {
            // SAFETY: the error the task ended with.
            unsafe {
                let problem = &*error.0;
                // When the main task failed, its error is reported instead.
                if problem.code != CANCELLED_CODE && problem.code != EXIT_CODE && !main_failed {
                    FAILED.store(true, Ordering::Relaxed);
                    eprintln!(
                        "error: a task failed: {}: {}",
                        problem.code, problem.message
                    );
                    if let Some(location) = &problem.location {
                        eprintln!("  --> {location}");
                    }
                }
                crate::ffi::sayon_error_drop(&mut error.0);
            }
        } else if let Some(outcome) = outcome {
            // A result nobody joined is destroyed with the state.
            *lock(&shared.outcome) = Some(outcome);
        }
    }
    exit_status()
}

// ----- mutexes -------------------------------------------------------------------------------

/// A mutex: its value, in words, and which task holds its lock.
pub struct MutexBlock {
    references: AtomicUsize,
    holder: Mutex<Option<(std::thread::ThreadId, usize)>>,
    free: Condvar,
    size: usize,
    drop: DropFn,
    /// Read and written only by the task holding the lock.
    value: UnsafeCell<Vec<u64>>,
}

/// Makes a mutex holding the value at `value`, which it takes over.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_mutex_new(
    value: *mut u8,
    size: i64,
    drop: DropFn,
    out: *mut *mut MutexBlock,
) {
    let size = size.max(0) as usize;
    let mut words = vec![0u64; size.div_ceil(8).max(1)];
    // Tasks on other threads may take the value out.
    crate::tasks::cross_channels();
    // SAFETY: `value` holds `size` bytes of the value.
    unsafe {
        std::ptr::copy_nonoverlapping(value, words.as_mut_ptr() as *mut u8, size);
        std::ptr::write_bytes(value, 0, size);
        allocated();
        *out = Box::into_raw(Box::new(MutexBlock {
            references: AtomicUsize::new(1),
            holder: Mutex::new(None),
            free: Condvar::new(),
            size,
            drop,
            value: UnsafeCell::new(words),
        }));
    }
}

/// Takes a mutex's lock, waiting for another thread to free it, and moves
/// its value into `out`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_mutex_take(
    err: *mut *mut SayonError,
    slot: *const *mut MutexBlock,
    out: *mut u8,
    location: *const c_char,
) -> i32 {
    // SAFETY: the slot holds a live mutex.
    unsafe {
        let mutex = &**slot;
        let me = (std::thread::current().id(), crate::tasks::current_fiber());
        let mut holder = lock(&mutex.holder);
        while let Some(held) = *holder {
            if held == me {
                return set_error(
                    err,
                    "LockError",
                    "the task holds the mutex's lock already".to_string(),
                    location,
                );
            }
            holder = mutex.free.wait(holder).unwrap_or_else(|e| e.into_inner());
        }
        *holder = Some(me);
        drop(holder);
        let words = &mut *mutex.value.get();
        std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, out, mutex.size);
        std::ptr::write_bytes(words.as_mut_ptr() as *mut u8, 0, mutex.size);
        crate::tasks::count_lock(1);
        0
    }
}

/// Moves a value back into a mutex and frees its lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_mutex_put(slot: *const *mut MutexBlock, value: *mut u8) {
    crate::tasks::cross_channels();
    // SAFETY: the slot holds a live mutex whose lock the running task holds.
    unsafe {
        let mutex = &**slot;
        let words = &mut *mutex.value.get();
        std::ptr::copy_nonoverlapping(value, words.as_mut_ptr() as *mut u8, mutex.size);
        std::ptr::write_bytes(value, 0, mutex.size);
        *lock(&mutex.holder) = None;
        mutex.free.notify_one();
        crate::tasks::count_lock(-1);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_mutex_clone(src: *const *mut MutexBlock, dst: *mut *mut MutexBlock) {
    // SAFETY: the source holds a live mutex.
    unsafe {
        (**src).references.fetch_add(1, Ordering::Relaxed);
        *dst = *src;
    }
}

/// Drops a handle; the last one destroys the mutex and its value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_mutex_release(slot: *mut *mut MutexBlock) {
    // SAFETY: the slot holds null or a live mutex.
    unsafe {
        let mutex = *slot;
        if mutex.is_null() {
            return;
        }
        *slot = std::ptr::null_mut();
        if (*mutex).references.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let mut block = Box::from_raw(mutex);
        if let Some(drop) = block.drop {
            drop(block.value.get_mut().as_mut_ptr() as *mut u8);
        }
        released();
    }
}
