//! Functions called by compiled Sayon programs.
//!
//! Conventions:
//! - Strings are `SayonStr` values; `cap == 0` marks static or empty text
//!   that is never freed.
//! - Arrays are `SayonArray` values whose elements the generated code reads
//!   and writes directly; this module only manages their storage.
//! - Dictionaries and errors are heap objects referred to by pointer. An
//!   error slot is a `*mut *mut SayonError`; a null pointer means empty.
//! - Fallible functions return 0 on success and 1 after storing an error.
//! - Integer type codes are the indexes of `text::INTEGER_TYPE_NAMES`.
//!
//! Every function here is called only by generated code that upholds these
//! conventions, so the individual functions do not repeat them.

#![allow(clippy::missing_safety_doc)]

use crate::text;
use std::alloc::{Layout, alloc, dealloc, realloc};
use std::ffi::{CStr, c_char};
use std::io::Write;
use std::sync::atomic::{AtomicI64, Ordering};

/// Heap objects the runtime has created and not yet released: owned string
/// data, array storage, dictionaries, errors, text builders, and closure
/// environments. With `SAYON_DEBUG_ALLOC` set, a program reports the count
/// when it exits; a correct program reports zero.
static LIVE: AtomicI64 = AtomicI64::new(0);

fn allocated() {
    LIVE.fetch_add(1, Ordering::Relaxed);
}

fn released() {
    LIVE.fetch_sub(1, Ordering::Relaxed);
}

#[repr(C)]
pub struct SayonStr {
    pub ptr: *const u8,
    pub len: i64,
    pub cap: i64,
}

#[repr(C)]
pub struct SayonArray {
    pub ptr: *mut u8,
    pub len: i64,
    pub cap: i64,
}

#[repr(C)]
pub struct SayonRange {
    pub start: i64,
    pub end: i64,
    pub step: i64,
}

pub struct SayonError {
    pub code: String,
    pub message: String,
    pub location: Option<String>,
}

/// An insertion-ordered dictionary: keys and values in insertion order, and
/// an open-addressing table of positions found by the keys' hashes.
pub struct SayonDict {
    ops: *const KeyOps,
    /// Key storage in 8-byte words, `key_stride` words per key.
    keys: Vec<u64>,
    key_stride: usize,
    hashes: Vec<u64>,
    /// Positions plus one, or 0 for a free slot; the length is a power of two.
    table: Vec<u32>,
    /// Value storage in 8-byte words, `stride` words per value.
    values: Vec<u64>,
    stride: usize,
    value_size: usize,
}

pub struct Builder(String);

type DropFn = Option<unsafe extern "C" fn(*mut u8)>;
type CloneFn = Option<unsafe extern "C" fn(*const u8, *mut u8)>;
type EqFn = unsafe extern "C" fn(*const u8, *const u8) -> bool;

const ELEMENT_ALIGN: usize = 8;

// ----- helpers ---------------------------------------------------------------

unsafe fn as_str<'a>(s: *const SayonStr) -> &'a str {
    // SAFETY: callers pass a valid SayonStr holding UTF-8 text.
    unsafe {
        let s = &*s;
        if s.len == 0 {
            return "";
        }
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(s.ptr, s.len as usize))
    }
}

unsafe fn c_str<'a>(text: *const c_char) -> &'a str {
    if text.is_null() {
        return "";
    }
    // SAFETY: the generated code passes NUL-terminated UTF-8 constants.
    unsafe { CStr::from_ptr(text).to_str().unwrap_or("") }
}

fn owned(text: String) -> SayonStr {
    if text.is_empty() {
        return SayonStr {
            ptr: std::ptr::null(),
            len: 0,
            cap: 0,
        };
    }
    let bytes = text.into_bytes().into_boxed_slice();
    let len = bytes.len() as i64;
    let ptr = Box::into_raw(bytes) as *const u8;
    allocated();
    SayonStr { ptr, len, cap: len }
}

/// One-byte strings for every ASCII character, so that most characters
/// taken from a string need no allocation.
static ASCII: [u8; 128] = {
    let mut table = [0u8; 128];
    let mut index = 0;
    while index < 128 {
        table[index] = index as u8;
        index += 1;
    }
    table
};

/// A string holding one character.
fn character(ch: char) -> SayonStr {
    if ch.is_ascii() {
        SayonStr {
            ptr: &ASCII[ch as usize],
            len: 1,
            cap: 0,
        }
    } else {
        owned(ch.to_string())
    }
}

unsafe fn set_error(
    slot: *mut *mut SayonError,
    code: &str,
    message: String,
    location: *const c_char,
) -> i32 {
    // SAFETY: `slot` points to the caller's error slot.
    unsafe {
        if !(*slot).is_null() {
            drop(Box::from_raw(*slot));
            released();
        }
        let location = if location.is_null() {
            None
        } else {
            Some(c_str(location).to_string())
        };
        allocated();
        *slot = Box::into_raw(Box::new(SayonError {
            code: code.to_string(),
            message,
            location,
        }));
    }
    1
}

fn type_name(code: u32) -> &'static str {
    text::INTEGER_TYPE_NAMES
        .get(code as usize)
        .copied()
        .unwrap_or("integer")
}

fn widen(value: i64, code: u32) -> i128 {
    if code >= 4 {
        value as u64 as i128
    } else {
        value as i128
    }
}

// ----- strings ---------------------------------------------------------------------

/// Releases an owned string and leaves it empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_str_drop(s: *mut SayonStr) {
    // SAFETY: `s` is a valid string; owned data came from `owned`.
    unsafe {
        let value = &mut *s;
        if value.cap > 0 {
            let slice =
                std::ptr::slice_from_raw_parts_mut(value.ptr as *mut u8, value.cap as usize);
            drop(Box::from_raw(slice));
            released();
        }
        *value = SayonStr {
            ptr: std::ptr::null(),
            len: 0,
            cap: 0,
        };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_str_clone(src: *const SayonStr, dst: *mut SayonStr) {
    // SAFETY: both pointers are valid; static text can be shared.
    unsafe {
        let source = &*src;
        *dst = if source.cap == 0 {
            SayonStr {
                ptr: source.ptr,
                len: source.len,
                cap: 0,
            }
        } else {
            owned(as_str(src).to_string())
        };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_str_concat(
    a: *const SayonStr,
    b: *const SayonStr,
    out: *mut SayonStr,
) {
    // SAFETY: valid strings in, `out` is writable.
    unsafe {
        let (a, b) = (as_str(a), as_str(b));
        let mut text = String::with_capacity(a.len() + b.len());
        text.push_str(a);
        text.push_str(b);
        *out = owned(text);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_str_eq(a: *const SayonStr, b: *const SayonStr) -> bool {
    // SAFETY: valid strings.
    unsafe { as_str(a) == as_str(b) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_str_cmp(a: *const SayonStr, b: *const SayonStr) -> i32 {
    // SAFETY: valid strings.
    unsafe { as_str(a).cmp(as_str(b)) as i32 }
}

/// The character at a position, counted in characters, into `out`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_str_char_at(
    err: *mut *mut SayonError,
    s: *const SayonStr,
    position: i64,
    out: *mut SayonStr,
    location: *const c_char,
) -> i32 {
    // SAFETY: valid string, output place, and error slot.
    unsafe {
        let text = as_str(s);
        let found = usize::try_from(position)
            .ok()
            .and_then(|position| text.chars().nth(position));
        match found {
            Some(ch) => {
                *out = character(ch);
                0
            }
            None => {
                let message = text::position_error(position as i128, text.chars().count());
                set_error(err, "BoundsError", message, location)
            }
        }
    }
}

/// The character starting at byte `*offset` into `out`, advancing the offset.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_str_next_char(
    s: *const SayonStr,
    offset: *mut i64,
    out: *mut SayonStr,
) {
    // SAFETY: valid string and places; the offset is a character boundary
    // before the end of the string.
    unsafe {
        let text = as_str(s);
        let ch = text[*offset as usize..]
            .chars()
            .next()
            .expect("a character");
        *offset += ch.len_utf8() as i64;
        *out = character(ch);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_str_chars(s: *const SayonStr) -> i64 {
    // SAFETY: valid string.
    unsafe { as_str(s).chars().count() as i64 }
}

// ----- building text -------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn sayon_builder_new() -> *mut Builder {
    allocated();
    Box::into_raw(Box::new(Builder(String::new())))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_builder_bytes(b: *mut Builder, ptr: *const u8, len: i64) {
    // SAFETY: `ptr` holds `len` bytes of UTF-8 text from a constant.
    unsafe {
        if len > 0 {
            let bytes = std::slice::from_raw_parts(ptr, len as usize);
            (*b).0.push_str(std::str::from_utf8_unchecked(bytes));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_builder_str(b: *mut Builder, s: *const SayonStr, quoted: bool) {
    // SAFETY: valid builder and string.
    unsafe {
        if quoted {
            text::quote(as_str(s), &mut (*b).0);
        } else {
            (*b).0.push_str(as_str(s));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_builder_int(b: *mut Builder, value: i64, code: u32) {
    // SAFETY: valid builder.
    unsafe {
        use std::fmt::Write as _;
        let _ = write!((*b).0, "{}", widen(value, code));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_builder_f64(b: *mut Builder, value: f64) {
    // SAFETY: valid builder.
    unsafe { (*b).0.push_str(&text::format_f64(value)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_builder_f32(b: *mut Builder, value: f32) {
    // SAFETY: valid builder.
    unsafe { (*b).0.push_str(&text::format_f32(value)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_builder_bool(b: *mut Builder, value: bool) {
    // SAFETY: valid builder.
    unsafe { (*b).0.push_str(if value { "true" } else { "false" }) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_builder_error(
    b: *mut Builder,
    e: *const *mut SayonError,
    nested: bool,
) {
    // SAFETY: valid builder and error slot.
    unsafe {
        let error = &**e;
        if nested {
            (*b).0.push_str("Error(");
            text::quote(&error.message, &mut (*b).0);
            (*b).0.push(')');
        } else {
            (*b).0.push_str(&error.message);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_builder_range(b: *mut Builder, r: *const SayonRange) {
    // SAFETY: valid builder and range.
    unsafe {
        use std::fmt::Write as _;
        let range = &*r;
        if range.step == 1 {
            let _ = write!((*b).0, "range({}, {})", range.start, range.end);
        } else {
            let _ = write!(
                (*b).0,
                "range({}, {}, {})",
                range.start,
                range.end,
                range.step
            );
        }
    }
}

/// Consumes the builder and produces an owned string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_builder_finish(b: *mut Builder, out: *mut SayonStr) {
    // SAFETY: `b` came from `sayon_builder_new` and is not used again.
    unsafe {
        let builder = Box::from_raw(b);
        released();
        *out = owned(builder.0);
    }
}

// ----- output ----------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_print(
    err: *mut *mut SayonError,
    s: *const SayonStr,
    location: *const c_char,
) -> i32 {
    // SAFETY: valid string and error slot.
    unsafe {
        let mut stdout = std::io::stdout().lock();
        let result = stdout
            .write_all(as_str(s).as_bytes())
            .and_then(|_| stdout.write_all(b"\n"));
        match result {
            Ok(()) => 0,
            Err(error) => set_error(
                err,
                "IOError",
                format!("could not write output: {error}"),
                location,
            ),
        }
    }
}

// ----- closure environments ---------------------------------------------------------------

fn block_layout(size: i64) -> Layout {
    Layout::from_size_align(size.max(1) as usize, ELEMENT_ALIGN).expect("block layout")
}

/// Allocates `size` bytes for a closure environment.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_alloc(size: i64) -> *mut u8 {
    let layout = block_layout(size);
    // SAFETY: the layout has a nonzero size.
    let pointer = unsafe { alloc(layout) };
    if pointer.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    allocated();
    pointer
}

/// Frees a block from `sayon_alloc` of the same size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_free(pointer: *mut u8, size: i64) {
    released();
    // SAFETY: `pointer` came from `sayon_alloc(size)`.
    unsafe { dealloc(pointer, block_layout(size)) }
}

// ----- arrays ------------------------------------------------------------------------------

fn array_layout(capacity: usize, element_size: usize) -> Layout {
    Layout::from_size_align((capacity * element_size).max(1), ELEMENT_ALIGN).expect("array layout")
}

/// Ensures room for `additional` more elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_array_reserve(
    a: *mut SayonArray,
    additional: i64,
    element_size: i64,
) {
    // SAFETY: `a` is a valid array whose storage came from this module.
    unsafe {
        let array = &mut *a;
        let needed = array.len as usize + additional as usize;
        if needed <= array.cap as usize {
            return;
        }
        let size = element_size.max(1) as usize;
        let capacity = needed.max(array.cap as usize * 2).max(4);
        let new_layout = array_layout(capacity, size);
        let pointer = if array.ptr.is_null() || array.cap == 0 {
            allocated();
            alloc(new_layout)
        } else {
            realloc(
                array.ptr,
                array_layout(array.cap as usize, size),
                new_layout.size(),
            )
        };
        if pointer.is_null() {
            std::alloc::handle_alloc_error(new_layout);
        }
        array.ptr = pointer;
        array.cap = capacity as i64;
    }
}

/// Releases array storage (elements must already be destroyed) and leaves it empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_array_free(a: *mut SayonArray, element_size: i64) {
    // SAFETY: valid array.
    unsafe {
        let array = &mut *a;
        if !array.ptr.is_null() && array.cap > 0 {
            released();
            dealloc(
                array.ptr,
                array_layout(array.cap as usize, element_size.max(1) as usize),
            );
        }
        *array = SayonArray {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        };
    }
}

/// Checks the bounds of a view of an array or view of `length` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_check_view(
    err: *mut *mut SayonError,
    start: i64,
    end: i64,
    length: i64,
    location: *const c_char,
) -> i32 {
    if 0 <= start && start <= end && end <= length {
        return 0;
    }
    let message = text::view_bounds_message(start as i128, end as i128, length as usize);
    // SAFETY: valid error slot.
    unsafe { set_error(err, "BoundsError", message, location) }
}

/// Exchanges two elements, or fails when either position is outside the
/// array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_array_swap(
    err: *mut *mut SayonError,
    a: *mut SayonArray,
    first: i64,
    second: i64,
    element_size: i64,
    location: *const c_char,
) -> i32 {
    // SAFETY: valid array and error slot.
    unsafe {
        let array = &mut *a;
        for index in [first, second] {
            if index < 0 || index >= array.len {
                return sayon_check_index(err, index, array.len, location);
            }
        }
        if first != second {
            let size = element_size.max(0) as usize;
            std::ptr::swap_nonoverlapping(
                array.ptr.add(first as usize * size),
                array.ptr.add(second as usize * size),
                size,
            );
        }
        0
    }
}

/// Moves the element at `index` into `out` and the later elements down by
/// one, or fails outside the array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_array_remove(
    err: *mut *mut SayonError,
    a: *mut SayonArray,
    index: i64,
    element_size: i64,
    out: *mut u8,
    location: *const c_char,
) -> i32 {
    // SAFETY: valid array, output of `element_size` bytes, and error slot.
    unsafe {
        let array = &mut *a;
        if index < 0 || index >= array.len {
            return sayon_check_index(err, index, array.len, location);
        }
        let size = element_size.max(0) as usize;
        let start = array.ptr.add(index as usize * size);
        std::ptr::copy_nonoverlapping(start, out, size);
        let later = (array.len - index - 1) as usize * size;
        std::ptr::copy(start.add(size), start, later);
        array.len -= 1;
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_check_index(
    err: *mut *mut SayonError,
    index: i64,
    length: i64,
    location: *const c_char,
) -> i32 {
    if index >= 0 && index < length {
        return 0;
    }
    let message = if length == 0 {
        format!("index {index} is out of bounds: the array is empty")
    } else {
        format!("index {index} is out of bounds for an array of length {length}")
    };
    // SAFETY: valid error slot.
    unsafe { set_error(err, "BoundsError", message, location) }
}

// ----- dictionaries ------------------------------------------------------------------------

/// The generated helpers a dictionary uses for its key type.
#[repr(C)]
pub struct KeyOps {
    pub size: i64,
    pub hash: unsafe extern "C" fn(*const u8) -> i64,
    pub eq: EqFn,
    /// `None` for keys that are copied bit for bit.
    pub clone: CloneFn,
    /// `None` for keys that own nothing.
    pub drop: DropFn,
    pub display: unsafe extern "C" fn(*mut Builder, *const u8, bool),
}

/// Spreads the bits of a key hash over the table.
fn mix(hash: i64) -> u64 {
    let mut x = hash as u64;
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

impl SayonDict {
    fn len(&self) -> usize {
        self.hashes.len()
    }

    fn key(&self, position: usize) -> *const u8 {
        // SAFETY: the position is below the length.
        unsafe { self.keys.as_ptr().add(position * self.key_stride) as *const u8 }
    }

    fn value(&mut self, position: usize) -> *mut u8 {
        // SAFETY: the position is below the length.
        unsafe { self.values.as_mut_ptr().add(position * self.stride) as *mut u8 }
    }

    unsafe fn hash_of(&self, key: *const u8) -> u64 {
        // SAFETY: the key is a valid value of the key type.
        unsafe { mix(((*self.ops).hash)(key)) }
    }

    unsafe fn find(&self, key: *const u8) -> Option<usize> {
        if self.table.is_empty() {
            return None;
        }
        // SAFETY: valid key; the table holds positions of stored keys.
        unsafe {
            let hash = self.hash_of(key);
            let mask = self.table.len() - 1;
            let mut slot = hash as usize & mask;
            loop {
                let entry = self.table[slot];
                if entry == 0 {
                    return None;
                }
                let position = entry as usize - 1;
                if self.hashes[position] == hash && ((*self.ops).eq)(self.key(position), key) {
                    return Some(position);
                }
                slot = (slot + 1) & mask;
            }
        }
    }

    fn place(&mut self, position: usize) {
        let mask = self.table.len() - 1;
        let mut slot = self.hashes[position] as usize & mask;
        while self.table[slot] != 0 {
            slot = (slot + 1) & mask;
        }
        self.table[slot] = position as u32 + 1;
    }

    /// Appends a copy of the key with an empty value, returning its position.
    unsafe fn push(&mut self, key: *const u8, hash: u64) -> usize {
        if (self.len() + 1) * 4 > self.table.len() * 3 {
            self.table = vec![0; (self.table.len() * 2).max(8)];
            for position in 0..self.len() {
                self.place(position);
            }
        }
        let position = self.len();
        self.keys.resize(self.keys.len() + self.key_stride, 0);
        // SAFETY: the new key storage is `key_stride` words.
        unsafe {
            let target = self.keys.as_mut_ptr().add(position * self.key_stride) as *mut u8;
            match (*self.ops).clone {
                Some(clone) => clone(key, target),
                None => std::ptr::copy_nonoverlapping(key, target, (*self.ops).size as usize),
            }
        }
        self.hashes.push(hash);
        self.values.resize(self.values.len() + self.stride, 0);
        self.place(position);
        position
    }

    unsafe fn describe(&self, key: *const u8) -> String {
        let mut builder = Builder(String::new());
        // SAFETY: valid key and builder.
        unsafe { ((*self.ops).display)(&mut builder, key, true) };
        builder.0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_new(
    out: *mut *mut SayonDict,
    ops: *const KeyOps,
    value_size: i64,
) {
    let value_size = value_size.max(0) as usize;
    // SAFETY: `ops` is a static table from the generated code.
    let key_size = unsafe { (*ops).size.max(0) as usize };
    let dict = SayonDict {
        ops,
        keys: Vec::new(),
        key_stride: key_size.div_ceil(8).max(1),
        hashes: Vec::new(),
        table: Vec::new(),
        values: Vec::new(),
        stride: value_size.div_ceil(8).max(1),
        value_size,
    };
    // SAFETY: `out` is writable.
    allocated();
    unsafe { *out = Box::into_raw(Box::new(dict)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_len(d: *const SayonDict) -> i64 {
    // SAFETY: valid dictionary.
    unsafe { (*d).len() as i64 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_find(d: *const SayonDict, key: *const u8) -> i64 {
    // SAFETY: valid dictionary and key.
    unsafe { (*d).find(key).map(|i| i as i64).unwrap_or(-1) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_find_checked(
    err: *mut *mut SayonError,
    d: *const SayonDict,
    key: *const u8,
    location: *const c_char,
    out: *mut i64,
) -> i32 {
    // SAFETY: valid pointers.
    unsafe {
        match (*d).find(key) {
            Some(position) => {
                *out = position as i64;
                0
            }
            None => {
                let shown = (*d).describe(key);
                set_error(
                    err,
                    "KeyError",
                    format!("the key {shown} is not in the dictionary"),
                    location,
                )
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_key_ptr(d: *const SayonDict, position: i64) -> *const u8 {
    // SAFETY: valid dictionary and position.
    unsafe { (*d).key(position as usize) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_value_ptr(d: *mut SayonDict, position: i64) -> *mut u8 {
    // SAFETY: valid dictionary and position.
    unsafe { (*d).value(position as usize) }
}

/// Inserts or replaces an entry. The key is copied; the value's bytes are
/// moved in, and a replaced value is destroyed with `drop_value`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_set(
    d: *mut SayonDict,
    key: *const u8,
    value: *const u8,
    drop_value: DropFn,
) {
    // SAFETY: valid dictionary, key, and value of `value_size` bytes.
    unsafe {
        let dict = &mut *d;
        let position = match dict.find(key) {
            Some(position) => {
                if let Some(drop_value) = drop_value {
                    drop_value(dict.value(position));
                }
                position
            }
            None => {
                let hash = dict.hash_of(key);
                dict.push(key, hash)
            }
        };
        let target = dict.value(position);
        std::ptr::copy_nonoverlapping(value, target, dict.value_size);
    }
}

/// Removes a key's entry, moving its value into `out`; the other entries
/// keep their order. Returns whether the key was there.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_remove(
    d: *mut SayonDict,
    key: *const u8,
    out: *mut u8,
) -> bool {
    // SAFETY: valid dictionary, key, and output of `value_size` bytes.
    unsafe {
        let dict = &mut *d;
        let Some(position) = dict.find(key) else {
            return false;
        };
        std::ptr::copy_nonoverlapping(dict.value(position), out, dict.value_size);
        if let Some(drop_key) = (*dict.ops).drop {
            drop_key(dict.key(position) as *mut u8);
        }
        let (key_stride, stride) = (dict.key_stride, dict.stride);
        dict.keys
            .drain(position * key_stride..(position + 1) * key_stride);
        dict.values
            .drain(position * stride..(position + 1) * stride);
        dict.hashes.remove(position);
        dict.table.iter_mut().for_each(|slot| *slot = 0);
        for later in 0..dict.len() {
            dict.place(later);
        }
        true
    }
}

/// Destroys every key and value, frees the dictionary, and leaves the slot
/// empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_drop(slot: *mut *mut SayonDict, drop_value: DropFn) {
    // SAFETY: `slot` holds null or a dictionary from `sayon_dict_new`.
    unsafe {
        let d = *slot;
        if d.is_null() {
            return;
        }
        let mut dict = Box::from_raw(d);
        released();
        for position in 0..dict.len() {
            if let Some(drop_value) = drop_value {
                drop_value(dict.value(position));
            }
            if let Some(drop_key) = (*dict.ops).drop {
                drop_key(dict.key(position) as *mut u8);
            }
        }
        *slot = std::ptr::null_mut();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_clone(
    src: *const SayonDict,
    dst: *mut *mut SayonDict,
    clone_value: CloneFn,
) {
    // SAFETY: valid source; `dst` is writable.
    unsafe {
        let source = &*src;
        sayon_dict_new(dst, source.ops, source.value_size as i64);
        let target = &mut **dst;
        for position in 0..source.len() {
            let copied = target.push(source.key(position), source.hashes[position]);
            let from = source.values.as_ptr().add(position * source.stride) as *const u8;
            let to = target.value(copied);
            match clone_value {
                Some(clone_value) => clone_value(from, to),
                None => std::ptr::copy_nonoverlapping(from, to, source.value_size),
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_dict_eq(
    a: *const SayonDict,
    b: *const SayonDict,
    eq_value: EqFn,
) -> bool {
    // SAFETY: valid dictionaries of the same type.
    unsafe {
        let (x, y) = (&*a, &*b);
        if x.len() != y.len() {
            return false;
        }
        for position in 0..x.len() {
            let Some(other) = y.find(x.key(position)) else {
                return false;
            };
            let left = x.values.as_ptr().add(position * x.stride) as *const u8;
            let right = y.values.as_ptr().add(other * y.stride) as *const u8;
            if !eq_value(left, right) {
                return false;
            }
        }
        true
    }
}

/// A hash of a string's bytes, for dictionary keys.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_str_hash(s: *const SayonStr) -> i64 {
    // SAFETY: valid string.
    let text = unsafe { as_str(s) };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash as i64
}

// ----- errors ----------------------------------------------------------------------------------

/// Stores a new error with a message string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_error_raise(
    err: *mut *mut SayonError,
    code: *const c_char,
    message: *const SayonStr,
    location: *const c_char,
) -> i32 {
    // SAFETY: valid pointers.
    unsafe { set_error(err, c_str(code), as_str(message).to_string(), location) }
}

/// Stores a new error with a constant message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_error_raise_text(
    err: *mut *mut SayonError,
    code: *const c_char,
    message: *const c_char,
    location: *const c_char,
) -> i32 {
    // SAFETY: valid pointers.
    unsafe { set_error(err, c_str(code), c_str(message).to_string(), location) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_error_overflow(
    err: *mut *mut SayonError,
    symbol: *const c_char,
    a: i64,
    b: i64,
    code: u32,
    location: *const c_char,
) -> i32 {
    // SAFETY: valid pointers.
    unsafe {
        let message = format!(
            "{} {} {} does not fit in {}",
            widen(a, code),
            c_str(symbol),
            widen(b, code),
            type_name(code)
        );
        set_error(err, "OverflowError", message, location)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_error_negate(
    err: *mut *mut SayonError,
    a: i64,
    code: u32,
    location: *const c_char,
) -> i32 {
    // SAFETY: valid pointers.
    unsafe {
        let message = format!("-({}) does not fit in {}", widen(a, code), type_name(code));
        set_error(err, "OverflowError", message, location)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_error_drop(slot: *mut *mut SayonError) {
    // SAFETY: the slot holds null or an error from this module.
    unsafe {
        if !(*slot).is_null() {
            drop(Box::from_raw(*slot));
            released();
            *slot = std::ptr::null_mut();
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_error_clone(src: *const *mut SayonError, dst: *mut *mut SayonError) {
    // SAFETY: valid slots.
    unsafe {
        let error = &**src;
        allocated();
        *dst = Box::into_raw(Box::new(SayonError {
            code: error.code.clone(),
            message: error.message.clone(),
            location: error.location.clone(),
        }));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_error_message(e: *const *mut SayonError, out: *mut SayonStr) {
    // SAFETY: valid error slot.
    unsafe { *out = owned((**e).message.clone()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_error_code(e: *const *mut SayonError, out: *mut SayonStr) {
    // SAFETY: valid error slot.
    unsafe { *out = owned((**e).code.clone()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_error_eq(
    a: *const *mut SayonError,
    b: *const *mut SayonError,
) -> bool {
    // SAFETY: valid error slots.
    unsafe {
        let (x, y) = (&**a, &**b);
        x.code == y.code && x.message == y.message
    }
}

/// Reports an error nothing caught and releases it. Returns the exit status.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_report_uncaught(e: *mut SayonError) -> i32 {
    let _ = std::io::stdout().flush();
    // SAFETY: `e` came from this module and is not used again.
    unsafe {
        if e.is_null() {
            eprintln!("error: uncaught error");
            return 1;
        }
        let error = Box::from_raw(e);
        released();
        if error.code == crate::host::EXIT_CODE {
            return crate::host::exit_status(&error.message);
        }
        eprintln!("error: uncaught {}: {}", error.code, error.message);
        if let Some(location) = &error.location {
            eprintln!("  --> {location}");
        }
    }
    1
}

// ----- checks and conversions ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_int_convert(
    err: *mut *mut SayonError,
    value: i64,
    from: u32,
    to: u32,
    location: *const c_char,
    out: *mut i64,
) -> i32 {
    let wide = widen(value, from);
    let (min, max) = text::integer_type_range(to);
    // SAFETY: valid pointers.
    unsafe {
        if wide < min || wide > max {
            return set_error(
                err,
                "ConversionError",
                format!("{wide} does not fit in {}", type_name(to)),
                location,
            );
        }
        *out = wide as i64;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_float_to_int(
    err: *mut *mut SayonError,
    value: f64,
    to: u32,
    location: *const c_char,
    out: *mut i64,
) -> i32 {
    // SAFETY: valid pointers.
    unsafe {
        if !value.is_finite() {
            let message = format!(
                "{} is not a finite number, so it has no {} value",
                text::format_f64(value),
                type_name(to)
            );
            return set_error(err, "ConversionError", message, location);
        }
        let truncated = value.trunc();
        let (min, max) = text::integer_type_range(to);
        if truncated < min as f64 || truncated >= (max + 1) as f64 {
            let message = format!(
                "{} does not fit in {}",
                text::format_f64(value),
                type_name(to)
            );
            return set_error(err, "ConversionError", message, location);
        }
        *out = truncated as i128 as i64;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_f64_to_f32(
    err: *mut *mut SayonError,
    value: f64,
    location: *const c_char,
    out: *mut f32,
) -> i32 {
    let narrowed = value as f32;
    // SAFETY: valid pointers.
    unsafe {
        if value.is_finite() && narrowed.is_infinite() {
            let message = format!("{} is too large for float32", text::format_f64(value));
            return set_error(err, "ConversionError", message, location);
        }
        *out = narrowed;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_tonumber(
    err: *mut *mut SayonError,
    s: *const SayonStr,
    location: *const c_char,
    is_float: *mut bool,
    integer: *mut i64,
    float: *mut f64,
) -> i32 {
    // SAFETY: valid pointers.
    unsafe {
        match text::parse_number(as_str(s)) {
            Ok(text::Number::Integer(value)) => {
                *is_float = false;
                *integer = value;
                0
            }
            Ok(text::Number::Float(value)) => {
                *is_float = true;
                *float = value;
                0
            }
            Err((code, message)) => set_error(err, code, message, location),
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_range_new(
    err: *mut *mut SayonError,
    start: i64,
    end: i64,
    step: i64,
    location: *const c_char,
    out: *mut SayonRange,
) -> i32 {
    // SAFETY: valid pointers.
    unsafe {
        if step == 0 {
            return set_error(
                err,
                "RangeError",
                "a range step cannot be zero".to_string(),
                location,
            );
        }
        *out = SayonRange { start, end, step };
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_range_length(r: *const SayonRange) -> i64 {
    // SAFETY: valid range.
    unsafe {
        let range = &*r;
        text::range_length(range.start, range.end, range.step)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_assert_failed(
    err: *mut *mut SayonError,
    message: *const SayonStr,
    location: *const c_char,
) -> i32 {
    // SAFETY: valid pointers; `message` may be null.
    unsafe {
        let text = if message.is_null() {
            "assertion failed".to_string()
        } else {
            as_str(message).to_string()
        };
        set_error(err, "AssertionError", text, location)
    }
}

// ----- files -------------------------------------------------------------------------------------

fn io_message(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "the file does not exist".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => error.to_string(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_read_file(
    err: *mut *mut SayonError,
    path: *const SayonStr,
    location: *const c_char,
    out: *mut SayonStr,
) -> i32 {
    // SAFETY: valid pointers.
    unsafe {
        let path = as_str(path);
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return set_error(
                    err,
                    "IOError",
                    format!("could not read \"{path}\": {}", io_message(&error)),
                    location,
                );
            }
        };
        match String::from_utf8(bytes) {
            Ok(text) => {
                *out = owned(text);
                0
            }
            Err(error) => set_error(
                err,
                "EncodingError",
                format!(
                    "\"{path}\" is not valid UTF-8 (byte {})",
                    error.utf8_error().valid_up_to()
                ),
                location,
            ),
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_write_file(
    err: *mut *mut SayonError,
    path: *const SayonStr,
    content: *const SayonStr,
    location: *const c_char,
) -> i32 {
    // SAFETY: valid pointers.
    unsafe {
        let path = as_str(path);
        match std::fs::write(path, as_str(content).as_bytes()) {
            Ok(()) => 0,
            Err(error) => set_error(
                err,
                "IOError",
                format!("could not write \"{path}\": {}", io_message(&error)),
                location,
            ),
        }
    }
}

// ----- program lifecycle ----------------------------------------------------------------------------

// ----- streams --------------------------------------------------------------------------------------

/// Drops a stream reference; 0 is a moved-from stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_stream_drop(slot: *mut i64) {
    // SAFETY: the slot holds a stream number or 0.
    unsafe {
        if *slot != 0 {
            crate::streams::release(*slot);
            released();
            *slot = 0;
        }
    }
}

/// A clone of a stream refers to the same open file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_stream_clone(src: *const i64, dst: *mut i64) {
    // SAFETY: valid slots.
    unsafe {
        crate::streams::share(*src);
        allocated();
        *dst = *src;
    }
}

// ----- program lifecycle ----------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn sayon_runtime_finish() {
    crate::streams::flush_all();
    let _ = std::io::stdout().flush();
    if std::env::var_os("SAYON_DEBUG_ALLOC").is_some() {
        eprintln!(
            "sayon: allocations live at exit: {}",
            LIVE.load(Ordering::Relaxed)
        );
    }
}

/// Maps the integer `main` returned to the process exit status. Portable
/// statuses are 0 to 255; anything else exits with 255 after a warning.
#[unsafe(no_mangle)]
pub extern "C" fn sayon_exit_status(code: i64) -> i32 {
    if (0..=255).contains(&code) {
        code as i32
    } else {
        let _ = std::io::stdout().flush();
        eprintln!(
            "warning: `main` returned {code}; portable exit statuses are 0 to 255, so 255 is used"
        );
        255
    }
}

// ----- host primitives -------------------------------------------------------

/// Records the program's arguments (without the program name).
#[unsafe(no_mangle)]
pub extern "C" fn sayon_runtime_init() {
    crate::host::set_arguments(std::env::args().skip(1).collect());
}

/// Calls a host primitive. `args` points to one pointer per argument value,
/// in the primitive's parameter order. The result is written through `out`;
/// for an optional string, whether it is present is written through
/// `present` and the text through `out` when it is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sayon_host_call(
    err: *mut *mut SayonError,
    code: u32,
    args: *const *const u8,
    out: *mut u8,
    present: *mut bool,
    location: *const c_char,
) -> i32 {
    use crate::host::{Host, HostArg, HostType, HostValue, invoke};
    let host = Host::from_code(code).expect("known host primitive");
    // SAFETY: the generated code passes valid pointers matching the
    // primitive's parameter and result types.
    unsafe {
        let mut values = Vec::new();
        for (index, ty) in host.params().iter().enumerate() {
            let pointer = *args.add(index);
            values.push(match ty {
                HostType::String => HostArg::String(as_str(pointer as *const SayonStr)),
                HostType::Integer => HostArg::Integer(*(pointer as *const i64)),
                HostType::Float => HostArg::Float(*(pointer as *const f64)),
                HostType::Boolean => HostArg::Boolean(*pointer != 0),
                HostType::StringArray => {
                    let array = &*(pointer as *const SayonArray);
                    let elements: Vec<&str> = (0..array.len as usize)
                        .map(|i| as_str((array.ptr as *const SayonStr).add(i)))
                        .collect();
                    HostArg::Strings(elements)
                }
                HostType::Stream => HostArg::Stream(*(pointer as *const i64)),
                HostType::Bytes => {
                    let array = &*(pointer as *const SayonArray);
                    let bytes = if array.len == 0 {
                        Vec::new()
                    } else {
                        std::slice::from_raw_parts(array.ptr, array.len as usize).to_vec()
                    };
                    HostArg::Bytes(bytes)
                }
                HostType::Nothing | HostType::OptionalString => {
                    panic!("unsupported host parameter")
                }
            });
        }
        match invoke(host, &values) {
            Ok(HostValue::String(text)) => *(out as *mut SayonStr) = owned(text),
            Ok(HostValue::Integer(value)) => *(out as *mut i64) = value,
            Ok(HostValue::Float(value)) => *(out as *mut f64) = value,
            Ok(HostValue::Boolean(value)) => *out = value as u8,
            Ok(HostValue::Nothing) => *out = 0,
            Ok(HostValue::Strings(items)) => {
                let array = out as *mut SayonArray;
                *array = SayonArray {
                    ptr: std::ptr::null_mut(),
                    len: 0,
                    cap: 0,
                };
                let element_size = std::mem::size_of::<SayonStr>() as i64;
                let count = items.len() as i64;
                if count > 0 {
                    sayon_array_reserve(array, count, element_size);
                }
                for (index, item) in items.into_iter().enumerate() {
                    let target = ((*array).ptr as *mut SayonStr).add(index);
                    std::ptr::write(target, owned(item));
                }
                (*array).len = count;
            }
            Ok(HostValue::OptionalString(text)) => {
                *present = text.is_some();
                if let Some(text) = text {
                    *(out as *mut SayonStr) = owned(text);
                }
            }
            Ok(HostValue::Stream(id)) => {
                allocated();
                *(out as *mut i64) = id;
            }
            Ok(HostValue::Bytes(bytes)) => {
                let array = out as *mut SayonArray;
                *array = SayonArray {
                    ptr: std::ptr::null_mut(),
                    len: 0,
                    cap: 0,
                };
                let count = bytes.len() as i64;
                if count > 0 {
                    sayon_array_reserve(array, count, 1);
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), (*array).ptr, bytes.len());
                }
                (*array).len = count;
            }
            Err((code, message)) => return set_error(err, code, message, location),
        }
    }
    0
}
