//! Calls of C functions from interpreted programs.
//!
//! A library is loaded the first time one of its functions is called, and
//! each function is looked up by its name. The call itself goes through a
//! small routine written for the target's calling convention, which takes
//! the arguments as words already in their places, so one routine serves
//! every signature, variadic ones included; the result comes back from the
//! integer and the float result registers, and the caller keeps what the
//! signature names.
//!
//! On Windows x64 each argument has a position, whose integer and float
//! registers both get it (as a variadic function wants of a float), and a
//! structure goes in its position's register when it has 1, 2, 4, or 8
//! bytes, or by the address of a copy. On Linux (System V) integers and
//! floats take the next register of their own kind, and a structure of up
//! to 16 bytes goes in a register for each of its eightbytes, of the kind
//! the eightbyte needs, while a larger one is copied onto the stack. A
//! structure given back in memory, on either, is written where an address
//! passed before the arguments points.
//!
//! A function value goes to C as one of a fixed set of entry points, one
//! for each parameter of a C function that is a function, which C calls
//! with its convention. An entry point finds the function value in the
//! newest call of its C function, which the call recorded, and runs it on
//! the machine making that call; as in compiled programs, the function
//! cannot wait, and its first failure is kept for the call to raise once C
//! returns, C getting 0 from it and from every later call meanwhile.

use crate::value::{ErrorValue, Fields, Ref, Value};
use sayon_check::types::{FloatType, FunctionType, IntType, Type, TypeDef, c_layout, c_size_align};
use sayon_ir::ForeignFunction;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

/// The libraries and functions found so far.
#[derive(Default)]
pub struct Foreign {
    /// The handles of each library, by the name its `foreign` block gave it.
    /// The C runtime is more than one library.
    libraries: HashMap<String, Vec<usize>>,
    /// The address of each function, by its position in the module.
    functions: HashMap<u32, usize>,
    /// The C runtime's `fflush`, called after each of its functions so that
    /// what C prints comes out in order with what the program prints.
    flush: Option<usize>,
}

/// Why a call could not be made.
pub type ForeignError = String;

/// Why a call failed.
pub enum CallFailure {
    /// It could not be made.
    Foreign(ForeignError),
    /// A function that C called back failed.
    Callback(ErrorValue),
}

/// Where a C function is, found before it is called.
pub struct Target {
    address: usize,
    flush: Option<usize>,
}

/// Runs a function value on a machine, for C that calls it back.
pub type RunCallback = unsafe fn(*mut c_void, &Ref, Vec<Value>) -> Result<Value, ErrorValue>;

/// What a call of a C function that takes functions gives the entry points
/// C calls: the machine making the call, and the function value of each
/// parameter that is a function.
pub struct Callbacks {
    pub machine: *mut c_void,
    pub run: RunCallback,
    pub values: Vec<Option<Ref>>,
}

impl Foreign {
    /// Finds the function at `index` of the module's C functions.
    pub fn target(
        &mut self,
        index: u32,
        function: &ForeignFunction,
    ) -> Result<Target, ForeignError> {
        let address = self.function(index, function)?;
        let flush = if function.library == "c" {
            self.flush
        } else {
            None
        };
        Ok(Target { address, flush })
    }
}

/// Calls the function at `index` of the module's C functions with `args`,
/// each value with its C type, and gives its result.
pub fn call(
    target: Target,
    index: u32,
    function: &ForeignFunction,
    args: &[(Value, Type)],
    types: &[TypeDef],
    callbacks: Option<Callbacks>,
) -> Result<Value, CallFailure> {
    let mut pieces = Vec::new();
    for (position, (value, ty)) in args.iter().enumerate() {
        pieces.push(match ty {
            Type::Function(_) => Piece::Integer(
                callback_entry(index, position).map_err(CallFailure::Foreign)? as u64,
            ),
            Type::Named(_) => {
                let (size, _) = c_size_align(types, ty, sayon_target::host());
                let mut words = vec![0u64; size.div_ceil(8).max(1) as usize];
                // SAFETY: the words have room for the structure.
                unsafe { write(words.as_mut_ptr() as usize, value, ty, types) };
                Piece::Structure(words, ty.clone())
            }
            Type::Float(_) => Piece::Float(slot(value, ty)),
            _ => Piece::Integer(slot(value, ty)),
        });
    }
    let calling_back = callbacks.is_some();
    if let Some(callbacks) = callbacks {
        let signatures = function
            .params
            .iter()
            .map(|param| match param {
                Type::Function(signature) => Some((**signature).clone()),
                _ => None,
            })
            .collect();
        CALLS.with(|calls| {
            calls.borrow_mut().push(Call {
                function: index,
                machine: callbacks.machine,
                run: callbacks.run,
                values: callbacks.values,
                signatures,
                failed: None,
            })
        });
    }
    // SAFETY: the address is the named C function, and unsafe code
    // vouched for the declared signature and the arguments.
    let value = unsafe { platform::invoke(target.address, &pieces, &function.ret, types) };
    if let Some(flush) = target.flush {
        // SAFETY: `fflush(NULL)` flushes every output stream.
        unsafe { platform::invoke(flush, &[Piece::Integer(0)], &Type::Int(IntType::I32), types) };
    }
    if calling_back {
        let call = CALLS
            .with(|calls| calls.borrow_mut().pop())
            .expect("the record of the call");
        if let Some(problem) = call.failed {
            return Err(CallFailure::Callback(problem));
        }
    }
    Ok(value)
}

/// An argument, for the platform's convention to place.
enum Piece {
    /// In an integer register, or a word of the stack.
    Integer(u64),
    /// In a float register, or a word of the stack; a `float32` in the low
    /// 32 bits.
    Float(u64),
    /// A structure's bytes, in words, and its type.
    Structure(Vec<u64>, Type),
}

/// The value of the declared result type, from the registers C gave it in:
/// the integer and the float ones in order, or, for one C wrote in memory,
/// there.
///
/// # Safety
/// `memory`, when there is one, holds the structure C wrote.
unsafe fn given_back(
    ty: &Type,
    integers: [u64; 2],
    floats: [u64; 2],
    memory: Option<&[u64]>,
    types: &[TypeDef],
    eightbytes: Option<Vec<sayon_check::types::Eightbyte>>,
) -> Value {
    if let Some(words) = memory {
        // SAFETY: as the caller vouches.
        return unsafe { read(words.as_ptr() as usize, ty, types) };
    }
    match (ty, eightbytes) {
        (Type::Named(_), Some(classes)) => {
            let (mut integer, mut float) = (integers.into_iter(), floats.into_iter());
            let words: Vec<u64> = classes
                .iter()
                .map(|class| match class {
                    sayon_check::types::Eightbyte::Integer => integer.next().unwrap_or(0),
                    sayon_check::types::Eightbyte::Sse => float.next().unwrap_or(0),
                })
                .collect();
            // SAFETY: the words hold the structure's eightbytes.
            unsafe { read(words.as_ptr() as usize, ty, types) }
        }
        _ => result(ty, integers[0], floats[0]),
    }
}

/// A call of a C function that takes functions, while C runs.
struct Call {
    function: u32,
    machine: *mut c_void,
    run: RunCallback,
    values: Vec<Option<Ref>>,
    signatures: Vec<Option<FunctionType>>,
    failed: Option<ErrorValue>,
}

thread_local! {
    static CALLS: RefCell<Vec<Call>> = const { RefCell::new(Vec::new()) };
}

/// The C function and parameter each entry point serves, by its number.
static ENTRIES: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());

/// The address of the entry point for parameter `position` of the C
/// function at `index`.
fn callback_entry(index: u32, position: usize) -> Result<usize, ForeignError> {
    let mut entries = ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
    let number = match entries.iter().position(|entry| *entry == (index, position)) {
        Some(number) => number,
        None if entries.len() < platform::CALLBACK_ENTRIES => {
            entries.push((index, position));
            entries.len() - 1
        }
        None => {
            return Err(format!(
                "the interpreter passes functions to at most {} parameters of C functions",
                platform::CALLBACK_ENTRIES
            ));
        }
    };
    Ok(platform::callback_entry(number))
}

/// What an entry point runs when C calls it: the function value of its
/// parameter in the newest call of its C function, with the arguments C
/// passed, whose bits `arguments` reads for their types, as the platform's
/// convention placed them. Gives the result's bits for both result
/// registers.
#[cfg_attr(
    not(any(
        all(windows, target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64")
    )),
    allow(dead_code)
)]
unsafe fn called_back(number: usize, arguments: impl FnOnce(&[Type]) -> Vec<u64>) -> u64 {
    let (index, position) = ENTRIES.lock().unwrap_or_else(|e| e.into_inner())[number];
    let found = CALLS.with(|calls| {
        let calls = calls.borrow();
        let at = calls.iter().rposition(|call| call.function == index)?;
        let call = &calls[at];
        if call.failed.is_some() {
            return Some(None);
        }
        let reference = call.values[position].clone()?;
        let signature = call.signatures[position].clone()?;
        Some(Some((at, call.machine, call.run, reference, signature)))
    });
    let Some(found) = found else {
        eprintln!(
            "fatal error: C called a Sayon function after the call it was given to returned, or on another thread"
        );
        std::process::abort();
    };
    let Some((at, machine, run, reference, signature)) = found else {
        return 0;
    };
    let params: Vec<Type> = signature.params.iter().map(|(_, ty)| ty.clone()).collect();
    let args = params
        .iter()
        .zip(arguments(&params))
        .map(|(ty, bits)| result(ty, bits, bits))
        .collect();
    // SAFETY: the machine waits in the call of the C function.
    match unsafe { run(machine, &reference, args) } {
        Ok(value) => match value {
            Value::Int(number) => number as u64,
            Value::F64(number) => number.to_bits(),
            Value::F32(number) => number.to_bits() as u64,
            Value::Pointer(address) => address as u64,
            _ => 0,
        },
        Err(problem) => {
            CALLS.with(|calls| {
                calls.borrow_mut()[at].failed.get_or_insert(problem);
            });
            0
        }
    }
}

impl Foreign {
    fn function(&mut self, index: u32, function: &ForeignFunction) -> Result<usize, ForeignError> {
        if let Some(address) = self.functions.get(&index) {
            return Ok(*address);
        }
        let handles = self.library(&function.library)?;
        let address = handles
            .iter()
            .find_map(|handle| platform::symbol(*handle, &function.name))
            .ok_or_else(|| {
                format!(
                    "the library \"{}\" has no function `{}`",
                    function.library, function.name
                )
            })?;
        if function.library == "c" && self.flush.is_none() {
            self.flush = handles
                .iter()
                .find_map(|handle| platform::symbol(*handle, "fflush"));
        }
        self.functions.insert(index, address);
        Ok(address)
    }

    fn library(&mut self, name: &str) -> Result<Vec<usize>, ForeignError> {
        if let Some(handles) = self.libraries.get(name) {
            return Ok(handles.clone());
        }
        let candidates = platform::library_files(name);
        let handles: Vec<usize> = candidates
            .iter()
            .filter_map(|file| platform::open(file))
            .collect();
        if handles.is_empty() {
            return Err(platform::missing_library(name));
        }
        self.libraries.insert(name.to_string(), handles.clone());
        Ok(handles)
    }
}

/// The 64 bits an argument is passed in. Integers are extended to 64 bits;
/// a `float32` is in the low 32.
fn slot(value: &Value, ty: &Type) -> u64 {
    match (value, ty) {
        (Value::Int(number), _) => *number as i64 as u64,
        (Value::F64(number), _) => number.to_bits(),
        (Value::F32(number), _) => number.to_bits() as u64,
        (Value::Pointer(address), _) => *address as u64,
        (Value::CString(bytes), _) => bytes.as_ptr() as u64,
        (value, ty) => panic!("C argument {value:?} of type {ty}"),
    }
}

/// The result of the declared type from the result registers.
fn result(ty: &Type, integer: u64, float: u64) -> Value {
    match ty {
        // C leaves the bits above a narrow result undefined.
        Type::Int(int) => {
            let shift = 64 - int.bits();
            let value = if int.signed() {
                ((integer << shift) as i64 >> shift) as i128
            } else {
                ((integer << shift) >> shift) as i128
            };
            Value::Int(value)
        }
        Type::Float(FloatType::F64) => Value::F64(f64::from_bits(float)),
        Type::Float(FloatType::F32) => Value::F32(f32::from_bits(float as u32)),
        Type::Pointer => Value::Pointer(integer as usize),
        _ => Value::Nothing,
    }
}

/// Reads a number, pointer, or C structure of type `ty` at `address`.
///
/// # Safety
/// The memory must hold that many readable bytes.
pub unsafe fn read(address: usize, ty: &Type, types: &[TypeDef]) -> Value {
    use sayon_check::types::IntType;
    let p = address as *const u8;
    // SAFETY: the caller vouches for the memory; reads need no alignment.
    unsafe {
        match ty {
            Type::Named(named) => {
                let fields: Vec<Value> = c_layout(types, named, sayon_target::host())
                    .fields
                    .iter()
                    .map(|(offset, field)| read(address + *offset as usize, field, types))
                    .collect();
                Value::Record(named.id, Fields::from(fields))
            }
            Type::Int(int) => Value::Int(match int {
                IntType::I8 => (p as *const i8).read_unaligned() as i128,
                IntType::I16 => (p as *const i16).read_unaligned() as i128,
                IntType::I32 => (p as *const i32).read_unaligned() as i128,
                IntType::I64 => (p as *const i64).read_unaligned() as i128,
                IntType::U8 => p.read_unaligned() as i128,
                IntType::U16 => (p as *const u16).read_unaligned() as i128,
                IntType::U32 => (p as *const u32).read_unaligned() as i128,
                IntType::U64 => (p as *const u64).read_unaligned() as i128,
            }),
            Type::Float(FloatType::F64) => Value::F64((p as *const f64).read_unaligned()),
            Type::Float(FloatType::F32) => Value::F32((p as *const f32).read_unaligned()),
            Type::Pointer => Value::Pointer((p as *const usize).read_unaligned()),
            other => panic!("raw read of {other}"),
        }
    }
}

/// Stores a number, pointer, or C structure at `address`.
///
/// # Safety
/// The memory must have room for the value.
pub unsafe fn write(address: usize, value: &Value, ty: &Type, types: &[TypeDef]) {
    use sayon_check::types::IntType;
    let p = address as *mut u8;
    // SAFETY: the caller vouches for the memory; writes need no alignment.
    unsafe {
        match (ty, value) {
            (Type::Named(named), Value::Record(_, fields)) => {
                let layout = c_layout(types, named, sayon_target::host());
                for ((offset, field_ty), field) in layout.fields.iter().zip(fields.iter()) {
                    write(address + *offset as usize, field, field_ty, types);
                }
            }
            (Type::Int(int), Value::Int(number)) => match int {
                IntType::I8 | IntType::U8 => p.write_unaligned(*number as u8),
                IntType::I16 | IntType::U16 => (p as *mut u16).write_unaligned(*number as u16),
                IntType::I32 | IntType::U32 => (p as *mut u32).write_unaligned(*number as u32),
                IntType::I64 | IntType::U64 => (p as *mut u64).write_unaligned(*number as u64),
            },
            (Type::Float(_), Value::F64(number)) => (p as *mut f64).write_unaligned(*number),
            (Type::Float(_), Value::F32(number)) => (p as *mut f32).write_unaligned(*number),
            (Type::Pointer, Value::Pointer(address)) => (p as *mut usize).write_unaligned(*address),
            (ty, value) => panic!("raw write of {value:?} as {ty}"),
        }
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
mod platform {
    use super::{Piece, given_back};
    use crate::value::Value;
    use sayon_check::types::{Type, TypeDef, c_size_align, win64_in_register};
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    // Windows x64: the first four arguments go in RCX, RDX, R8, and R9, or
    // XMM0 to XMM3 for floats, each with 32 bytes reserved for it on the
    // stack; the rest follow on the stack. Loading every position into both
    // kinds of register suits every signature, and is what a variadic
    // function needs for a float. `count` is at least four.
    std::arch::global_asm!(
        ".text",
        ".p2align 4",
        ".globl sayon_interp_call_c",
        ".def sayon_interp_call_c",
        ".scl 2",
        ".type 32",
        ".endef",
        ".seh_proc sayon_interp_call_c",
        "sayon_interp_call_c:",
        "push rbp",
        ".seh_pushreg rbp",
        "push rbx",
        ".seh_pushreg rbx",
        "push rsi",
        ".seh_pushreg rsi",
        "push rdi",
        ".seh_pushreg rdi",
        "push r12",
        ".seh_pushreg r12",
        "mov rbp, rsp",
        ".seh_setframe rbp, 0",
        ".seh_endprologue",
        "mov rbx, rcx",
        "mov rsi, rdx",
        "mov rdi, r8",
        "mov r12, r9",
        "lea rax, [rdi*8 + 15]",
        "and rax, -16",
        "sub rsp, rax",
        "mov rcx, 4",
        "2:",
        "cmp rcx, rdi",
        "jae 3f",
        "mov rax, qword ptr [rsi + rcx*8]",
        "mov qword ptr [rsp + rcx*8], rax",
        "inc rcx",
        "jmp 2b",
        "3:",
        "mov rcx, qword ptr [rsi]",
        "mov rdx, qword ptr [rsi + 8]",
        "mov r8, qword ptr [rsi + 16]",
        "mov r9, qword ptr [rsi + 24]",
        "movq xmm0, rcx",
        "movq xmm1, rdx",
        "movq xmm2, r8",
        "movq xmm3, r9",
        "call rbx",
        "mov qword ptr [r12], rax",
        "movq qword ptr [r12 + 8], xmm0",
        "lea rsp, [rbp]",
        "pop r12",
        "pop rdi",
        "pop rsi",
        "pop rbx",
        "pop rbp",
        "ret",
        ".seh_endproc",
    );

    unsafe extern "C" {
        fn sayon_interp_call_c(function: usize, slots: *const u64, count: usize, out: *mut u64);
        fn sayon_interp_callback_entries();
    }

    /// Calls `function` with the pieces in their positions.
    ///
    /// # Safety
    /// `function` must take arguments matching the pieces.
    pub(super) unsafe fn invoke(
        function: usize,
        pieces: &[Piece],
        ret: &Type,
        types: &[TypeDef],
    ) -> Value {
        // The copies of structures C takes or gives by address, which live
        // through the call.
        let mut copies: Vec<Vec<u64>> = Vec::new();
        let mut slots = Vec::new();
        let in_memory = match ret {
            Type::Named(_) => {
                let (size, _) = c_size_align(types, ret, sayon_target::host());
                (!win64_in_register(size)).then(|| {
                    copies.push(vec![0; size.div_ceil(8) as usize]);
                    slots.push(copies[0].as_ptr() as u64);
                })
            }
            _ => None,
        };
        for piece in pieces {
            match piece {
                Piece::Integer(word) | Piece::Float(word) => slots.push(*word),
                Piece::Structure(words, ty) => {
                    let (size, _) = c_size_align(types, ty, sayon_target::host());
                    if win64_in_register(size) {
                        slots.push(words[0]);
                    } else {
                        let copy = words.clone();
                        slots.push(copy.as_ptr() as u64);
                        copies.push(copy);
                    }
                }
            }
        }
        // The first four positions are always loaded.
        while slots.len() < 4 {
            slots.push(0);
        }
        let mut out = [0u64; 2];
        // SAFETY: the routine reads `slots.len()` slots and writes two words.
        unsafe { sayon_interp_call_c(function, slots.as_ptr(), slots.len(), out.as_mut_ptr()) };
        let memory = in_memory.map(|()| copies[0].as_slice());
        // A structure in a register is the integer's bytes.
        let eightbytes = match ret {
            Type::Named(_) => Some(vec![sayon_check::types::Eightbyte::Integer]),
            _ => None,
        };
        // SAFETY: C wrote a structure in memory into the first copy.
        unsafe { given_back(ret, [out[0], 0], [out[1], 0], memory, types, eightbytes) }
    }

    /// The entry points C can call back.
    pub const CALLBACK_ENTRIES: usize = 64;

    /// Entry points 16 bytes apart, each putting its number in EAX before
    /// the routine they share. That routine stores the first four
    /// arguments' integer registers in the 32 bytes the caller reserved for
    /// them, just below the arguments on the stack, so that every argument
    /// is in one array of words, and their float registers beside it; it
    /// gives the result in both RAX and XMM0.
    macro_rules! callback_entries {
        ($($number:literal)*) => {
            std::arch::global_asm!(
                ".text",
                ".p2align 4",
                ".globl sayon_interp_callback_entries",
                "sayon_interp_callback_entries:",
                $(
                    ".p2align 4",
                    concat!("mov eax, ", stringify!($number)),
                    "jmp sayon_interp_callback_common",
                )*
                ".p2align 4",
                ".def sayon_interp_callback_common",
                ".scl 3",
                ".type 32",
                ".endef",
                ".seh_proc sayon_interp_callback_common",
                "sayon_interp_callback_common:",
                "mov qword ptr [rsp + 8], rcx",
                "mov qword ptr [rsp + 16], rdx",
                "mov qword ptr [rsp + 24], r8",
                "mov qword ptr [rsp + 32], r9",
                "sub rsp, 72",
                ".seh_stackalloc 72",
                ".seh_endprologue",
                "movq qword ptr [rsp + 32], xmm0",
                "movq qword ptr [rsp + 40], xmm1",
                "movq qword ptr [rsp + 48], xmm2",
                "movq qword ptr [rsp + 56], xmm3",
                "mov ecx, eax",
                "lea rdx, [rsp + 80]",
                "lea r8, [rsp + 32]",
                "call sayon_interp_called_back",
                "movq xmm0, rax",
                "add rsp, 72",
                "ret",
                ".seh_endproc",
            );
        };
    }

    callback_entries!(
        0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31
        32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60
        61 62 63
    );

    #[unsafe(no_mangle)]
    unsafe extern "C" fn sayon_interp_called_back(
        number: u32,
        ints: *const u64,
        floats: *const u64,
    ) -> u64 {
        // SAFETY: the shared routine passes the arguments' words: each
        // position's integer register, then the stack, and the first four
        // positions' float registers.
        unsafe {
            super::called_back(number as usize, |params| {
                params
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| match ty {
                        Type::Float(_) if i < 4 => *floats.add(i),
                        _ => *ints.add(i),
                    })
                    .collect()
            })
        }
    }

    pub fn callback_entry(number: usize) -> usize {
        sayon_interp_callback_entries as *const () as usize + 16 * number
    }

    /// The files a library name stands for. The C runtime is the classic
    /// one, which has every C function (the newer one leaves `printf` and
    /// its kin to headers), then the newer one for what is only there.
    pub fn library_files(name: &str) -> Vec<String> {
        if name == "c" {
            return vec!["msvcrt.dll".to_string(), "ucrtbase.dll".to_string()];
        }
        let file = if name.contains('.') {
            name.to_string()
        } else {
            format!("{name}.dll")
        };
        vec![file]
    }

    pub fn open(file: &str) -> Option<usize> {
        let wide: Vec<u16> = file.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the name is NUL-terminated.
        let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
        (!handle.is_null()).then_some(handle as usize)
    }

    pub fn symbol(handle: usize, name: &str) -> Option<usize> {
        let mut bytes = name.as_bytes().to_vec();
        bytes.push(0);
        // SAFETY: the handle is a loaded library and the name is
        // NUL-terminated.
        let address = unsafe { GetProcAddress(handle as *mut c_void, bytes.as_ptr()) };
        (!address.is_null()).then_some(address as usize)
    }

    pub fn missing_library(name: &str) -> String {
        format!("the library \"{name}\" could not be loaded")
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod platform {
    use super::{Piece, given_back};
    use crate::value::Value;
    use sayon_check::types::{Eightbyte, Type, TypeDef, sysv_eightbytes};
    use std::ffi::{CStr, c_char, c_void};

    unsafe extern "C" {
        fn dlopen(file: *const c_char, mode: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
        fn dlerror() -> *const c_char;
        fn sayon_interp_call_sysv(
            function: usize,
            integers: *const u64,
            floats: *const u64,
            stack: *const u64,
            count: usize,
            out: *mut u64,
        );
        fn sayon_interp_callback_entries();
    }

    const RTLD_NOW: i32 = 2;

    // System V on x86-64: integers in RDI, RSI, RDX, RCX, R8, and R9, floats
    // in XMM0 to XMM7, and the rest on the stack, which is 16-byte aligned
    // at the call. AL tells a variadic function how many vector registers
    // may hold arguments; eight is always enough. The results come from RAX
    // and RDX, and XMM0 and XMM1.
    std::arch::global_asm!(
        ".text",
        ".p2align 4",
        ".globl sayon_interp_call_sysv",
        "sayon_interp_call_sysv:",
        "push rbp",
        "mov rbp, rsp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "mov rbx, rdi",
        "mov r12, rsi",
        "mov r13, rdx",
        "mov r14, r9",
        "lea rax, [r8*8 + 15]",
        "and rax, -16",
        "sub rsp, rax",
        "xor r10, r10",
        "2:",
        "cmp r10, r8",
        "jae 3f",
        "mov r11, qword ptr [rcx + r10*8]",
        "mov qword ptr [rsp + r10*8], r11",
        "inc r10",
        "jmp 2b",
        "3:",
        "movq xmm0, qword ptr [r13]",
        "movq xmm1, qword ptr [r13 + 8]",
        "movq xmm2, qword ptr [r13 + 16]",
        "movq xmm3, qword ptr [r13 + 24]",
        "movq xmm4, qword ptr [r13 + 32]",
        "movq xmm5, qword ptr [r13 + 40]",
        "movq xmm6, qword ptr [r13 + 48]",
        "movq xmm7, qword ptr [r13 + 56]",
        "mov rdi, qword ptr [r12]",
        "mov rsi, qword ptr [r12 + 8]",
        "mov rdx, qword ptr [r12 + 16]",
        "mov rcx, qword ptr [r12 + 24]",
        "mov r8, qword ptr [r12 + 32]",
        "mov r9, qword ptr [r12 + 40]",
        "mov eax, 8",
        "call rbx",
        "mov qword ptr [r14], rax",
        "mov qword ptr [r14 + 8], rdx",
        "movq qword ptr [r14 + 16], xmm0",
        "movq qword ptr [r14 + 24], xmm1",
        "lea rsp, [rbp - 32]",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    );

    /// Calls `function` with the pieces in the registers and stack words the
    /// convention gives them.
    ///
    /// # Safety
    /// `function` must take arguments matching the pieces.
    pub(super) unsafe fn invoke(
        function: usize,
        pieces: &[Piece],
        ret: &Type,
        types: &[TypeDef],
    ) -> Value {
        let mut integers: Vec<u64> = Vec::new();
        let mut floats: Vec<u64> = Vec::new();
        let mut stack: Vec<u64> = Vec::new();
        let mut memory: Option<Vec<u64>> = None;
        let returned = match ret {
            Type::Named(_) => sysv_eightbytes(types, ret, sayon_target::host()),
            _ => None,
        };
        if matches!(ret, Type::Named(_)) && returned.is_none() {
            let (size, _) = sayon_check::types::c_size_align(types, ret, sayon_target::host());
            let words = vec![0u64; size.div_ceil(8) as usize];
            integers.push(words.as_ptr() as u64);
            memory = Some(words);
        }
        for piece in pieces {
            match piece {
                Piece::Integer(word) if integers.len() < 6 => integers.push(*word),
                Piece::Float(word) if floats.len() < 8 => floats.push(*word),
                Piece::Integer(word) | Piece::Float(word) => stack.push(*word),
                Piece::Structure(words, ty) => {
                    match sysv_eightbytes(types, ty, sayon_target::host()) {
                        Some(classes)
                            if integers.len()
                                + classes.iter().filter(|c| **c == Eightbyte::Integer).count()
                                <= 6
                                && floats.len()
                                    + classes.iter().filter(|c| **c == Eightbyte::Sse).count()
                                    <= 8 =>
                        {
                            for (class, word) in classes.iter().zip(words) {
                                match class {
                                    Eightbyte::Integer => integers.push(*word),
                                    Eightbyte::Sse => floats.push(*word),
                                }
                            }
                        }
                        // In memory, or without registers enough for all of it.
                        _ => stack.extend_from_slice(words),
                    }
                }
            }
        }
        integers.resize(6, 0);
        floats.resize(8, 0);
        let mut out = [0u64; 4];
        // SAFETY: the routine reads six integers, eight floats, and
        // `stack.len()` stack words, and writes four words.
        unsafe {
            sayon_interp_call_sysv(
                function,
                integers.as_ptr(),
                floats.as_ptr(),
                stack.as_ptr(),
                stack.len(),
                out.as_mut_ptr(),
            );
            given_back(
                ret,
                [out[0], out[1]],
                [out[2], out[3]],
                memory.as_deref(),
                types,
                returned,
            )
        }
    }

    /// The entry points C can call back.
    pub const CALLBACK_ENTRIES: usize = 64;

    /// Entry points 16 bytes apart, each putting its number in EAX before
    /// the routine they share, which stores the integer and float argument
    /// registers beside each other and passes them with the address of the
    /// arguments on the stack; it gives the result in both RAX and XMM0.
    macro_rules! callback_entries {
        ($($number:literal)*) => {
            std::arch::global_asm!(
                ".text",
                ".p2align 4",
                ".globl sayon_interp_callback_entries",
                "sayon_interp_callback_entries:",
                $(
                    ".p2align 4",
                    concat!("mov eax, ", stringify!($number)),
                    "jmp sayon_interp_callback_common",
                )*
                ".p2align 4",
                "sayon_interp_callback_common:",
                "push rbp",
                "mov rbp, rsp",
                "sub rsp, 112",
                "mov qword ptr [rsp], rdi",
                "mov qword ptr [rsp + 8], rsi",
                "mov qword ptr [rsp + 16], rdx",
                "mov qword ptr [rsp + 24], rcx",
                "mov qword ptr [rsp + 32], r8",
                "mov qword ptr [rsp + 40], r9",
                "movq qword ptr [rsp + 48], xmm0",
                "movq qword ptr [rsp + 56], xmm1",
                "movq qword ptr [rsp + 64], xmm2",
                "movq qword ptr [rsp + 72], xmm3",
                "movq qword ptr [rsp + 80], xmm4",
                "movq qword ptr [rsp + 88], xmm5",
                "movq qword ptr [rsp + 96], xmm6",
                "movq qword ptr [rsp + 104], xmm7",
                "mov edi, eax",
                "mov rsi, rsp",
                "lea rdx, [rsp + 48]",
                "lea rcx, [rbp + 16]",
                "call sayon_interp_called_back",
                "movq xmm0, rax",
                "leave",
                "ret",
            );
        };
    }

    callback_entries!(
        0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31
        32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60
        61 62 63
    );

    #[unsafe(no_mangle)]
    unsafe extern "C" fn sayon_interp_called_back(
        number: u32,
        integers: *const u64,
        floats: *const u64,
        stack: *const u64,
    ) -> u64 {
        // SAFETY: the shared routine passes the six integer and eight float
        // argument registers, and the arguments on the stack, which take
        // the convention's order.
        unsafe {
            super::called_back(number as usize, |params| {
                let (mut integer, mut float, mut word) = (0, 0, 0);
                params
                    .iter()
                    .map(|ty| match ty {
                        Type::Float(_) if float < 8 => {
                            float += 1;
                            *floats.add(float - 1)
                        }
                        Type::Float(_) => {
                            word += 1;
                            *stack.add(word - 1)
                        }
                        _ if integer < 6 => {
                            integer += 1;
                            *integers.add(integer - 1)
                        }
                        _ => {
                            word += 1;
                            *stack.add(word - 1)
                        }
                    })
                    .collect()
            })
        }
    }

    pub fn callback_entry(number: usize) -> usize {
        sayon_interp_callback_entries as *const () as usize + 16 * number
    }

    /// The files a library name stands for: the C runtime is the C library
    /// and its math functions'.
    pub fn library_files(name: &str) -> Vec<String> {
        match name {
            "c" => vec!["libc.so.6".to_string(), "libm.so.6".to_string()],
            name if name.contains(".so") || name.contains('/') => vec![name.to_string()],
            name => vec![format!("lib{name}.so"), format!("lib{name}.so.6")],
        }
    }

    pub fn open(file: &str) -> Option<usize> {
        let name = std::ffi::CString::new(file).ok()?;
        // SAFETY: a NUL-terminated name.
        let handle = unsafe { dlopen(name.as_ptr(), RTLD_NOW) };
        (!handle.is_null()).then_some(handle as usize)
    }

    pub fn symbol(handle: usize, name: &str) -> Option<usize> {
        let name = std::ffi::CString::new(name).ok()?;
        // SAFETY: a loaded library and a NUL-terminated name.
        let address = unsafe { dlsym(handle as *mut c_void, name.as_ptr()) };
        (!address.is_null()).then_some(address as usize)
    }

    pub fn missing_library(name: &str) -> String {
        // SAFETY: dlerror gives null or the text of the last failure.
        let reason = unsafe {
            let text = dlerror();
            if text.is_null() {
                String::new()
            } else {
                format!(": {}", CStr::from_ptr(text).to_string_lossy())
            }
        };
        format!("the library \"{name}\" could not be loaded{reason}")
    }
}

#[cfg(not(any(
    all(windows, target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64")
)))]
mod platform {
    pub const CALLBACK_ENTRIES: usize = 0;

    pub fn callback_entry(_number: usize) -> usize {
        unreachable!("no library opens on this platform")
    }

    pub unsafe fn call(_function: usize, _slots: &[u64]) -> (u64, u64) {
        unreachable!("no library opens on this platform")
    }

    pub fn library_files(_name: &str) -> Vec<String> {
        Vec::new()
    }

    pub fn open(_file: &str) -> Option<usize> {
        None
    }

    pub fn symbol(_handle: usize, _name: &str) -> Option<usize> {
        None
    }

    pub fn missing_library(_name: &str) -> String {
        "the interpreter calls C functions on Windows x64 so far; build the program with `sayon build` to call them here".to_string()
    }
}
