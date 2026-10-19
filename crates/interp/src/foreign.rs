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

use crate::value::Value;
use sayon_check::types::{FloatType, Type, TypeDef, c_layout, c_size_align, win64_in_register};
use sayon_ir::ForeignFunction;
use std::collections::HashMap;

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

impl Foreign {
    /// Calls the function at `index` of the module's C functions with
    /// `args`, each value with its C type, and gives its result.
    pub fn call(
        &mut self,
        index: u32,
        function: &ForeignFunction,
        args: &[(Value, Type)],
        types: &[TypeDef],
    ) -> Result<Value, ForeignError> {
        let address = self.function(index, function)?;
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
        for (value, ty) in args {
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
        // SAFETY: the address is the named C function, and unsafe code
        // vouched for the declared signature and the arguments.
        let (integer, float) = unsafe { platform::call(address, &slots) };
        if function.library == "c"
            && let Some(flush) = self.flush
        {
            // SAFETY: `fflush(NULL)` flushes every output stream.
            unsafe { platform::call(flush, &[0, 0, 0, 0]) };
        }
        let value = match (&function.ret, given_back) {
            // SAFETY: C wrote the structure into the copy.
            (ty, Some(copy)) => unsafe { read(copies[copy].as_ptr() as usize, ty, types) },
            // SAFETY: the integer holds the structure's bytes.
            (ty @ Type::Named(_), None) => unsafe {
                read(&integer as *const u64 as usize, ty, types)
            },
            (ty, None) => result(ty, integer, float),
        };
        Ok(value)
    }

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
