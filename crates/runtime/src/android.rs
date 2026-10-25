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
//!
//! The activity's window is the screen a program's window of
//! `standard/graphics` shows: what happens to it, and the user's touches
//! and keys, become the window's events, and a frame goes to it through a
//! buffer of its own, which Android locks for the program to fill.

use crate::window::Event;
use std::collections::VecDeque;
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
    external_data_path: *const c_char,
    sdk_version: i32,
    instance: *mut c_void,
    asset_manager: *mut c_void,
}

type Notice = Option<unsafe extern "C" fn(*mut Activity)>;
type QueueNotice = Option<unsafe extern "C" fn(*mut Activity, *mut c_void)>;
type FocusNotice = Option<unsafe extern "C" fn(*mut Activity, i32)>;

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
    window_focus_changed: FocusNotice,
    native_window_created: QueueNotice,
    native_window_resized: QueueNotice,
    native_window_redraw_needed: QueueNotice,
    native_window_destroyed: QueueNotice,
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
    fn AInputEvent_getType(event: *const c_void) -> i32;
    fn AKeyEvent_getAction(event: *const c_void) -> i32;
    fn AKeyEvent_getKeyCode(event: *const c_void) -> i32;
    fn AMotionEvent_getAction(event: *const c_void) -> i32;
    fn AMotionEvent_getX(event: *const c_void, pointer: usize) -> f32;
    fn AMotionEvent_getY(event: *const c_void, pointer: usize) -> f32;
    fn ANativeWindow_getWidth(window: *mut c_void) -> i32;
    fn ANativeWindow_getHeight(window: *mut c_void) -> i32;
    fn ANativeWindow_setBuffersGeometry(
        window: *mut c_void,
        width: i32,
        height: i32,
        format: i32,
    ) -> i32;
    fn ANativeWindow_lock(window: *mut c_void, buffer: *mut Buffer, bounds: *mut c_void) -> i32;
    fn ANativeWindow_unlockAndPost(window: *mut c_void) -> i32;
    fn AConfiguration_new() -> *mut c_void;
    fn AConfiguration_fromAssetManager(configuration: *mut c_void, assets: *mut c_void);
    fn AConfiguration_getDensity(configuration: *const c_void) -> i32;
    fn AConfiguration_delete(configuration: *mut c_void);
}

/// Android's `ANativeWindow_Buffer`: the pixels of a frame, locked.
#[repr(C)]
struct Buffer {
    width: i32,
    height: i32,
    /// Pixels from a row to the next.
    stride: i32,
    format: i32,
    bits: *mut u8,
    reserved: [u32; 6],
}

const FORMAT_RGBA: i32 = 1;
const INPUT_KEY: i32 = 1;
const INPUT_MOTION: i32 = 2;
const ACTION_DOWN: i32 = 0;
const ACTION_UP: i32 = 1;
const ACTION_MOVE: i32 = 2;

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

/// The activity's window, and what happened to it.
struct Screen {
    window: *mut c_void,
    width: i64,
    height: i64,
    /// The screen's pixels for a point of Android's measure, a 160th of an
    /// inch.
    density: f64,
    /// Whether the program has a window open, which takes the events.
    open: bool,
    events: VecDeque<Event>,
}

// SAFETY: Android's window may be drawn on from any thread; the lock keeps
// it from being destroyed meanwhile.
unsafe impl Send for Screen {}

static SCREEN: Mutex<Screen> = Mutex::new(Screen {
    window: std::ptr::null_mut(),
    width: 0,
    height: 0,
    density: 1.0,
    open: false,
    events: VecDeque::new(),
});

fn screen() -> std::sync::MutexGuard<'static, Screen> {
    SCREEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Adds an event, while the program has a window to take it.
fn happened(event: Event) {
    let mut screen = screen();
    if screen.open {
        screen.events.push_back(event);
    }
}

/// Opens or closes the program's window: with one open, the screen's
/// events gather, the first telling its size. Opening waits a while for
/// the activity's window, which Android makes once the app is in front.
pub fn screen_open(open: bool) {
    let waited = std::time::Instant::now();
    while open && screen().window.is_null() && waited.elapsed().as_secs() < 10 {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut screen = screen();
    screen.open = open;
    screen.events.clear();
    if open && !screen.window.is_null() {
        let size = Event::Resized(screen.width, screen.height);
        screen.events.push_back(size);
    }
}

pub fn screen_events() -> Vec<Event> {
    screen().events.drain(..).collect()
}

pub fn screen_size() -> (i64, i64) {
    let screen = screen();
    (screen.width, screen.height)
}

pub fn screen_density() -> f64 {
    screen().density
}

/// Shows the frame `frame` makes for the window's size, a row of pixels
/// after another; without a window, as while the app is in the background,
/// nothing.
pub fn screen_present(frame: impl FnOnce(usize, usize) -> Vec<u8>) {
    let screen = screen();
    if screen.window.is_null() {
        return;
    }
    // SAFETY: the window lives while the lock is held: Android's callback
    // that destroys it waits for the lock.
    unsafe {
        ANativeWindow_setBuffersGeometry(screen.window, 0, 0, FORMAT_RGBA);
        let mut buffer: Buffer = std::mem::zeroed();
        if ANativeWindow_lock(screen.window, &mut buffer, std::ptr::null_mut()) != 0 {
            return;
        }
        let (width, height) = (buffer.width.max(0) as usize, buffer.height.max(0) as usize);
        let pixels = frame(width, height);
        for row in 0..height {
            let to = buffer.bits.add(row * buffer.stride as usize * 4);
            std::ptr::copy_nonoverlapping(pixels.as_ptr().add(row * width * 4), to, width * 4);
        }
        ANativeWindow_unlockAndPost(screen.window);
    }
}

/// A key's name, as `standard/graphics` names keys.
fn key(code: i32) -> &'static str {
    const LETTERS: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    const FUNCTIONS: [&str; 12] = [
        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
    ];
    match code {
        7..=16 => DIGITS[(code - 7) as usize],
        29..=54 => LETTERS[(code - 29) as usize],
        19 => "ArrowUp",
        20 => "ArrowDown",
        21 => "ArrowLeft",
        22 => "ArrowRight",
        57 | 58 => "Alt",
        59 | 60 => "Shift",
        61 => "Tab",
        62 => " ",
        66 => "Enter",
        67 => "Backspace",
        92 => "PageUp",
        93 => "PageDown",
        111 => "Escape",
        112 => "Delete",
        113 | 114 => "Control",
        122 => "Home",
        123 => "End",
        124 => "Insert",
        131..=142 => FUNCTIONS[(code - 131) as usize],
        _ => "Unidentified",
    }
}

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
        callbacks.pause = Some(paused);
        callbacks.resume = Some(resumed);
        callbacks.window_focus_changed = Some(focus_changed);
        callbacks.native_window_created = Some(window_created);
        callbacks.native_window_resized = Some(window_created);
        callbacks.native_window_destroyed = Some(window_destroyed);
        callbacks.input_queue_created = Some(input_queue_created);
        callbacks.input_queue_destroyed = Some(input_queue_destroyed);
        // The screen's density, in Android's measure: 160 for a point.
        let configuration = AConfiguration_new();
        AConfiguration_fromAssetManager(configuration, (*activity).asset_manager);
        let density = AConfiguration_getDensity(configuration);
        AConfiguration_delete(configuration);
        if density > 0 && density < 0xFFFF {
            screen().density = density as f64 / 160.0;
        }
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
    happened(Event::Closed);
    let mut life = life();
    if life.activity == activity {
        life.activity = std::ptr::null_mut();
    }
    if life.ended && life.activity.is_null() {
        // SAFETY: the program has ended, and all it wrote is in the log.
        unsafe { _exit(0) };
    }
}

unsafe extern "C" fn paused(_activity: *mut Activity) {
    happened(Event::Paused);
}

unsafe extern "C" fn resumed(_activity: *mut Activity) {
    happened(Event::Resumed);
}

unsafe extern "C" fn focus_changed(_activity: *mut Activity, focused: i32) {
    happened(Event::Focused(focused != 0));
}

/// The activity has a window to show, or its window has a new size.
unsafe extern "C" fn window_created(_activity: *mut Activity, window: *mut c_void) {
    let mut screen = screen();
    // SAFETY: the window Android gives, valid until it is destroyed.
    let (width, height) = unsafe {
        (
            ANativeWindow_getWidth(window) as i64,
            ANativeWindow_getHeight(window) as i64,
        )
    };
    screen.window = window;
    if (width, height) != (screen.width, screen.height) {
        screen.width = width;
        screen.height = height;
        if screen.open {
            screen.events.push_back(Event::Resized(width, height));
        }
    }
}

/// The window is going away, as when the app goes to the background; the
/// lock makes it wait for a frame being drawn.
unsafe extern "C" fn window_destroyed(_activity: *mut Activity, _window: *mut c_void) {
    screen().window = std::ptr::null_mut();
}

/// Takes the activity's input from its queue on the main thread, leaving
/// every event to Android's own handling, such as Back finishing the
/// activity, but telling a program's window of touches and keys. An app
/// that took none would stop answering.
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
            if AInputQueue_preDispatchEvent(queue, event) != 0 {
                continue;
            }
            match AInputEvent_getType(event) {
                INPUT_KEY => match AKeyEvent_getAction(event) {
                    ACTION_DOWN => happened(Event::KeyDown(key(AKeyEvent_getKeyCode(event)))),
                    ACTION_UP => happened(Event::KeyUp(key(AKeyEvent_getKeyCode(event)))),
                    _ => {}
                },
                INPUT_MOTION => {
                    let (x, y) = (
                        AMotionEvent_getX(event, 0) as i64,
                        AMotionEvent_getY(event, 0) as i64,
                    );
                    match AMotionEvent_getAction(event) & 0xFF {
                        ACTION_DOWN => happened(Event::PointerDown(x, y, 1)),
                        ACTION_UP => happened(Event::PointerUp(x, y, 1)),
                        ACTION_MOVE => happened(Event::PointerMoved(x, y)),
                        _ => {}
                    }
                }
                _ => {}
            }
            AInputQueue_finishEvent(queue, event, 0);
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
