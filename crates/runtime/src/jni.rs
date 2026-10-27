//! Calls into Java, for what Android gives an app only there, such as TLS
//! (see `tls`). The app's activity gives the runtime its Java VM; each
//! thread that calls into Java attaches to it the first time, and detaches
//! as it ends, which Android requires. A program run from `adb shell` has
//! no Java VM, so none of this is there for it.
//!
//! The JNI's functions are reached by their places in its tables, which
//! the JNI specification fixes. Each call into Java runs in a frame of
//! local references that ends with it, since a native thread never
//! returns to Java to have them released.

use std::cell::Cell;
use std::ffi::{CString, c_char, c_void};
use std::sync::atomic::{AtomicPtr, Ordering};

/// A reference to a Java object, as the JNI gives it.
pub(crate) type Object = *mut c_void;
/// A method's identity.
pub(crate) type Method = *mut c_void;

/// An argument of a call into Java.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union Value {
    pub(crate) z: u8,
    pub(crate) i: i32,
    pub(crate) j: i64,
    pub(crate) l: Object,
}

/// The app's Java VM, once its activity has started.
static VM: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

const VERSION: i32 = 0x0001_0006;
const DETACHED: i32 = -2;

/// Remembers the app's Java VM.
pub(crate) fn set_vm(vm: *mut c_void) {
    VM.store(vm, Ordering::Relaxed);
}

/// The function at `index` of a JNI table.
///
/// # Safety
/// `table` is a JNI table, and `T` the type of its function at `index`.
unsafe fn function<T: Copy>(table: *mut c_void, index: usize) -> T {
    // SAFETY: as the caller says; the table is an array of function
    // addresses after its pointer.
    unsafe {
        let functions = *(table as *const *const *const c_void);
        std::mem::transmute_copy(&*functions.add(index))
    }
}

/// Detaches this thread from the Java VM as it ends, if it attached.
struct Attached(Cell<bool>);

impl Drop for Attached {
    fn drop(&mut self) {
        let vm = VM.load(Ordering::Relaxed);
        if self.0.get() && !vm.is_null() {
            // SAFETY: the VM's `DetachCurrentThread`, for a thread it attached.
            unsafe {
                let detach: unsafe extern "C" fn(*mut c_void) -> i32 = function(vm, 5);
                detach(vm);
            }
        }
    }
}

thread_local! {
    static ATTACHED: Attached = const { Attached(Cell::new(false)) };
}

/// A thread's way into Java: its `JNIEnv`.
#[derive(Clone, Copy)]
pub(crate) struct Env(*mut c_void);

/// A Java exception, as its `toString` and those of its causes say.
pub(crate) struct Thrown(pub(crate) String);

/// The calling thread's way into Java, attaching it the first time; none
/// without a Java VM.
pub(crate) fn env() -> Option<Env> {
    let vm = VM.load(Ordering::Relaxed);
    if vm.is_null() {
        return None;
    }
    // SAFETY: the VM's `GetEnv` and `AttachCurrentThread`.
    unsafe {
        let mut env: *mut c_void = std::ptr::null_mut();
        let get: unsafe extern "C" fn(*mut c_void, *mut *mut c_void, i32) -> i32 = function(vm, 6);
        if get(vm, &mut env, VERSION) == DETACHED {
            let attach: unsafe extern "C" fn(*mut c_void, *mut *mut c_void, *mut c_void) -> i32 =
                function(vm, 4);
            if attach(vm, &mut env, std::ptr::null_mut()) != 0 {
                return None;
            }
            ATTACHED.with(|attached| attached.0.set(true));
        }
        (!env.is_null()).then_some(Env(env))
    }
}

fn name(text: &str) -> CString {
    CString::new(text).expect("a JNI name without NUL")
}

impl Env {
    /// # Safety
    /// `T` is the type of the JNI function at `index`.
    unsafe fn get<T: Copy>(self, index: usize) -> T {
        // SAFETY: as the caller says.
        unsafe { function(self.0, index) }
    }

    /// The exception a call threw, cleared, as text.
    pub(crate) fn thrown(self) -> Option<Thrown> {
        // SAFETY: `ExceptionCheck`, `ExceptionOccurred`, and `ExceptionClear`.
        unsafe {
            let check: unsafe extern "C" fn(*mut c_void) -> u8 = self.get(228);
            if check(self.0) == 0 {
                return None;
            }
            let occurred: unsafe extern "C" fn(*mut c_void) -> Object = self.get(15);
            let clear: unsafe extern "C" fn(*mut c_void) = self.get(17);
            let mut exception = occurred(self.0);
            clear(self.0);
            let mut parts = Vec::new();
            // The exception and its causes, which say more.
            while !exception.is_null() && parts.len() < 8 {
                let class = self.object_class(exception);
                let to_string = self.method(class, "toString", "()Ljava/lang/String;");
                let cause = self.method(class, "getCause", "()Ljava/lang/Throwable;");
                let text = self.call_object(exception, to_string, &[]);
                parts.push(self.string(text));
                let next = self.call_object(exception, cause, &[]);
                clear(self.0);
                if next.is_null() || self.same(next, exception) {
                    break;
                }
                exception = next;
            }
            Some(Thrown(parts.join("; caused by ")))
        }
    }

    /// Fails with the exception a call threw.
    pub(crate) fn check(self) -> Result<(), Thrown> {
        match self.thrown() {
            Some(thrown) => Err(thrown),
            None => Ok(()),
        }
    }

    fn same(self, a: Object, b: Object) -> bool {
        // SAFETY: `IsSameObject`.
        unsafe {
            let same: unsafe extern "C" fn(*mut c_void, Object, Object) -> u8 = self.get(24);
            same(self.0, a, b) != 0
        }
    }

    /// Runs `f` in a frame of local references, which end with it, but for
    /// those it makes global.
    pub(crate) fn frame<T>(self, f: impl FnOnce() -> T) -> T {
        // SAFETY: `PushLocalFrame` and `PopLocalFrame`, paired.
        unsafe {
            let push: unsafe extern "C" fn(*mut c_void, i32) -> i32 = self.get(19);
            let pop: unsafe extern "C" fn(*mut c_void, Object) -> Object = self.get(20);
            push(self.0, 64);
            let result = f();
            pop(self.0, std::ptr::null_mut());
            result
        }
    }

    pub(crate) fn find_class(self, class: &str) -> Object {
        let class = name(class);
        // SAFETY: `FindClass`, with a class's name in JNI's form.
        unsafe {
            let find: unsafe extern "C" fn(*mut c_void, *const c_char) -> Object = self.get(6);
            find(self.0, class.as_ptr())
        }
    }

    fn object_class(self, object: Object) -> Object {
        // SAFETY: `GetObjectClass`.
        unsafe {
            let get: unsafe extern "C" fn(*mut c_void, Object) -> Object = self.get(31);
            get(self.0, object)
        }
    }

    pub(crate) fn method(self, class: Object, method: &str, signature: &str) -> Method {
        let (method, signature) = (name(method), name(signature));
        // SAFETY: `GetMethodID`.
        unsafe {
            let get: unsafe extern "C" fn(
                *mut c_void,
                Object,
                *const c_char,
                *const c_char,
            ) -> Method = self.get(33);
            get(self.0, class, method.as_ptr(), signature.as_ptr())
        }
    }

    pub(crate) fn static_method(self, class: Object, method: &str, signature: &str) -> Method {
        let (method, signature) = (name(method), name(signature));
        // SAFETY: `GetStaticMethodID`.
        unsafe {
            let get: unsafe extern "C" fn(
                *mut c_void,
                Object,
                *const c_char,
                *const c_char,
            ) -> Method = self.get(113);
            get(self.0, class, method.as_ptr(), signature.as_ptr())
        }
    }

    pub(crate) fn call_object(self, object: Object, method: Method, args: &[Value]) -> Object {
        // SAFETY: `CallObjectMethodA`, with the method's arguments.
        unsafe {
            let call: unsafe extern "C" fn(*mut c_void, Object, Method, *const Value) -> Object =
                self.get(36);
            call(self.0, object, method, args.as_ptr())
        }
    }

    pub(crate) fn call_boolean(self, object: Object, method: Method, args: &[Value]) -> bool {
        // SAFETY: `CallBooleanMethodA`.
        unsafe {
            let call: unsafe extern "C" fn(*mut c_void, Object, Method, *const Value) -> u8 =
                self.get(39);
            call(self.0, object, method, args.as_ptr()) != 0
        }
    }

    pub(crate) fn call_int(self, object: Object, method: Method, args: &[Value]) -> i32 {
        // SAFETY: `CallIntMethodA`.
        unsafe {
            let call: unsafe extern "C" fn(*mut c_void, Object, Method, *const Value) -> i32 =
                self.get(51);
            call(self.0, object, method, args.as_ptr())
        }
    }

    pub(crate) fn call_void(self, object: Object, method: Method, args: &[Value]) {
        // SAFETY: `CallVoidMethodA`.
        unsafe {
            let call: unsafe extern "C" fn(*mut c_void, Object, Method, *const Value) =
                self.get(63);
            call(self.0, object, method, args.as_ptr())
        }
    }

    pub(crate) fn call_static_object(
        self,
        class: Object,
        method: Method,
        args: &[Value],
    ) -> Object {
        // SAFETY: `CallStaticObjectMethodA`.
        unsafe {
            let call: unsafe extern "C" fn(*mut c_void, Object, Method, *const Value) -> Object =
                self.get(116);
            call(self.0, class, method, args.as_ptr())
        }
    }

    pub(crate) fn new_string(self, text: &str) -> Object {
        let text = name(&text.replace('\0', " "));
        // SAFETY: `NewStringUTF`, with text of no NUL; ASCII and the rest of
        // UTF-8 below U+10000 are the same in Java's modified form.
        unsafe {
            let new: unsafe extern "C" fn(*mut c_void, *const c_char) -> Object = self.get(167);
            new(self.0, text.as_ptr())
        }
    }

    /// A Java string's text.
    pub(crate) fn string(self, string: Object) -> String {
        if string.is_null() {
            return String::new();
        }
        // SAFETY: `GetStringUTFChars` and `ReleaseStringUTFChars`, paired.
        unsafe {
            let get: unsafe extern "C" fn(*mut c_void, Object, *mut u8) -> *const c_char =
                self.get(169);
            let release: unsafe extern "C" fn(*mut c_void, Object, *const c_char) = self.get(170);
            let chars = get(self.0, string, std::ptr::null_mut());
            if chars.is_null() {
                return String::new();
            }
            let text = std::ffi::CStr::from_ptr(chars)
                .to_string_lossy()
                .into_owned();
            release(self.0, string, chars);
            text
        }
    }

    /// A reference that outlives the frame, until `drop_global`.
    pub(crate) fn global(self, object: Object) -> Object {
        // SAFETY: `NewGlobalRef`.
        unsafe {
            let new: unsafe extern "C" fn(*mut c_void, Object) -> Object = self.get(21);
            new(self.0, object)
        }
    }

    pub(crate) fn drop_global(self, object: Object) {
        if object.is_null() {
            return;
        }
        // SAFETY: `DeleteGlobalRef`, of a reference `global` made.
        unsafe {
            let delete: unsafe extern "C" fn(*mut c_void, Object) = self.get(22);
            delete(self.0, object)
        }
    }

    /// Where a direct byte buffer's bytes are.
    pub(crate) fn buffer_address(self, buffer: Object) -> *mut u8 {
        // SAFETY: `GetDirectBufferAddress`.
        unsafe {
            let get: unsafe extern "C" fn(*mut c_void, Object) -> *mut u8 = self.get(230);
            get(self.0, buffer)
        }
    }
}
