//! Windows for `standard/graphics`: a window on the desktop, the canvas of
//! a web page, or the surface of an Android app, which shows a canvas the
//! program drew (see `graphics`). Each platform's part only puts the pixels
//! on the screen and turns what happens to the window into events, so the
//! picture is the same everywhere: a canvas over black, its top left corner
//! at the window's, a pixel of the canvas for a pixel of the screen.
//!
//! Sizes are in the screen's pixels; `scale` says how many of them a
//! point of the system's own measure is, such as 2 on a screen of twice the
//! usual density. Events cross to the library as strings: each event's name
//! and then its fields.

use crate::graphics::Canvas;
use crate::host::HostError;

/// Something that happened to a window, since the program last asked.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The user asked to close the window.
    Closed,
    /// The window's surface has a new size, in pixels.
    Resized(i64, i64),
    KeyDown(&'static str),
    KeyUp(&'static str),
    /// A button or a finger, at a point of the surface: 1 for the primary
    /// button or a touch, 2 for the secondary, 3 for the middle.
    PointerDown(i64, i64, i64),
    PointerUp(i64, i64, i64),
    PointerMoved(i64, i64),
    /// The window took or lost the keyboard.
    Focused(bool),
    /// The app went to the background, or came back.
    Paused,
    Resumed,
}

impl Event {
    fn encode(&self, out: &mut Vec<String>) {
        let mut put = |name: &str, fields: &[String]| {
            out.push(name.to_string());
            out.extend(fields.iter().cloned());
        };
        match self {
            Event::Closed => put("closed", &[]),
            Event::Resized(width, height) => {
                put("resized", &[width.to_string(), height.to_string()])
            }
            Event::KeyDown(key) => put("key_down", &[key.to_string()]),
            Event::KeyUp(key) => put("key_up", &[key.to_string()]),
            Event::PointerDown(x, y, button) => put(
                "pointer_down",
                &[x.to_string(), y.to_string(), button.to_string()],
            ),
            Event::PointerUp(x, y, button) => put(
                "pointer_up",
                &[x.to_string(), y.to_string(), button.to_string()],
            ),
            Event::PointerMoved(x, y) => put("pointer_moved", &[x.to_string(), y.to_string()]),
            Event::Focused(focused) => put("focused", &[focused.to_string()]),
            Event::Paused => put("paused", &[]),
            Event::Resumed => put("resumed", &[]),
        }
    }
}

/// Events for the library: each one's name, then its fields.
fn encode(events: impl IntoIterator<Item = Event>) -> Vec<String> {
    let mut out = Vec::new();
    for event in events {
        event.encode(&mut out);
    }
    out
}

/// The canvas as the screen shows it, `width` by `height` pixels: opaque,
/// over black, the rest of the surface black. `order` gives each pixel's
/// bytes from the canvas's red, green, blue, and alpha.
#[cfg_attr(
    not(any(
        windows,
        target_os = "linux",
        target_os = "android",
        all(target_family = "wasm", target_os = "unknown")
    )),
    allow(dead_code)
)]
fn frame(canvas: &Canvas, width: usize, height: usize, order: [usize; 4]) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 4];
    for row in 0..height.min(canvas.height) {
        for column in 0..width.min(canvas.width) {
            let from = (row * canvas.width + column) * 4;
            let pixel = &canvas.pixels[from..from + 4];
            let alpha = pixel[3] as u32;
            let shown = [0, 1, 2].map(|c| ((pixel[c] as u32 * alpha + 127) / 255) as u8);
            let to = (row * width + column) * 4;
            out[to + order[0]] = shown[0];
            out[to + order[1]] = shown[1];
            out[to + order[2]] = shown[2];
        }
    }
    for pixel in out.as_chunks_mut::<4>().0 {
        pixel[order[3]] = 255;
    }
    out
}

fn no_screen(why: &str) -> HostError {
    ("GraphicsError", format!("no window can open: {why}"))
}

/// A window and its events. It belongs to the thread that opened it, as
/// the systems' windows do.
pub struct Window {
    platform: platform::Window,
    thread: std::thread::ThreadId,
    /// The frames it has shown, and whether it has asked to close for
    /// [`CLOSE_WINDOWS`].
    presented: u64,
    asked: bool,
}

/// The host option that has each window ask to close, as its close button
/// does, once it has shown [`CLOSING_FRAME`] frames: for checks of programs
/// that run until their window closes, on any host, apps and pages too.
pub const CLOSE_WINDOWS: &str = "SAYON_CLOSE_WINDOWS";
pub const CLOSING_FRAME: u64 = 120;

// SAFETY: the window is used only from the thread that opened it, which
// each use checks; the stream table merely holds it.
unsafe impl Send for Window {}

/// Opens a window, `width` by `height` points, titled `title`. On Android
/// and in a web page, the window is the app's or the page's surface.
pub fn open(title: &str, width: i64, height: i64) -> Result<Window, HostError> {
    if !(1..=16384).contains(&width) || !(1..=16384).contains(&height) {
        return Err((
            "ValueError",
            format!(
                "a window of {width} by {height} points is not possible; each side is 1 to 16384"
            ),
        ));
    }
    Ok(Window {
        platform: platform::Window::open(title, width, height)?,
        thread: std::thread::current().id(),
        presented: 0,
        asked: false,
    })
}

impl Window {
    fn here(&self) -> Result<(), HostError> {
        if std::thread::current().id() == self.thread {
            Ok(())
        } else {
            Err((
                "GraphicsError",
                "a window is used on the thread that opened it".to_string(),
            ))
        }
    }

    /// What happened to the window since the last call, for the library.
    pub fn events(&mut self) -> Result<Vec<String>, HostError> {
        self.here()?;
        let mut events = self.platform.events();
        if self.presented >= CLOSING_FRAME && !self.asked && crate::console::option(CLOSE_WINDOWS) {
            self.asked = true;
            events.push(Event::Closed);
        }
        Ok(encode(events))
    }

    pub fn present(&mut self, canvas: &Canvas) -> Result<(), HostError> {
        self.here()?;
        self.presented += 1;
        self.platform.present(canvas)
    }

    pub fn size(&self) -> (i64, i64) {
        self.platform.size()
    }

    pub fn scale(&self) -> f64 {
        self.platform.scale()
    }
}

/// Where there is no screen: WASI, and systems not done yet.
#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    all(target_family = "wasm", target_os = "unknown")
)))]
mod platform {
    use super::*;

    pub struct Window;

    impl Window {
        pub fn open(_: &str, _: i64, _: i64) -> Result<Window, HostError> {
            Err(no_screen(
                "this system has no screen for Sayon's programs yet",
            ))
        }
        pub fn events(&mut self) -> Vec<Event> {
            Vec::new()
        }
        pub fn present(&mut self, _: &Canvas) -> Result<(), HostError> {
            Ok(())
        }
        pub fn size(&self) -> (i64, i64) {
            (0, 0)
        }
        pub fn scale(&self) -> f64 {
            1.0
        }
    }
}

/// Windows: a window of its own class, drawn with GDI. The process is
/// aware of each monitor's density, so sizes are the screen's pixels.
#[cfg(windows)]
mod platform {
    use super::*;
    use std::collections::VecDeque;
    use std::ffi::c_void;

    type Handle = *mut c_void;

    #[repr(C)]
    struct Message {
        window: Handle,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        point: [i32; 2],
        private: u32,
    }

    #[repr(C)]
    struct Class {
        size: u32,
        style: u32,
        procedure: unsafe extern "system" fn(Handle, u32, usize, isize) -> isize,
        class_extra: i32,
        window_extra: i32,
        instance: Handle,
        icon: Handle,
        cursor: Handle,
        background: Handle,
        menu: *const u16,
        name: *const u16,
        small_icon: Handle,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct Paint {
        dc: Handle,
        erase: i32,
        area: Rect,
        restore: i32,
        update: i32,
        reserved: [u8; 32],
    }

    #[repr(C)]
    struct BitmapInfo {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bits: u16,
        compression: u32,
        image_size: u32,
        x_density: i32,
        y_density: i32,
        used: u32,
        important: u32,
        colors: [u32; 1],
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn RegisterClassExW(class: *const Class) -> u16;
        fn CreateWindowExW(
            extended: u32,
            class: *const u16,
            title: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: Handle,
            menu: Handle,
            instance: Handle,
            parameter: *mut c_void,
        ) -> Handle;
        fn DefWindowProcW(window: Handle, message: u32, wparam: usize, lparam: isize) -> isize;
        fn DestroyWindow(window: Handle) -> i32;
        fn PeekMessageW(
            message: *mut Message,
            window: Handle,
            first: u32,
            last: u32,
            remove: u32,
        ) -> i32;
        fn TranslateMessage(message: *const Message) -> i32;
        fn DispatchMessageW(message: *const Message) -> isize;
        fn GetClientRect(window: Handle, rect: *mut Rect) -> i32;
        fn SetWindowLongPtrW(window: Handle, index: i32, value: isize) -> isize;
        fn GetWindowLongPtrW(window: Handle, index: i32) -> isize;
        fn SetWindowPos(
            window: Handle,
            after: Handle,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            flags: u32,
        ) -> i32;
        fn AdjustWindowRectExForDpi(
            rect: *mut Rect,
            style: u32,
            menu: i32,
            extended: u32,
            dpi: u32,
        ) -> i32;
        fn GetDpiForWindow(window: Handle) -> u32;
        fn GetDpiForSystem() -> u32;
        fn SetProcessDpiAwarenessContext(context: isize) -> i32;
        fn LoadCursorW(instance: Handle, name: *const u16) -> Handle;
        fn GetDC(window: Handle) -> Handle;
        fn ReleaseDC(window: Handle, dc: Handle) -> i32;
        fn BeginPaint(window: Handle, paint: *mut Paint) -> Handle;
        fn EndPaint(window: Handle, paint: *const Paint) -> i32;
        fn SetCapture(window: Handle) -> Handle;
        fn ReleaseCapture() -> i32;
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn StretchDIBits(
            dc: Handle,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            source_x: i32,
            source_y: i32,
            source_width: i32,
            source_height: i32,
            bits: *const c_void,
            info: *const BitmapInfo,
            usage: u32,
            operation: u32,
        ) -> i32;
    }

    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> Handle;
    }

    const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const CW_USEDEFAULT: i32 = i32::MIN;
    const GWLP_USERDATA: i32 = -21;
    const PM_REMOVE: u32 = 1;
    const SRCCOPY: u32 = 0x00CC_0020;
    const IDC_ARROW: usize = 32512;
    const PER_MONITOR_AWARE_V2: isize = -4;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;

    const WM_NCCREATE: u32 = 0x0081;
    const WM_SIZE: u32 = 0x0005;
    const WM_SETFOCUS: u32 = 0x0007;
    const WM_KILLFOCUS: u32 = 0x0008;
    const WM_PAINT: u32 = 0x000F;
    const WM_CLOSE: u32 = 0x0010;
    const WM_ERASEBKGND: u32 = 0x0014;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const WM_SYSKEYDOWN: u32 = 0x0104;
    const WM_SYSKEYUP: u32 = 0x0105;
    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_RBUTTONDOWN: u32 = 0x0204;
    const WM_RBUTTONUP: u32 = 0x0205;
    const WM_MBUTTONDOWN: u32 = 0x0207;
    const WM_MBUTTONUP: u32 = 0x0208;
    const WM_DPICHANGED: u32 = 0x02E0;
    const SIZE_MINIMIZED: usize = 1;

    /// What the window procedure and the program share.
    struct State {
        events: VecDeque<Event>,
        width: i64,
        height: i64,
        dpi: u32,
        minimized: bool,
        /// The last frame shown, to paint again: blue, green, red, unused.
        frame: Vec<u8>,
        frame_width: usize,
        frame_height: usize,
    }

    pub struct Window {
        handle: Handle,
        state: *mut State,
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn class_name() -> Vec<u16> {
        wide("SayonWindow")
    }

    /// Registers the window class, and makes the process aware of each
    /// monitor's density, once.
    fn register() -> bool {
        static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *REGISTERED.get_or_init(|| {
            let name = class_name();
            // SAFETY: plain system calls with valid arguments.
            unsafe {
                SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2);
                let class = Class {
                    size: std::mem::size_of::<Class>() as u32,
                    style: 0,
                    procedure,
                    class_extra: 0,
                    window_extra: 0,
                    instance: GetModuleHandleW(std::ptr::null()),
                    icon: std::ptr::null_mut(),
                    cursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW as *const u16),
                    background: std::ptr::null_mut(),
                    menu: std::ptr::null(),
                    name: name.as_ptr(),
                    small_icon: std::ptr::null_mut(),
                };
                RegisterClassExW(&class) != 0
            }
        })
    }

    /// The signed coordinates a mouse message carries.
    fn point(lparam: isize) -> (i64, i64) {
        (
            (lparam & 0xFFFF) as i16 as i64,
            ((lparam >> 16) & 0xFFFF) as i16 as i64,
        )
    }

    fn key(code: usize) -> &'static str {
        const LETTERS: [&str; 26] = [
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q",
            "r", "s", "t", "u", "v", "w", "x", "y", "z",
        ];
        const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
        const FUNCTIONS: [&str; 12] = [
            "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
        ];
        match code {
            0x08 => "Backspace",
            0x09 => "Tab",
            0x0D => "Enter",
            0x10 => "Shift",
            0x11 => "Control",
            0x12 => "Alt",
            0x1B => "Escape",
            0x20 => " ",
            0x21 => "PageUp",
            0x22 => "PageDown",
            0x23 => "End",
            0x24 => "Home",
            0x25 => "ArrowLeft",
            0x26 => "ArrowUp",
            0x27 => "ArrowRight",
            0x28 => "ArrowDown",
            0x2D => "Insert",
            0x2E => "Delete",
            0x30..=0x39 => DIGITS[code - 0x30],
            0x41..=0x5A => LETTERS[code - 0x41],
            0x70..=0x7B => FUNCTIONS[code - 0x70],
            _ => "Unidentified",
        }
    }

    unsafe extern "system" fn procedure(
        window: Handle,
        message: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        // SAFETY: the state is the window's, set when it is made, and only
        // touched here and between messages.
        unsafe {
            if message == WM_NCCREATE {
                // The state comes as the creation's parameter.
                let create = lparam as *const *mut c_void;
                SetWindowLongPtrW(window, GWLP_USERDATA, *create as isize);
                return DefWindowProcW(window, message, wparam, lparam);
            }
            let state = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut State;
            if state.is_null() {
                return DefWindowProcW(window, message, wparam, lparam);
            }
            let state = &mut *state;
            match message {
                WM_CLOSE => state.events.push_back(Event::Closed),
                WM_SIZE => {
                    if wparam == SIZE_MINIMIZED {
                        state.minimized = true;
                        state.events.push_back(Event::Paused);
                    } else {
                        if std::mem::take(&mut state.minimized) {
                            state.events.push_back(Event::Resumed);
                        }
                        let (width, height) =
                            ((lparam & 0xFFFF) as i64, ((lparam >> 16) & 0xFFFF) as i64);
                        if (width, height) != (state.width, state.height) && width > 0 && height > 0
                        {
                            state.width = width;
                            state.height = height;
                            state.events.push_back(Event::Resized(width, height));
                        }
                    }
                }
                WM_DPICHANGED => {
                    state.dpi = (wparam & 0xFFFF) as u32;
                    let rect = &*(lparam as *const Rect);
                    SetWindowPos(
                        window,
                        std::ptr::null_mut(),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                WM_PAINT => {
                    let mut paint: Paint = std::mem::zeroed();
                    let dc = BeginPaint(window, &mut paint);
                    show(dc, state);
                    EndPaint(window, &paint);
                }
                WM_ERASEBKGND => return 1,
                WM_SETFOCUS => state.events.push_back(Event::Focused(true)),
                WM_KILLFOCUS => state.events.push_back(Event::Focused(false)),
                WM_KEYDOWN | WM_SYSKEYDOWN => state.events.push_back(Event::KeyDown(key(wparam))),
                WM_KEYUP | WM_SYSKEYUP => state.events.push_back(Event::KeyUp(key(wparam))),
                WM_MOUSEMOVE => {
                    let (x, y) = point(lparam);
                    state.events.push_back(Event::PointerMoved(x, y));
                }
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                    let (x, y) = point(lparam);
                    let button = match message {
                        WM_LBUTTONDOWN => 1,
                        WM_RBUTTONDOWN => 2,
                        _ => 3,
                    };
                    SetCapture(window);
                    state.events.push_back(Event::PointerDown(x, y, button));
                }
                WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
                    let (x, y) = point(lparam);
                    let button = match message {
                        WM_LBUTTONUP => 1,
                        WM_RBUTTONUP => 2,
                        _ => 3,
                    };
                    ReleaseCapture();
                    state.events.push_back(Event::PointerUp(x, y, button));
                }
                // Keys such as Alt and F10 go on to the system, which they
                // also mean something to.
                _ => return DefWindowProcW(window, message, wparam, lparam),
            }
            if matches!(message, WM_SYSKEYDOWN | WM_SYSKEYUP) {
                return DefWindowProcW(window, message, wparam, lparam);
            }
            0
        }
    }

    /// Puts the last frame on the window's device context.
    unsafe fn show(dc: Handle, state: &State) {
        if state.frame.is_empty() {
            return;
        }
        let info = BitmapInfo {
            size: 40,
            width: state.frame_width as i32,
            // From the top down.
            height: -(state.frame_height as i32),
            planes: 1,
            bits: 32,
            compression: 0,
            image_size: 0,
            x_density: 0,
            y_density: 0,
            used: 0,
            important: 0,
            colors: [0],
        };
        let (width, height) = (state.frame_width as i32, state.frame_height as i32);
        // SAFETY: the frame holds `width` by `height` pixels of 4 bytes.
        unsafe {
            StretchDIBits(
                dc,
                0,
                0,
                width,
                height,
                0,
                0,
                width,
                height,
                state.frame.as_ptr().cast(),
                &info,
                0,
                SRCCOPY,
            )
        };
    }

    impl Window {
        pub fn open(title: &str, width: i64, height: i64) -> Result<Window, HostError> {
            if !register() {
                return Err(no_screen("Windows refused the window's class"));
            }
            let state = Box::into_raw(Box::new(State {
                events: VecDeque::new(),
                width: 0,
                height: 0,
                dpi: 96,
                minimized: false,
                frame: Vec::new(),
                frame_width: 0,
                frame_height: 0,
            }));
            let class = class_name();
            let title = wide(title);
            // SAFETY: the class is registered; the state outlives the window.
            unsafe {
                let dpi = GetDpiForSystem().max(96);
                let mut rect = Rect {
                    left: 0,
                    top: 0,
                    right: (width * dpi as i64 / 96) as i32,
                    bottom: (height * dpi as i64 / 96) as i32,
                };
                AdjustWindowRectExForDpi(&mut rect, WS_OVERLAPPEDWINDOW, 0, 0, dpi);
                let handle = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    title.as_ptr(),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    GetModuleHandleW(std::ptr::null()),
                    state.cast(),
                );
                if handle.is_null() {
                    drop(Box::from_raw(state));
                    return Err(no_screen(&format!(
                        "Windows made no window ({})",
                        std::io::Error::last_os_error()
                    )));
                }
                (*state).dpi = GetDpiForWindow(handle).max(96);
                let mut client = Rect::default();
                GetClientRect(handle, &mut client);
                let (width, height) = (client.right as i64, client.bottom as i64);
                // The size it opened with, even if no message told it yet.
                (*state)
                    .events
                    .retain(|event| !matches!(event, Event::Resized(..)));
                (*state).width = width;
                (*state).height = height;
                (*state).events.push_front(Event::Resized(width, height));
                Ok(Window { handle, state })
            }
        }

        /// Handles the messages waiting for this thread's windows.
        fn pump(&mut self) {
            // SAFETY: a message structure for the system to fill.
            unsafe {
                let mut message: Message = std::mem::zeroed();
                while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }

        pub fn events(&mut self) -> Vec<Event> {
            self.pump();
            // SAFETY: no message is being handled now.
            unsafe { (*self.state).events.drain(..).collect() }
        }

        pub fn present(&mut self, canvas: &Canvas) -> Result<(), HostError> {
            self.pump();
            // SAFETY: no message is being handled now; the device context is
            // released after drawing.
            unsafe {
                let state = &mut *self.state;
                let (width, height) = (state.width.max(1) as usize, state.height.max(1) as usize);
                state.frame = frame(canvas, width, height, [2, 1, 0, 3]);
                state.frame_width = width;
                state.frame_height = height;
                let dc = GetDC(self.handle);
                show(dc, state);
                ReleaseDC(self.handle, dc);
            }
            Ok(())
        }

        pub fn size(&self) -> (i64, i64) {
            // SAFETY: no message is being handled now.
            unsafe { ((*self.state).width, (*self.state).height) }
        }

        pub fn scale(&self) -> f64 {
            // SAFETY: no message is being handled now.
            unsafe { (*self.state).dpi as f64 / 96.0 }
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            // SAFETY: the window and its state, destroyed once.
            unsafe {
                SetWindowLongPtrW(self.handle, GWLP_USERDATA, 0);
                DestroyWindow(self.handle);
                drop(Box::from_raw(self.state));
            }
        }
    }
}

/// Linux: a window of the X server (or XWayland), through Xlib, which is
/// opened only when a program opens a window, so that a program that shows
/// none does not need it. Frames go to the window as images of the
/// runtime's own memory.
#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::ffi::{CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
    use std::sync::OnceLock;

    type Display = *mut c_void;
    type Xid = c_ulong;

    /// Xlib's `XImage`, which `XInitImage` completes.
    #[repr(C)]
    struct Image {
        width: c_int,
        height: c_int,
        xoffset: c_int,
        format: c_int,
        data: *mut c_char,
        byte_order: c_int,
        bitmap_unit: c_int,
        bitmap_bit_order: c_int,
        bitmap_pad: c_int,
        depth: c_int,
        bytes_per_line: c_int,
        bits_per_pixel: c_int,
        red_mask: c_ulong,
        green_mask: c_ulong,
        blue_mask: c_ulong,
        object_data: *mut c_char,
        functions: [*mut c_void; 6],
    }

    /// The functions of Xlib the window uses.
    struct Xlib {
        open_display: unsafe extern "C" fn(*const c_char) -> Display,
        close_display: unsafe extern "C" fn(Display) -> c_int,
        default_screen: unsafe extern "C" fn(Display) -> c_int,
        root_window: unsafe extern "C" fn(Display, c_int) -> Xid,
        black_pixel: unsafe extern "C" fn(Display, c_int) -> c_ulong,
        default_depth: unsafe extern "C" fn(Display, c_int) -> c_int,
        default_gc: unsafe extern "C" fn(Display, c_int) -> *mut c_void,
        create_simple_window: unsafe extern "C" fn(
            Display,
            Xid,
            c_int,
            c_int,
            c_uint,
            c_uint,
            c_uint,
            c_ulong,
            c_ulong,
        ) -> Xid,
        store_name: unsafe extern "C" fn(Display, Xid, *const c_char) -> c_int,
        select_input: unsafe extern "C" fn(Display, Xid, c_long) -> c_int,
        intern_atom: unsafe extern "C" fn(Display, *const c_char, c_int) -> c_ulong,
        set_protocols: unsafe extern "C" fn(Display, Xid, *mut c_ulong, c_int) -> c_int,
        map_window: unsafe extern "C" fn(Display, Xid) -> c_int,
        destroy_window: unsafe extern "C" fn(Display, Xid) -> c_int,
        flush: unsafe extern "C" fn(Display) -> c_int,
        pending: unsafe extern "C" fn(Display) -> c_int,
        next_event: unsafe extern "C" fn(Display, *mut XEvent) -> c_int,
        lookup_keysym: unsafe extern "C" fn(*mut XEvent, c_int) -> c_ulong,
        init_image: unsafe extern "C" fn(*mut Image) -> c_int,
        put_image: unsafe extern "C" fn(
            Display,
            Xid,
            *mut c_void,
            *mut Image,
            c_int,
            c_int,
            c_int,
            c_int,
            c_uint,
            c_uint,
        ) -> c_int,
    }

    // SAFETY: the table holds addresses of functions.
    unsafe impl Send for Xlib {}
    unsafe impl Sync for Xlib {}

    /// Xlib's `XEvent`, a union of 24 longs, read by offset.
    #[repr(C)]
    struct XEvent([c_long; 24]);

    impl XEvent {
        fn int(&self, offset: usize) -> i64 {
            // SAFETY: within the event, at an int's place.
            unsafe { *((self.0.as_ptr() as *const u8).add(offset) as *const c_int) as i64 }
        }

        fn long(&self, offset: usize) -> u64 {
            self.0[offset / 8] as u64
        }
    }

    const RTLD_NOW: c_int = 2;
    const KEY_PRESS: i64 = 2;
    const KEY_RELEASE: i64 = 3;
    const BUTTON_PRESS: i64 = 4;
    const BUTTON_RELEASE: i64 = 5;
    const MOTION_NOTIFY: i64 = 6;
    const FOCUS_IN: i64 = 9;
    const FOCUS_OUT: i64 = 10;
    const EXPOSE: i64 = 12;
    const UNMAP_NOTIFY: i64 = 18;
    const MAP_NOTIFY: i64 = 19;
    const CONFIGURE_NOTIFY: i64 = 22;
    const CLIENT_MESSAGE: i64 = 33;
    /// Key presses and releases, buttons, motion, exposure, structure, and
    /// focus.
    const EVENTS: c_long = 1 | 2 | 4 | 8 | 64 | 32768 | 131072 | 2097152;
    const Z_PIXMAP: c_int = 2;

    unsafe extern "C" {
        fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    // Each symbol becomes the function type of the field it fills.
    #[allow(clippy::missing_transmute_annotations)]
    fn xlib() -> Result<&'static Xlib, HostError> {
        static XLIB: OnceLock<Option<Xlib>> = OnceLock::new();
        XLIB.get_or_init(|| {
            // SAFETY: NUL-terminated names; each symbol, when found, has the
            // type of Xlib's function of that name.
            unsafe {
                let library = ["libX11.so.6", "libX11.so"].iter().find_map(|name| {
                    let name = CString::new(*name).ok()?;
                    let handle = dlopen(name.as_ptr(), RTLD_NOW);
                    (!handle.is_null()).then_some(handle)
                })?;
                let symbol = |name: &str| {
                    let name = CString::new(name).ok()?;
                    let address = dlsym(library, name.as_ptr());
                    (!address.is_null()).then_some(address)
                };
                macro_rules! function {
                    ($name:literal) => {
                        std::mem::transmute::<*mut c_void, _>(symbol($name)?)
                    };
                }
                Some(Xlib {
                    open_display: function!("XOpenDisplay"),
                    close_display: function!("XCloseDisplay"),
                    default_screen: function!("XDefaultScreen"),
                    root_window: function!("XRootWindow"),
                    black_pixel: function!("XBlackPixel"),
                    default_depth: function!("XDefaultDepth"),
                    default_gc: function!("XDefaultGC"),
                    create_simple_window: function!("XCreateSimpleWindow"),
                    store_name: function!("XStoreName"),
                    select_input: function!("XSelectInput"),
                    intern_atom: function!("XInternAtom"),
                    set_protocols: function!("XSetWMProtocols"),
                    map_window: function!("XMapWindow"),
                    destroy_window: function!("XDestroyWindow"),
                    flush: function!("XFlush"),
                    pending: function!("XPending"),
                    next_event: function!("XNextEvent"),
                    lookup_keysym: function!("XLookupKeysym"),
                    init_image: function!("XInitImage"),
                    put_image: function!("XPutImage"),
                })
            }
        })
        .as_ref()
        .ok_or_else(|| no_screen("Xlib (libX11.so.6) is not installed"))
    }

    fn key(symbol: u64) -> &'static str {
        const LETTERS: [&str; 26] = [
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q",
            "r", "s", "t", "u", "v", "w", "x", "y", "z",
        ];
        const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
        const FUNCTIONS: [&str; 12] = [
            "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
        ];
        match symbol {
            0x20 => " ",
            0x30..=0x39 => DIGITS[(symbol - 0x30) as usize],
            0x61..=0x7A => LETTERS[(symbol - 0x61) as usize],
            0x41..=0x5A => LETTERS[(symbol - 0x41) as usize],
            0xFF08 => "Backspace",
            0xFF09 => "Tab",
            0xFF0D => "Enter",
            0xFF1B => "Escape",
            0xFF50 => "Home",
            0xFF51 => "ArrowLeft",
            0xFF52 => "ArrowUp",
            0xFF53 => "ArrowRight",
            0xFF54 => "ArrowDown",
            0xFF55 => "PageUp",
            0xFF56 => "PageDown",
            0xFF57 => "End",
            0xFF63 => "Insert",
            0xFFFF => "Delete",
            0xFFE1 | 0xFFE2 => "Shift",
            0xFFE3 | 0xFFE4 => "Control",
            0xFFE9 | 0xFFEA => "Alt",
            0xFFBE..=0xFFC9 => FUNCTIONS[(symbol - 0xFFBE) as usize],
            _ => "Unidentified",
        }
    }

    pub struct Window {
        xlib: &'static Xlib,
        display: Display,
        window: Xid,
        screen: c_int,
        close_atom: c_ulong,
        width: i64,
        height: i64,
        mapped: bool,
        unmapped: bool,
        /// Whether the size it opened with has been told.
        announced: bool,
        /// The last frame shown, to draw again: blue, green, red, unused.
        frame: Vec<u8>,
        frame_size: (usize, usize),
    }

    impl Window {
        pub fn open(title: &str, width: i64, height: i64) -> Result<Window, HostError> {
            let xlib = xlib()?;
            let name = CString::new(title.replace('\0', " ")).expect("NUL bytes replaced");
            // SAFETY: Xlib's functions with a display it opened.
            unsafe {
                let display = (xlib.open_display)(std::ptr::null());
                if display.is_null() {
                    return Err(no_screen("no X display answered (is DISPLAY set?)"));
                }
                let screen = (xlib.default_screen)(display);
                if (xlib.default_depth)(display, screen) != 24 {
                    (xlib.close_display)(display);
                    return Err(no_screen("the X display's colors are not 24 bits"));
                }
                let black = (xlib.black_pixel)(display, screen);
                let root = (xlib.root_window)(display, screen);
                let window = (xlib.create_simple_window)(
                    display,
                    root,
                    0,
                    0,
                    width as c_uint,
                    height as c_uint,
                    0,
                    black,
                    black,
                );
                (xlib.store_name)(display, window, name.as_ptr());
                (xlib.select_input)(display, window, EVENTS);
                let atom_name = CString::new("WM_DELETE_WINDOW").expect("a name");
                let mut close_atom = (xlib.intern_atom)(display, atom_name.as_ptr(), 0);
                (xlib.set_protocols)(display, window, &mut close_atom, 1);
                (xlib.map_window)(display, window);
                (xlib.flush)(display);
                Ok(Window {
                    xlib,
                    display,
                    window,
                    screen,
                    close_atom,
                    width,
                    height,
                    mapped: false,
                    unmapped: false,
                    announced: false,
                    frame: Vec::new(),
                    frame_size: (0, 0),
                })
            }
        }

        /// Puts the last frame on the window.
        fn show(&mut self) {
            if self.frame.is_empty() || !self.mapped {
                return;
            }
            let (width, height) = self.frame_size;
            let mut image = Image {
                width: width as c_int,
                height: height as c_int,
                xoffset: 0,
                format: Z_PIXMAP,
                data: self.frame.as_mut_ptr().cast(),
                byte_order: 0,
                bitmap_unit: 32,
                bitmap_bit_order: 0,
                bitmap_pad: 32,
                depth: 24,
                bytes_per_line: (width * 4) as c_int,
                bits_per_pixel: 32,
                red_mask: 0xFF_0000,
                green_mask: 0xFF00,
                blue_mask: 0xFF,
                object_data: std::ptr::null_mut(),
                functions: [std::ptr::null_mut(); 6],
            };
            // SAFETY: the image describes the frame's memory, which lives
            // through the call.
            unsafe {
                if (self.xlib.init_image)(&mut image) == 0 {
                    return;
                }
                let gc = (self.xlib.default_gc)(self.display, self.screen);
                (self.xlib.put_image)(
                    self.display,
                    self.window,
                    gc,
                    &mut image,
                    0,
                    0,
                    0,
                    0,
                    width as c_uint,
                    height as c_uint,
                );
                (self.xlib.flush)(self.display);
            }
        }

        pub fn events(&mut self) -> Vec<Event> {
            let mut events = Vec::new();
            if !std::mem::replace(&mut self.announced, true) {
                // The size it opened with comes first.
                events.push(Event::Resized(self.width, self.height));
            }
            let mut expose = false;
            // SAFETY: events of the window's own display.
            unsafe {
                while (self.xlib.pending)(self.display) > 0 {
                    let mut event = XEvent([0; 24]);
                    (self.xlib.next_event)(self.display, &mut event);
                    match event.int(0) {
                        KEY_PRESS => events.push(Event::KeyDown(key((self.xlib.lookup_keysym)(
                            &mut event, 0,
                        )))),
                        KEY_RELEASE => {
                            events.push(Event::KeyUp(key((self.xlib.lookup_keysym)(&mut event, 0))))
                        }
                        BUTTON_PRESS | BUTTON_RELEASE => {
                            let button = match event.int(84) {
                                1 => 1,
                                3 => 2,
                                2 => 3,
                                // The wheel, which is not a button here.
                                _ => continue,
                            };
                            let (x, y) = (event.int(64), event.int(68));
                            events.push(if event.int(0) == BUTTON_PRESS {
                                Event::PointerDown(x, y, button)
                            } else {
                                Event::PointerUp(x, y, button)
                            });
                        }
                        MOTION_NOTIFY => {
                            events.push(Event::PointerMoved(event.int(64), event.int(68)))
                        }
                        FOCUS_IN => events.push(Event::Focused(true)),
                        FOCUS_OUT => events.push(Event::Focused(false)),
                        EXPOSE => expose = true,
                        MAP_NOTIFY => {
                            self.mapped = true;
                            expose = true;
                            if std::mem::take(&mut self.unmapped) {
                                events.push(Event::Resumed);
                            }
                        }
                        UNMAP_NOTIFY => {
                            self.mapped = false;
                            self.unmapped = true;
                            events.push(Event::Paused);
                        }
                        CONFIGURE_NOTIFY => {
                            let (width, height) = (event.int(56), event.int(60));
                            if (width, height) != (self.width, self.height) {
                                self.width = width;
                                self.height = height;
                                events.push(Event::Resized(width, height));
                            }
                        }
                        CLIENT_MESSAGE if event.long(56) == self.close_atom => {
                            events.push(Event::Closed)
                        }
                        _ => {}
                    }
                }
            }
            if expose {
                self.show();
            }
            events
        }

        pub fn present(&mut self, canvas: &Canvas) -> Result<(), HostError> {
            let (width, height) = (self.width.max(1) as usize, self.height.max(1) as usize);
            self.frame = frame(canvas, width, height, [2, 1, 0, 3]);
            self.frame_size = (width, height);
            self.show();
            Ok(())
        }

        pub fn size(&self) -> (i64, i64) {
            (self.width, self.height)
        }

        pub fn scale(&self) -> f64 {
            1.0
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            // SAFETY: the window and the display, closed once.
            unsafe {
                (self.xlib.destroy_window)(self.display, self.window);
                (self.xlib.close_display)(self.display);
            }
        }
    }
}

/// The name of a key, among those of [`Event`]; "Unidentified" for others.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn known_key(name: &str) -> &'static str {
    const KEYS: &[&str] = &[
        "a",
        "b",
        "c",
        "d",
        "e",
        "f",
        "g",
        "h",
        "i",
        "j",
        "k",
        "l",
        "m",
        "n",
        "o",
        "p",
        "q",
        "r",
        "s",
        "t",
        "u",
        "v",
        "w",
        "x",
        "y",
        "z",
        "0",
        "1",
        "2",
        "3",
        "4",
        "5",
        "6",
        "7",
        "8",
        "9",
        " ",
        "Enter",
        "Escape",
        "Backspace",
        "Tab",
        "ArrowLeft",
        "ArrowRight",
        "ArrowUp",
        "ArrowDown",
        "Shift",
        "Control",
        "Alt",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "Insert",
        "Delete",
        "F1",
        "F2",
        "F3",
        "F4",
        "F5",
        "F6",
        "F7",
        "F8",
        "F9",
        "F10",
        "F11",
        "F12",
    ];
    KEYS.iter()
        .copied()
        .find(|key| *key == name)
        .unwrap_or("Unidentified")
}

/// A web page: a canvas of the page, which the page's glue adds and
/// whose events it gathers.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod platform {
    use super::*;

    #[link(wasm_import_module = "sayon")]
    unsafe extern "C" {
        /// Adds a canvas to the page, `width` by `height` CSS pixels, and
        /// gives its number; 0 where there is no page.
        fn host_window_open(title: *const u8, title_length: usize, width: f64, height: f64) -> i32;
        /// Takes the events since the last call: each field ended by a
        /// line break. Gives the length of the text, which
        /// `host_window_take` copies.
        fn host_window_events(window: i32) -> usize;
        fn host_window_take(window: i32, into: *mut u8);
        /// Draws `width` by `height` pixels, red, green, blue, and alpha.
        fn host_window_present(window: i32, pixels: *const u8, width: usize, height: usize);
        /// The canvas's width (0) or height (1) in pixels, or the screen's
        /// density (2).
        fn host_window_size(window: i32, which: i32) -> f64;
        fn host_window_close(window: i32);
    }

    pub struct Window {
        number: i32,
    }

    fn integer(field: Option<&str>) -> i64 {
        field.and_then(|text| text.parse().ok()).unwrap_or(0)
    }

    impl Window {
        pub fn open(title: &str, width: i64, height: i64) -> Result<Window, HostError> {
            // SAFETY: the title is readable for its length.
            let number = unsafe {
                host_window_open(title.as_ptr(), title.len(), width as f64, height as f64)
            };
            if number == 0 {
                return Err(no_screen("this host has no page to show it in"));
            }
            Ok(Window { number })
        }

        pub fn events(&mut self) -> Vec<Event> {
            // SAFETY: a buffer of the length the glue gave.
            let text = unsafe {
                let mut bytes = vec![0u8; host_window_events(self.number)];
                host_window_take(self.number, bytes.as_mut_ptr());
                String::from_utf8_lossy(&bytes).into_owned()
            };
            let mut fields = text.lines();
            let mut events = Vec::new();
            while let Some(name) = fields.next() {
                let mut next = || fields.next();
                events.push(match name {
                    "closed" => Event::Closed,
                    "resized" => Event::Resized(integer(next()), integer(next())),
                    "key_down" => Event::KeyDown(known_key(next().unwrap_or(""))),
                    "key_up" => Event::KeyUp(known_key(next().unwrap_or(""))),
                    "pointer_down" => {
                        Event::PointerDown(integer(next()), integer(next()), integer(next()))
                    }
                    "pointer_up" => {
                        Event::PointerUp(integer(next()), integer(next()), integer(next()))
                    }
                    "pointer_moved" => Event::PointerMoved(integer(next()), integer(next())),
                    "focused" => Event::Focused(next() == Some("true")),
                    "paused" => Event::Paused,
                    _ => Event::Resumed,
                });
            }
            events
        }

        pub fn present(&mut self, canvas: &Canvas) -> Result<(), HostError> {
            let (width, height) = self.size();
            let (width, height) = (width.max(1) as usize, height.max(1) as usize);
            let pixels = frame(canvas, width, height, [0, 1, 2, 3]);
            // SAFETY: the pixels are readable for their length.
            unsafe { host_window_present(self.number, pixels.as_ptr(), width, height) };
            Ok(())
        }

        pub fn size(&self) -> (i64, i64) {
            // SAFETY: the glue only looks the window up.
            unsafe {
                (
                    host_window_size(self.number, 0) as i64,
                    host_window_size(self.number, 1) as i64,
                )
            }
        }

        pub fn scale(&self) -> f64 {
            // SAFETY: the glue only looks the window up.
            unsafe { host_window_size(self.number, 2) }
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            // SAFETY: the window is not used again.
            unsafe { host_window_close(self.number) }
        }
    }
}

/// Android: the app's own window, which its activity gives the runtime
/// (see `android`): a program run from `adb shell` has none.
#[cfg(target_os = "android")]
mod platform {
    use super::*;

    pub struct Window;

    impl Window {
        pub fn open(_title: &str, _width: i64, _height: i64) -> Result<Window, HostError> {
            if !crate::android::in_app() {
                return Err(no_screen(
                    "a program run from a shell has no screen; build it as an app",
                ));
            }
            crate::android::screen_open(true);
            Ok(Window)
        }

        pub fn events(&mut self) -> Vec<Event> {
            crate::android::screen_events()
        }

        pub fn present(&mut self, canvas: &Canvas) -> Result<(), HostError> {
            crate::android::screen_present(|width, height| {
                frame(canvas, width, height, [0, 1, 2, 3])
            });
            Ok(())
        }

        pub fn size(&self) -> (i64, i64) {
            crate::android::screen_size()
        }

        pub fn scale(&self) -> f64 {
            crate::android::screen_density()
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            crate::android::screen_open(false);
        }
    }
}
