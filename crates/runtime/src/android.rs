//! An Android app's entry. The app's package holds the program as a shared
//! library, which Android's `NativeActivity` loads and starts by calling
//! `ANativeActivity_onCreate` on the app's main thread. The program's `main`
//! runs on a thread of its own, so that the app answers its user meanwhile,
//! and what it writes to its output and errors goes to Android's log under
//! the tag `sayon`, a line at a time; the files it names are the app's own,
//! in its private directory. When the program ends, its activity
//! finishes; once the activity is gone too, the process ends, so that the
//! next launch runs the program afresh. A program run from `adb shell`
//! instead is an ordinary executable, with the process's own streams.

use std::ffi::{CString, c_char, c_void};
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// The start of Android's `ANativeActivity`, which is all the runtime reads.
#[repr(C)]
pub struct Activity {
    callbacks: *mut Callbacks,
    vm: *mut c_void,
    env: *mut c_void,
    object: *mut c_void,
    /// The app's private directory.
    internal_data_path: *const c_char,
}

type Notice = Option<unsafe extern "C" fn(*mut Activity)>;
type QueueNotice = Option<unsafe extern "C" fn(*mut Activity, *mut c_void)>;

/// Android's `ANativeActivityCallbacks`: what the activity calls as its
/// life goes on. Those left empty do nothing.
#[repr(C)]
struct Callbacks {
    start: Notice,
    resume: Notice,
    save_instance_state: *const c_void,
    pause: Notice,
    stop: Notice,
    destroy: Notice,
    window_focus_changed: *const c_void,
    native_window_created: *const c_void,
    native_window_resized: *const c_void,
    native_window_redraw_needed: *const c_void,
    native_window_destroyed: *const c_void,
    input_queue_created: QueueNotice,
    input_queue_destroyed: QueueNotice,
    content_rect_changed: *const c_void,
    configuration_changed: Notice,
    low_memory: Notice,
}

type LooperCallback = unsafe extern "C" fn(fd: i32, events: i32, data: *mut c_void) -> i32;

unsafe extern "C" {
    /// The program's entry, which the compiler generates.
    fn main(argc: i32, argv: *const *const c_char) -> i32;
    fn ANativeActivity_finish(activity: *mut Activity);
    /// Ends the process at once. `exit` would run the destructors of the
    /// libraries Android loaded while their own threads still use them.
    fn _exit(status: i32) -> !;
    fn __android_log_write(priority: i32, tag: *const c_char, text: *const c_char) -> i32;
    fn ALooper_forThread() -> *mut c_void;
    fn AInputQueue_attachLooper(
        queue: *mut c_void,
        looper: *mut c_void,
        ident: i32,
        callback: Option<LooperCallback>,
        data: *mut c_void,
    );
    fn AInputQueue_detachLooper(queue: *mut c_void);
    fn AInputQueue_getEvent(queue: *mut c_void, event: *mut *mut c_void) -> i32;
    fn AInputQueue_preDispatchEvent(queue: *mut c_void, event: *mut c_void) -> i32;
    fn AInputQueue_finishEvent(queue: *mut c_void, event: *mut c_void, handled: i32);
}

const LOG_INFO: i32 = 4;
const LOG_ERROR: i32 = 6;

/// Android's log keeps at most about 4 KiB of a message; longer lines go
/// in parts.
const LONGEST_MESSAGE: usize = 4000;

/// The program's stack, as large as a Linux process's main thread's.
const STACK: usize = 8 << 20;

/// Whether the program runs in an app, rather than from a shell.
static IN_APP: AtomicBool = AtomicBool::new(false);

/// The program's activity while it lives, and whether the program ended.
struct Life {
    activity: *mut Activity,
    started: bool,
    ended: bool,
}

// SAFETY: the activity is only passed to Android, which allows finishing it
// from any thread.
unsafe impl Send for Life {}

static LIFE: Mutex<Life> = Mutex::new(Life {
    activity: std::ptr::null_mut(),
    started: false,
    ended: false,
});

/// The output and the errors written but not yet a whole line.
static PENDING: Mutex<[Vec<u8>; 2]> = Mutex::new([Vec::new(), Vec::new()]);

fn life() -> std::sync::MutexGuard<'static, Life> {
    LIFE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether the program runs in an app, whose output goes to Android's log.
pub fn in_app() -> bool {
    IN_APP.load(Ordering::Relaxed)
}

/// Called by Android when the activity starts, on the app's main thread.
///
/// # Safety
/// `activity` is the activity Android creates, valid until it is destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ANativeActivity_onCreate(
    activity: *mut Activity,
    _saved_state: *mut c_void,
    _saved_size: usize,
) {
    // SAFETY: the activity's callbacks, which it reads from now on.
    unsafe {
        let callbacks = &mut *(*activity).callbacks;
        callbacks.destroy = Some(destroyed);
        callbacks.input_queue_created = Some(input_queue_created);
        callbacks.input_queue_destroyed = Some(input_queue_destroyed);
    }
    let mut life = life();
    life.activity = activity;
    // An activity made again, as when the app comes back, goes on with the
    // program that already runs.
    if std::mem::replace(&mut life.started, true) {
        return;
    }
    IN_APP.store(true, Ordering::Relaxed);
    std::panic::set_hook(Box::new(|info| report!("fatal error: {info}")));
    // The program's files are the app's own, in its private directory.
    // SAFETY: Android's path, a NUL-terminated string, while the activity
    // lives.
    let directory = unsafe { (*activity).internal_data_path };
    if !directory.is_null() {
        let path = unsafe { std::ffi::CStr::from_ptr(directory) };
        let _ = std::fs::create_dir_all(path.to_string_lossy().as_ref());
        let _ = std::env::set_current_dir(path.to_string_lossy().as_ref());
    }
    let started = std::thread::Builder::new()
        .name("sayon".to_string())
        .stack_size(STACK)
        .spawn(|| {
            // SAFETY: the program's entry, which takes no arguments here.
            let status = unsafe { main(0, std::ptr::null()) };
            ended(status);
        });
    if started.is_err() {
        drop(life);
        report!("fatal error: no thread for the program");
        ended(3);
    }
}

/// Ends the program with `status`: the rest of its output goes to the log,
/// and its activity finishes. Without an activity left, the process ends.
pub fn ended(status: i32) {
    flush();
    log(
        LOG_INFO,
        "sayon-app",
        format!("the program ended with status {status}").as_bytes(),
    );
    let mut life = life();
    life.ended = true;
    if life.activity.is_null() {
        // SAFETY: the program has ended, and all it wrote is in the log.
        unsafe { _exit(status) };
    }
    // SAFETY: the activity lives until `destroyed`, which waits for the lock.
    unsafe { ANativeActivity_finish(life.activity) };
}

/// The activity is gone: once the program has ended too, so is the process.
unsafe extern "C" fn destroyed(activity: *mut Activity) {
    let mut life = life();
    if life.activity == activity {
        life.activity = std::ptr::null_mut();
    }
    if life.ended && life.activity.is_null() {
        // SAFETY: the program has ended, and all it wrote is in the log.
        unsafe { _exit(0) };
    }
}

/// Takes the activity's input from its queue on the main thread, leaving
/// every event to Android's own handling, such as Back finishing the
/// activity. An app that took none would stop answering.
unsafe extern "C" fn input_queue_created(_activity: *mut Activity, queue: *mut c_void) {
    // SAFETY: the main thread's looper, and the queue Android gives.
    unsafe { AInputQueue_attachLooper(queue, ALooper_forThread(), 1, Some(input), queue) };
}

unsafe extern "C" fn input_queue_destroyed(_activity: *mut Activity, queue: *mut c_void) {
    // SAFETY: the queue attached in `input_queue_created`.
    unsafe { AInputQueue_detachLooper(queue) };
}

unsafe extern "C" fn input(_fd: i32, _events: i32, queue: *mut c_void) -> i32 {
    // SAFETY: the attached queue, whose events are each finished once.
    unsafe {
        let mut event = std::ptr::null_mut();
        while AInputQueue_getEvent(queue, &mut event) >= 0 {
            if AInputQueue_preDispatchEvent(queue, event) == 0 {
                AInputQueue_finishEvent(queue, event, 0);
            }
        }
    }
    1
}

/// Writes the program's output (1) or errors (2) to the log, a whole line
/// at a time.
pub fn write(stream: i32, bytes: &[u8]) {
    let priority = if stream == 1 { LOG_INFO } else { LOG_ERROR };
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let buffer = &mut pending[(stream != 1) as usize];
    buffer.extend_from_slice(bytes);
    while let Some(end) = buffer.iter().position(|&byte| byte == b'\n') {
        let line: Vec<u8> = buffer.drain(..=end).collect();
        log(priority, "sayon", &line[..end]);
    }
}

/// Logs what is left of the last lines, when the program ends.
fn flush() {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    for (index, buffer) in pending.iter_mut().enumerate() {
        if !buffer.is_empty() {
            let priority = if index == 0 { LOG_INFO } else { LOG_ERROR };
            log(priority, "sayon", buffer);
            buffer.clear();
        }
    }
}

/// Logs one line, in parts when it is long, cut between characters.
fn log(priority: i32, tag: &str, line: &[u8]) {
    let tag = CString::new(tag).expect("a tag without NUL");
    let mut rest = line;
    loop {
        let mut end = rest.len().min(LONGEST_MESSAGE);
        while end < rest.len() && end > 0 && (rest[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        let part: Vec<u8> = rest[..end]
            .iter()
            .map(|&b| if b == 0 { b' ' } else { b })
            .collect();
        let text = CString::new(part).expect("NUL bytes replaced");
        // SAFETY: both are NUL-terminated strings.
        unsafe { __android_log_write(priority, tag.as_ptr(), text.as_ptr()) };
        rest = &rest[end..];
        if rest.is_empty() {
            return;
        }
    }
}

/// The program's output or errors: the process's own from a shell, or in
/// an app Android's log.
pub struct Stream(pub i32);

impl Stream {
    /// Like a locked standard stream: the same stream.
    pub fn lock(&self) -> Stream {
        Stream(self.0)
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !in_app() {
            return match self.0 {
                1 => std::io::stdout().write(bytes),
                _ => std::io::stderr().write(bytes),
            };
        }
        write(self.0, bytes);
        Ok(bytes.len())
    }

    /// In an app, only whole lines go to the log, the rest once the program
    /// ends.
    fn flush(&mut self) -> std::io::Result<()> {
        if in_app() {
            return Ok(());
        }
        match self.0 {
            1 => std::io::stdout().flush(),
            _ => std::io::stderr().flush(),
        }
    }
}
