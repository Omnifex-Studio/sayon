//! Calls of C functions from interpreted programs.
//!
//! A library is loaded the first time one of its functions is called, and
//! each function is looked up by its name. The call itself goes through a
//! small routine written for the target's calling convention: it receives
//! every argument as 64 bits, puts each in the integer register and the
//! float register of its position, and the rest on the stack, so one routine
//! serves every signature, variadic ones included (where Windows x64 wants a
//! float in both). The result comes back from both the integer and the float
//! result register, and the caller keeps the one the signature names.
//!
//! A structure goes as Windows x64 passes it: one of 1, 2, 4, or 8 bytes in
//! its position's register, as an integer, and any other by the address of
//! a copy. One given back in more bytes is written where the address passed
//! before the arguments points.
//!
//! A function value goes to C as one of a fixed set of entry points, one
//! for each parameter of a C function that is a function, which C calls
//! with its convention. An entry point finds the function value in the
//! newest call of its C function, which the call recorded, and runs it on
//! the machine making that call; as in compiled programs, the function
//! cannot wait, and its first failure is kept for the call to raise once C
//! returns, C getting 0 from it and from every later call meanwhile.

use crate::value::{ErrorValue, Ref, Value};
use sayon_check::types::{
    FloatType, FunctionType, Type, TypeDef, c_layout, c_size_align, win64_in_register,
};
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
    let address = target.address;
    // The copies of structures C takes or gives by address, which live
    // through the call.
    let mut copies: Vec<Vec<u64>> = Vec::new();
    let mut slots = Vec::new();
    let given_back = match &function.ret {
        Type::Named(_) => {
            let (size, _) = c_size_align(types, &function.ret);
            (!win64_in_register(size)).then(|| {
                copies.push(vec![0; size.div_ceil(8) as usize]);
                slots.push(copies[copies.len() - 1].as_ptr() as u64);
                copies.len() - 1
            })
        }
        _ => None,
    };
    for (position, (value, ty)) in args.iter().enumerate() {
        if let Type::Function(_) = ty {
            let entry = callback_entry(index, position).map_err(CallFailure::Foreign)?;
            slots.push(entry as u64);
            continue;
        }
        let Type::Named(_) = ty else {
            slots.push(slot(value, ty));
            continue;
        };
        let (size, _) = c_size_align(types, ty);
        let mut copy = vec![0u64; size.div_ceil(8).max(1) as usize];
        // SAFETY: the copy has room for the structure.
        unsafe { write(copy.as_mut_ptr() as usize, value, ty, types) };
        if win64_in_register(size) {
            slots.push(copy[0]);
        } else {
            slots.push(copy.as_ptr() as u64);
            copies.push(copy);
        }
    }
    // The first four positions are always loaded.
    while slots.len() < 4 {
        slots.push(0);
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
    let (integer, float) = unsafe { platform::call(address, &slots) };
    if let Some(flush) = target.flush {
        // SAFETY: `fflush(NULL)` flushes every output stream.
        unsafe { platform::call(flush, &[0, 0, 0, 0]) };
    }
    if calling_back {
        let call = CALLS
            .with(|calls| calls.borrow_mut().pop())
            .expect("the record of the call");
        if let Some(problem) = call.failed {
            return Err(CallFailure::Callback(problem));
        }
    }
    let value = match (&function.ret, given_back) {
        // SAFETY: C wrote the structure into the copy.
        (ty, Some(copy)) => unsafe { read(copies[copy].as_ptr() as usize, ty, types) },
        // SAFETY: the integer holds the structure's bytes.
        (ty @ Type::Named(_), None) => unsafe { read(&integer as *const u64 as usize, ty, types) },
        (ty, None) => result(ty, integer, float),
    };
    Ok(value)
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
/// passed, which `ints` holds as integers (each position's register, then
/// the stack) and `floats` holds for the first four positions as floats.
/// Gives the result's bits for both result registers.
unsafe fn called_back(number: usize, ints: *const u64, floats: *const u64) -> u64 {
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
    let args = signature
        .params
        .iter()
        .enumerate()
        .map(|(i, (_, ty))| {
            // SAFETY: C passed an argument at each position.
            let bits = unsafe {
                match ty {
                    Type::Float(_) if i < 4 => *floats.add(i),
                    _ => *ints.add(i),
                }
            };
            result(ty, bits, bits)
        })
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
                let fields = c_layout(types, named)
                    .fields
                    .iter()
                    .map(|(offset, field)| read(address + *offset as usize, field, types))
                    .collect();
                Value::Record(named.id, fields)
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
                let layout = c_layout(types, named);
                for ((offset, field_ty), field) in layout.fields.iter().zip(fields) {
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
        // SAFETY: the shared routine passes the arguments' words.
        unsafe { super::called_back(number as usize, ints, floats) }
    }

    pub fn callback_entry(number: usize) -> usize {
        sayon_interp_callback_entries as *const () as usize + 16 * number
    }

    /// Calls `function` with the argument slots, at least four; gives the
    /// integer and the float result registers.
    ///
    /// # Safety
    /// `function` must take arguments matching the slots.
    pub unsafe fn call(function: usize, slots: &[u64]) -> (u64, u64) {
        let mut out = [0u64; 2];
        // SAFETY: the routine reads `slots.len()` slots and writes two words.
        unsafe { sayon_interp_call_c(function, slots.as_ptr(), slots.len(), out.as_mut_ptr()) };
        (out[0], out[1])
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

#[cfg(not(all(windows, target_arch = "x86_64")))]
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
