//! Host primitives used by the standard library modules.
//!
//! Each primitive is implemented once here, over plain Rust values. The
//! interpreter calls [`invoke`] directly; compiled programs reach the same
//! code through `sayon_host_call`, so both execution modes behave the same.
//! Standard library modules call a primitive as `host_<name>(...)`; other
//! modules cannot see them.

use std::io::{BufRead, Write};
use std::sync::Mutex;

/// The value types that cross the host boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostType {
    String,
    Integer,
    Float,
    Boolean,
    Nothing,
    StringArray,
    /// `string or nothing`.
    OptionalString,
    /// An open file of `standard/io`, `HostStream` in the library's code.
    Stream,
    /// `Array<byte>`.
    Bytes,
    /// `unsigned` (64 bits), for results only.
    Unsigned,
    /// A raw address, `pointer`.
    Pointer,
    /// An owned NUL-terminated buffer, `c_string`; passed as its address.
    CString,
}

macro_rules! hosts {
    ($($variant:ident = $name:literal ($($param:ident),*) -> $result:ident;)*) => {
        /// A host primitive. The discriminant is the code passed to
        /// `sayon_host_call`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u32)]
        pub enum Host { $($variant),* }

        impl Host {
            pub const ALL: &'static [Host] = &[$(Host::$variant),*];

            /// The name after the `host_` prefix.
            pub fn name(self) -> &'static str {
                match self { $(Host::$variant => $name),* }
            }

            pub fn params(self) -> &'static [HostType] {
                match self { $(Host::$variant => &[$(HostType::$param),*]),* }
            }

            pub fn result(self) -> HostType {
                match self { $(Host::$variant => HostType::$result),* }
            }
        }
    };
}

hosts! {
    ReadLine = "read_line"() -> OptionalString;
    Write = "write"(String) -> Nothing;
    WriteError = "write_error"(String) -> Nothing;
    AppendFile = "append_file"(String, String) -> Nothing;
    Arguments = "arguments"() -> StringArray;
    Environment = "environment"(String) -> OptionalString;
    Platform = "platform"() -> String;
    PathKind = "path_kind"(String) -> String;
    FileSize = "file_size"(String) -> Integer;
    ListDirectory = "list_directory"(String) -> StringArray;
    CreateDirectory = "create_directory"(String) -> Nothing;
    Remove = "remove"(String) -> Nothing;
    Rename = "rename"(String, String) -> Nothing;
    CopyFile = "copy_file"(String, String) -> Nothing;
    CurrentDirectory = "current_directory"() -> String;
    Split = "split"(String, String) -> StringArray;
    SplitLines = "split_lines"(String) -> StringArray;
    Join = "join"(StringArray, String) -> String;
    Trim = "trim"(String) -> String;
    Upper = "upper"(String) -> String;
    Lower = "lower"(String) -> String;
    Replace = "replace"(String, String, String) -> String;
    StartsWith = "starts_with"(String, String) -> Boolean;
    EndsWith = "ends_with"(String, String) -> Boolean;
    Contains = "contains"(String, String) -> Boolean;
    Find = "find"(String, String) -> Integer;
    Repeat = "repeat"(String, Integer) -> String;
    Substring = "substring"(String, Integer, Integer) -> String;
    Characters = "characters"(String) -> StringArray;
    Code = "code"(String) -> Integer;
    Character = "character"(Integer) -> String;
    ParseError = "parse_error"(String) -> Nothing;
    Monotonic = "monotonic"() -> Integer;
    UnixTime = "unix_time"() -> Integer;
    Sqrt = "sqrt"(Float) -> Float;
    Sin = "sin"(Float) -> Float;
    Cos = "cos"(Float) -> Float;
    Tan = "tan"(Float) -> Float;
    Asin = "asin"(Float) -> Float;
    Acos = "acos"(Float) -> Float;
    Atan = "atan"(Float) -> Float;
    Atan2 = "atan2"(Float, Float) -> Float;
    Exp = "exp"(Float) -> Float;
    Log = "log"(Float) -> Float;
    Log10 = "log10"(Float) -> Float;
    Log2 = "log2"(Float) -> Float;
    Pow = "pow"(Float, Float) -> Float;
    Floor = "floor"(Float) -> Float;
    Ceil = "ceil"(Float) -> Float;
    Round = "round"(Float) -> Float;
    Trunc = "trunc"(Float) -> Float;
    Random = "random"() -> Float;
    Open = "open"(String, String) -> Stream;
    StreamReadLine = "stream_read_line"(Stream, Integer) -> OptionalString;
    StreamRead = "stream_read"(Stream) -> String;
    StreamReadBytes = "stream_read_bytes"(Stream, Integer) -> Bytes;
    StreamWrite = "stream_write"(Stream, String) -> Nothing;
    StreamWriteBytes = "stream_write_bytes"(Stream, Bytes) -> Nothing;
    StreamFlush = "stream_flush"(Stream) -> Nothing;
    StreamPosition = "stream_position"(Stream) -> Integer;
    StreamSeek = "stream_seek"(Stream, Integer) -> Nothing;
    StreamClose = "stream_close"(Stream) -> Nothing;
    StreamShare = "stream_share"(Stream) -> Stream;
    StreamCopy = "stream_copy"(Stream, Stream) -> Integer;
    ReadFileBytes = "read_file_bytes"(String) -> Bytes;
    WriteFileBytes = "write_file_bytes"(String, Bytes) -> Nothing;
    Exit = "exit"(Integer) -> Nothing;
    Walk = "walk"(String) -> StringArray;
    IsLink = "is_link"(String) -> Boolean;
    StandardInput = "standard_input"() -> Stream;
    Run = "run"(String, StringArray, String) -> StringArray;
    JsonInteger = "json_integer"(String, String, Integer) -> Integer;
    JsonUnsigned = "json_unsigned"(String, String, Integer) -> Unsigned;
    JsonFloat = "json_float"(String, String, Integer) -> Float;
    JsonIntegral = "json_integral"(String) -> Boolean;
    JsonFloatText = "json_float_text"(Float, Integer) -> String;
    JsonFail = "json_fail"(String) -> Nothing;
    Graphemes = "graphemes"(String) -> StringArray;
    GraphemeCount = "grapheme_count"(String) -> Integer;
    Bytes = "bytes"(String) -> Bytes;
    FromBytes = "from_bytes"(Bytes) -> String;
    FromBytesLossy = "from_bytes_lossy"(Bytes) -> String;
    CString = "c_string"(String) -> CString;
    CStringAddress = "c_string_address"(CString) -> Pointer;
    ReadCString = "read_c_string"(Pointer) -> String;
    ReadBytes = "read_bytes"(Pointer, Integer) -> Bytes;
    WriteBytes = "write_bytes"(Pointer, Bytes) -> Nothing;
    PointerOffset = "pointer_offset"(Pointer, Integer) -> Pointer;
}

/// The code of the error `os.exit` raises to end the program. `catch` and
/// `try` let it pass, so the program ends after its `finally` blocks and
/// cleanup; the entry boundary turns it into the exit status, which is the
/// last word of its message.
pub const EXIT_CODE: &str = "Exit";

/// The code of the error a cancelled task stops with where it waits. Like
/// `Exit`, `catch` and `try` let it pass, so the task unwinds in order.
pub const CANCELLED_CODE: &str = "Cancelled";

/// The exit status an `Exit` error carries.
pub fn exit_status(message: &str) -> i32 {
    message
        .rsplit(' ')
        .next()
        .and_then(|word| word.parse().ok())
        .unwrap_or(1)
}

impl Host {
    /// Finds a primitive by the name a standard library module calls it by,
    /// for example `host_read_line`.
    pub fn lookup(name: &str) -> Option<Host> {
        let bare = name.strip_prefix("host_")?;
        Host::ALL.iter().copied().find(|host| host.name() == bare)
    }

    pub fn from_code(code: u32) -> Option<Host> {
        Host::ALL.get(code as usize).copied()
    }
}

/// An argument passed to a primitive.
#[derive(Clone, Debug)]
pub enum HostArg<'a> {
    String(&'a str),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Strings(Vec<&'a str>),
    Stream(i64),
    Bytes(Vec<u8>),
    /// An address: a `pointer`, or where a `c_string`'s bytes are.
    Pointer(usize),
}

/// A primitive's result.
#[derive(Clone, Debug, PartialEq)]
pub enum HostValue {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Nothing,
    Strings(Vec<String>),
    OptionalString(Option<String>),
    /// A new stream, with one reference.
    Stream(i64),
    Bytes(Vec<u8>),
    Unsigned(u64),
    Pointer(usize),
    /// The bytes of a new C string, without the NUL that ends it.
    CString(Vec<u8>),
}

/// A recoverable error: code and message.
pub type HostError = (&'static str, String);

static ARGUMENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Sets the arguments a program sees through `os.arguments()`.
pub fn set_arguments(arguments: Vec<String>) {
    *ARGUMENTS.lock().unwrap_or_else(|e| e.into_inner()) = arguments;
}

pub(crate) fn io_error(action: &str, path: &str, error: &std::io::Error) -> HostError {
    let reason = match error.kind() {
        std::io::ErrorKind::NotFound => "it does not exist".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        std::io::ErrorKind::AlreadyExists => "it already exists".to_string(),
        std::io::ErrorKind::DirectoryNotEmpty => "the directory is not empty".to_string(),
        _ => error.to_string(),
    };
    (
        "IOError",
        format!("could not {action} \"{path}\": {reason}"),
    )
}

fn string<'a>(args: &[HostArg<'a>], index: usize) -> &'a str {
    match args.get(index) {
        Some(HostArg::String(text)) => text,
        other => panic!("host argument {index} should be a string, found {other:?}"),
    }
}

fn integer(args: &[HostArg], index: usize) -> i64 {
    match args.get(index) {
        Some(HostArg::Integer(value)) => *value,
        other => panic!("host argument {index} should be an integer, found {other:?}"),
    }
}

fn stream(args: &[HostArg], index: usize) -> i64 {
    match args.get(index) {
        Some(HostArg::Stream(id)) => *id,
        other => panic!("host argument {index} should be a stream, found {other:?}"),
    }
}

fn bytes<'b>(args: &'b [HostArg], index: usize) -> &'b [u8] {
    match args.get(index) {
        Some(HostArg::Bytes(bytes)) => bytes,
        other => panic!("host argument {index} should be bytes, found {other:?}"),
    }
}

fn pointer(args: &[HostArg], index: usize) -> usize {
    match args.get(index) {
        Some(HostArg::Pointer(address)) => *address,
        other => panic!("host argument {index} should be a pointer, found {other:?}"),
    }
}

/// The address of argument `index`, or the error for `null_pointer`.
fn non_null(args: &[HostArg], index: usize, action: &str) -> Result<usize, HostError> {
    match pointer(args, index) {
        0 => Err(("NullPointerError", format!("cannot {action} null_pointer"))),
        address => Ok(address),
    }
}

fn float(args: &[HostArg], index: usize) -> f64 {
    match args.get(index) {
        Some(HostArg::Float(value)) => *value,
        other => panic!("host argument {index} should be a float, found {other:?}"),
    }
}

/// Converts a character index of `text` to a byte offset, if it is within
/// `0..=character count`.
fn char_offset(text: &str, index: i64) -> Option<usize> {
    if index < 0 {
        return None;
    }
    let index = index as usize;
    if index == 0 {
        return Some(0);
    }
    match text.char_indices().nth(index) {
        Some((offset, _)) => Some(offset),
        None if text.chars().count() == index => Some(text.len()),
        None => None,
    }
}

fn random() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static STATE: Mutex<u64> = Mutex::new(0);
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if *state == 0 {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        *state = seed | 1;
    }
    // xorshift64*: not suitable for security, as documented.
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    let bits = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
    bits as f64 / (1u64 << 53) as f64
}

/// The entries of a directory, sorted by name, as paths relative to the
/// directory walked (`prefix`), with whether each is a directory to enter.
/// A symbolic link is never entered, even when it points to a directory.
fn walk_entries(
    directory: &std::path::Path,
    prefix: &str,
) -> Result<Vec<(String, std::path::PathBuf, bool)>, HostError> {
    let shown = directory.display().to_string();
    let entries = std::fs::read_dir(directory).map_err(|e| io_error("list", &shown, &e))?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error("list", &shown, &e))?;
        let name = entry.file_name().into_string().map_err(|raw| {
            (
                "EncodingError",
                format!(
                    "\"{}\" in \"{shown}\" is not valid Unicode",
                    raw.to_string_lossy()
                ),
            )
        })?;
        let kind = entry
            .file_type()
            .map_err(|e| io_error("inspect", &entry.path().display().to_string(), &e))?;
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        found.push((relative, entry.path(), kind.is_dir()));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Whether JSON number text is an integer: no fraction and no exponent.
fn json_integral(digits: &str) -> bool {
    !digits.contains(['.', 'e', 'E'])
}

/// A JSON number read as an integer of the type with `code` (see
/// `text::INTEGER_TYPE_NAMES`), or the error to report at `path`.
fn json_integer(digits: &str, path: &str, code: i64) -> Result<i128, HostError> {
    let name = crate::text::INTEGER_TYPE_NAMES[code as usize];
    if !json_integral(digits) {
        return Err((
            "DecodeError",
            format!("{path}: expected {name}, found the number {digits}"),
        ));
    }
    let (low, high) = crate::text::integer_type_range(code as u32);
    match digits.parse::<i128>() {
        Ok(value) if (low..=high).contains(&value) => Ok(value),
        _ => Err((
            "DecodeError",
            format!("{path}: {digits} is outside the range of {name}, {low} to {high}"),
        )),
    }
}

/// Runs a program with an argument vector, without a shell, and waits for
/// it: its status, standard output, and standard error, as three strings.
/// Its standard input is empty.
fn run(program: &str, arguments: &HostArg, directory: &str) -> Result<HostValue, HostError> {
    let HostArg::Strings(arguments) = arguments else {
        panic!("run expects strings, found {arguments:?}")
    };
    let _ = std::io::stdout().flush();
    let mut command = std::process::Command::new(program);
    command.args(arguments).stdin(std::process::Stdio::null());
    if !directory.is_empty() {
        command.current_dir(directory);
    }
    let output = command
        .output()
        .map_err(|error| io_error("run", program, &error))?;
    let text = |bytes: Vec<u8>, what: &str| {
        String::from_utf8(bytes).map_err(|_| {
            (
                "EncodingError",
                format!("the {what} of \"{program}\" is not valid UTF-8"),
            )
        })
    };
    let status = output.status.code().map_or(-1, i64::from);
    Ok(HostValue::Strings(vec![
        status.to_string(),
        text(output.stdout, "output")?,
        text(output.stderr, "error output")?,
    ]))
}

/// Runs a primitive. Output primitives write to the process's standard
/// streams; the interpreter routes `Write` through its own output instead so
/// it stays ordered with `print`.
pub fn invoke(host: Host, args: &[HostArg]) -> Result<HostValue, HostError> {
    use HostValue as V;
    let unary = |f: fn(f64) -> f64| Ok(V::Float(f(float(args, 0))));
    match host {
        Host::ReadLine => {
            let _ = std::io::stdout().flush();
            let mut bytes = Vec::new();
            let count = std::io::stdin()
                .lock()
                .read_until(b'\n', &mut bytes)
                .map_err(|error| ("IOError", format!("could not read standard input: {error}")))?;
            if count == 0 {
                return Ok(V::OptionalString(None));
            }
            if bytes.ends_with(b"\n") {
                bytes.pop();
                if bytes.ends_with(b"\r") {
                    bytes.pop();
                }
            }
            String::from_utf8(bytes)
                .map(|line| V::OptionalString(Some(line)))
                .map_err(|_| {
                    (
                        "EncodingError",
                        "standard input is not valid UTF-8".to_string(),
                    )
                })
        }
        Host::Write => {
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(string(args, 0).as_bytes())
                .and_then(|_| stdout.flush())
                .map_err(|error| ("IOError", format!("could not write output: {error}")))?;
            Ok(V::Nothing)
        }
        Host::WriteError => {
            let _ = std::io::stdout().flush();
            let mut stderr = std::io::stderr().lock();
            stderr
                .write_all(string(args, 0).as_bytes())
                .map_err(|error| ("IOError", format!("could not write error output: {error}")))?;
            Ok(V::Nothing)
        }
        Host::AppendFile => {
            let path = string(args, 0);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| io_error("open", path, &e))?;
            file.write_all(string(args, 1).as_bytes())
                .map_err(|e| io_error("write", path, &e))?;
            Ok(V::Nothing)
        }
        Host::Open => crate::streams::open(string(args, 0), string(args, 1)).map(V::Stream),
        Host::StreamReadLine => {
            crate::streams::read_line(stream(args, 0), integer(args, 1)).map(V::OptionalString)
        }
        Host::StreamRead => crate::streams::read_rest(stream(args, 0)).map(V::String),
        Host::StreamReadBytes => {
            crate::streams::read_bytes(stream(args, 0), integer(args, 1)).map(V::Bytes)
        }
        Host::StreamWrite => {
            crate::streams::write(stream(args, 0), string(args, 1).as_bytes()).map(|()| V::Nothing)
        }
        Host::StreamWriteBytes => {
            crate::streams::write(stream(args, 0), bytes(args, 1)).map(|()| V::Nothing)
        }
        Host::StreamFlush => crate::streams::flush(stream(args, 0)).map(|()| V::Nothing),
        Host::StreamPosition => crate::streams::position(stream(args, 0)).map(V::Integer),
        Host::StreamSeek => {
            crate::streams::seek(stream(args, 0), integer(args, 1)).map(|()| V::Nothing)
        }
        Host::StreamClose => crate::streams::close(stream(args, 0)).map(|()| V::Nothing),
        Host::StreamShare => {
            let id = stream(args, 0);
            crate::streams::share(id);
            Ok(V::Stream(id))
        }
        Host::StreamCopy => crate::streams::copy(stream(args, 0), stream(args, 1)).map(V::Integer),
        Host::ReadFileBytes => {
            let path = string(args, 0);
            std::fs::read(path)
                .map(V::Bytes)
                .map_err(|e| io_error("read", path, &e))
        }
        Host::WriteFileBytes => {
            let path = string(args, 0);
            std::fs::write(path, bytes(args, 1))
                .map(|()| V::Nothing)
                .map_err(|e| io_error("write", path, &e))
        }
        Host::StandardInput => Ok(V::Stream(crate::streams::standard_input())),
        Host::Run => run(string(args, 0), &args[1], string(args, 2)),
        Host::JsonInteger => json_integer(string(args, 0), string(args, 1), integer(args, 2))
            .map(|value| V::Integer(value as i64)),
        Host::JsonUnsigned => json_integer(string(args, 0), string(args, 1), integer(args, 2))
            .map(|value| V::Unsigned(value as u64)),
        Host::JsonFloat => {
            let (digits, path) = (string(args, 0), string(args, 1));
            let value = if integer(args, 2) == 32 {
                digits.parse::<f32>().map(f64::from)
            } else {
                digits.parse::<f64>()
            };
            match value {
                Ok(value) if value.is_finite() => Ok(V::Float(value)),
                _ => Err((
                    "DecodeError",
                    format!("{path}: {digits} is too large for a float"),
                )),
            }
        }
        Host::JsonIntegral => Ok(V::Boolean(json_integral(string(args, 0)))),
        Host::JsonFloatText => {
            let value = float(args, 0);
            if !value.is_finite() {
                return Err((
                    "ValueError",
                    format!("JSON has no number for {}", crate::text::format_f64(value)),
                ));
            }
            Ok(V::String(if integer(args, 1) == 32 {
                crate::text::format_f32(value as f32)
            } else {
                crate::text::format_f64(value)
            }))
        }
        Host::JsonFail => Err(("DecodeError", string(args, 0).to_string())),
        Host::Graphemes => Ok(V::Strings(
            crate::graphemes::clusters(string(args, 0))
                .into_iter()
                .map(str::to_string)
                .collect(),
        )),
        Host::GraphemeCount => Ok(V::Integer(crate::graphemes::count(string(args, 0)) as i64)),
        Host::Bytes => Ok(V::Bytes(string(args, 0).as_bytes().to_vec())),
        Host::FromBytes => {
            let bytes = bytes(args, 0);
            String::from_utf8(bytes.to_vec())
                .map(V::String)
                .map_err(|error| {
                    (
                        "EncodingError",
                        format!(
                            "the bytes are not valid UTF-8 from byte {}",
                            error.utf8_error().valid_up_to()
                        ),
                    )
                })
        }
        Host::FromBytesLossy => Ok(V::String(
            String::from_utf8_lossy(bytes(args, 0)).into_owned(),
        )),
        Host::CString => {
            let text = string(args, 0);
            match text.find('\0') {
                Some(position) => Err((
                    "ValueError",
                    format!(
                        "the text holds a NUL character at byte {position}, which C would take for its end"
                    ),
                )),
                None => Ok(V::CString(text.as_bytes().to_vec())),
            }
        }
        Host::CStringAddress => Ok(V::Pointer(pointer(args, 0))),
        // The raw accesses below are reached only from `unsafe` functions
        // of `standard/foreign`, whose callers vouch for the memory.
        Host::ReadCString => {
            let address = non_null(args, 0, "read text from")?;
            // SAFETY: unsafe code promised a NUL-terminated buffer here.
            let bytes =
                unsafe { std::ffi::CStr::from_ptr(address as *const std::ffi::c_char) }.to_bytes();
            String::from_utf8(bytes.to_vec())
                .map(V::String)
                .map_err(|error| {
                    (
                        "EncodingError",
                        format!(
                            "the C string is not valid UTF-8 from byte {}",
                            error.utf8_error().valid_up_to()
                        ),
                    )
                })
        }
        Host::ReadBytes => {
            let count = integer(args, 1);
            if count < 0 {
                return Err(("ValueError", format!("cannot read {count} bytes")));
            }
            if count == 0 {
                return Ok(V::Bytes(Vec::new()));
            }
            let address = non_null(args, 0, "read from")?;
            // SAFETY: unsafe code promised `count` readable bytes here.
            let bytes = unsafe { std::slice::from_raw_parts(address as *const u8, count as usize) };
            Ok(V::Bytes(bytes.to_vec()))
        }
        Host::WriteBytes => {
            let data = bytes(args, 1);
            if data.is_empty() {
                return Ok(V::Nothing);
            }
            let address = non_null(args, 0, "write to")?;
            // SAFETY: unsafe code promised room for the bytes here.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), address as *mut u8, data.len()) };
            Ok(V::Nothing)
        }
        Host::PointerOffset => Ok(V::Pointer(
            pointer(args, 0).wrapping_add_signed(integer(args, 1) as isize),
        )),
        Host::Walk => {
            let mut stack = walk_entries(std::path::Path::new(string(args, 0)), "")?;
            stack.reverse();
            let mut found = Vec::new();
            while let Some((relative, path, directory)) = stack.pop() {
                if directory {
                    let mut inside = walk_entries(&path, &relative)?;
                    inside.reverse();
                    stack.extend(inside);
                }
                found.push(relative);
            }
            Ok(V::Strings(found))
        }
        Host::IsLink => Ok(V::Boolean(
            std::fs::symlink_metadata(string(args, 0))
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false),
        )),
        Host::Exit => {
            let status = integer(args, 0);
            if !(0..=255).contains(&status) {
                return Err((
                    "ValueError",
                    format!("{status} is not a portable exit status; use 0 to 255"),
                ));
            }
            Err((EXIT_CODE, format!("the program exits with status {status}")))
        }
        Host::Arguments => Ok(V::Strings(
            ARGUMENTS.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        )),
        Host::Environment => Ok(V::OptionalString(std::env::var(string(args, 0)).ok())),
        Host::Platform => Ok(V::String(
            if cfg!(windows) {
                "windows"
            } else if cfg!(target_os = "macos") {
                "macos"
            } else if cfg!(target_os = "android") {
                "android"
            } else {
                "linux"
            }
            .to_string(),
        )),
        Host::PathKind => {
            let kind = match std::fs::metadata(string(args, 0)) {
                Ok(metadata) if metadata.is_file() => "file",
                Ok(metadata) if metadata.is_dir() => "directory",
                Ok(_) => "other",
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing",
                Err(_) => "other",
            };
            Ok(V::String(kind.to_string()))
        }
        Host::FileSize => {
            let path = string(args, 0);
            let metadata = std::fs::metadata(path).map_err(|e| io_error("inspect", path, &e))?;
            Ok(V::Integer(metadata.len() as i64))
        }
        Host::ListDirectory => {
            let path = string(args, 0);
            let entries = std::fs::read_dir(path).map_err(|e| io_error("list", path, &e))?;
            let mut names = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| io_error("list", path, &e))?;
                match entry.file_name().into_string() {
                    Ok(name) => names.push(name),
                    Err(raw) => {
                        return Err((
                            "EncodingError",
                            format!(
                                "\"{}\" in \"{path}\" is not valid Unicode",
                                raw.to_string_lossy()
                            ),
                        ));
                    }
                }
            }
            names.sort();
            Ok(V::Strings(names))
        }
        Host::CreateDirectory => {
            let path = string(args, 0);
            std::fs::create_dir_all(path).map_err(|e| io_error("create", path, &e))?;
            Ok(V::Nothing)
        }
        Host::Remove => {
            let path = string(args, 0);
            let metadata =
                std::fs::symlink_metadata(path).map_err(|e| io_error("remove", path, &e))?;
            let result = if metadata.is_dir() {
                std::fs::remove_dir(path)
            } else {
                std::fs::remove_file(path)
            };
            result.map_err(|e| io_error("remove", path, &e))?;
            Ok(V::Nothing)
        }
        Host::Rename => {
            let (from, to) = (string(args, 0), string(args, 1));
            std::fs::rename(from, to).map_err(|e| io_error("rename", from, &e))?;
            Ok(V::Nothing)
        }
        Host::CopyFile => {
            let (from, to) = (string(args, 0), string(args, 1));
            std::fs::copy(from, to).map_err(|e| io_error("copy", from, &e))?;
            Ok(V::Nothing)
        }
        Host::CurrentDirectory => {
            let path = std::env::current_dir().map_err(|error| {
                (
                    "IOError",
                    format!("could not read the current directory: {error}"),
                )
            })?;
            path.into_os_string()
                .into_string()
                .map(V::String)
                .map_err(|_| {
                    (
                        "EncodingError",
                        "the current directory's path is not valid Unicode".to_string(),
                    )
                })
        }
        Host::Split => {
            let (text, separator) = (string(args, 0), string(args, 1));
            if separator.is_empty() {
                return Err(("ValueError", "the separator cannot be empty".to_string()));
            }
            Ok(V::Strings(
                text.split(separator).map(str::to_string).collect(),
            ))
        }
        Host::SplitLines => Ok(V::Strings(
            string(args, 0).lines().map(str::to_string).collect(),
        )),
        Host::Join => match &args[0] {
            HostArg::Strings(parts) => Ok(V::String(parts.join(string(args, 1)))),
            other => panic!("join expects strings, found {other:?}"),
        },
        Host::Trim => Ok(V::String(string(args, 0).trim().to_string())),
        Host::Upper => Ok(V::String(string(args, 0).to_uppercase())),
        Host::Lower => Ok(V::String(string(args, 0).to_lowercase())),
        Host::Replace => {
            let (text, from, to) = (string(args, 0), string(args, 1), string(args, 2));
            if from.is_empty() {
                return Err((
                    "ValueError",
                    "the text to replace cannot be empty".to_string(),
                ));
            }
            Ok(V::String(text.replace(from, to)))
        }
        Host::StartsWith => Ok(V::Boolean(string(args, 0).starts_with(string(args, 1)))),
        Host::EndsWith => Ok(V::Boolean(string(args, 0).ends_with(string(args, 1)))),
        Host::Contains => Ok(V::Boolean(string(args, 0).contains(string(args, 1)))),
        Host::Find => {
            let (text, part) = (string(args, 0), string(args, 1));
            Ok(V::Integer(match text.find(part) {
                Some(offset) => text[..offset].chars().count() as i64,
                None => -1,
            }))
        }
        Host::Repeat => {
            let (text, count) = (string(args, 0), integer(args, 1));
            if count < 0 {
                return Err(("ValueError", format!("cannot repeat text {count} times")));
            }
            let total = (text.len() as u128) * (count as u128);
            if total > (isize::MAX as u128) / 2 {
                return Err((
                    "ValueError",
                    "the repeated text would be too large".to_string(),
                ));
            }
            Ok(V::String(text.repeat(count as usize)))
        }
        Host::Substring => {
            let (text, start, end) = (string(args, 0), integer(args, 1), integer(args, 2));
            let length = text.chars().count();
            match (char_offset(text, start), char_offset(text, end)) {
                (Some(from), Some(to)) if from <= to => Ok(V::String(text[from..to].to_string())),
                _ => Err((
                    "BoundsError",
                    format!("characters {start} to {end} are outside a string of length {length}"),
                )),
            }
        }
        Host::Characters => Ok(V::Strings(
            string(args, 0).chars().map(|c| c.to_string()).collect(),
        )),
        Host::Code => {
            let text = string(args, 0);
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Ok(V::Integer(c as i64)),
                _ => Err((
                    "ValueError",
                    format!(
                        "`code` takes one character, but the text has {}",
                        text.chars().count()
                    ),
                )),
            }
        }
        Host::Character => {
            let code = integer(args, 0);
            match u32::try_from(code).ok().and_then(char::from_u32) {
                Some(c) => Ok(V::String(c.to_string())),
                None => Err((
                    "ValueError",
                    format!("{code} is not a Unicode character code"),
                )),
            }
        }
        Host::ParseError => Err(("ParseError", string(args, 0).to_string())),
        Host::Monotonic => {
            // Nanoseconds since the clock was first read in this process.
            static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let start = START.get_or_init(std::time::Instant::now);
            Ok(V::Integer(
                start.elapsed().as_nanos().min(i64::MAX as u128) as i64
            ))
        }
        Host::UnixTime => {
            let now = std::time::SystemTime::now();
            let nanoseconds = match now.duration_since(std::time::UNIX_EPOCH) {
                Ok(after) => after.as_nanos().min(i64::MAX as u128) as i64,
                Err(before) => -(before.duration().as_nanos().min(i64::MAX as u128) as i64),
            };
            Ok(V::Integer(nanoseconds))
        }
        Host::Sqrt => unary(f64::sqrt),
        Host::Sin => unary(f64::sin),
        Host::Cos => unary(f64::cos),
        Host::Tan => unary(f64::tan),
        Host::Asin => unary(f64::asin),
        Host::Acos => unary(f64::acos),
        Host::Atan => unary(f64::atan),
        Host::Atan2 => Ok(V::Float(float(args, 0).atan2(float(args, 1)))),
        Host::Exp => unary(f64::exp),
        Host::Log => unary(f64::ln),
        Host::Log10 => unary(f64::log10),
        Host::Log2 => unary(f64::log2),
        Host::Pow => Ok(V::Float(float(args, 0).powf(float(args, 1)))),
        Host::Floor => unary(f64::floor),
        Host::Ceil => unary(f64::ceil),
        Host::Round => unary(f64::round),
        Host::Trunc => unary(f64::trunc),
        Host::Random => Ok(V::Float(random())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for host in Host::ALL {
            assert_eq!(Host::lookup(&format!("host_{}", host.name())), Some(*host));
            assert_eq!(Host::from_code(*host as u32), Some(*host));
        }
        assert_eq!(Host::lookup("read_line"), None);
    }

    #[test]
    fn string_primitives() {
        let call = |host, args: &[HostArg]| invoke(host, args).unwrap();
        assert_eq!(
            call(
                Host::Split,
                &[HostArg::String("a,b,,c"), HostArg::String(",")]
            ),
            HostValue::Strings(vec!["a".into(), "b".into(), "".into(), "c".into()])
        );
        assert_eq!(
            call(
                Host::Find,
                &[HostArg::String("héllo"), HostArg::String("llo")]
            ),
            HostValue::Integer(2)
        );
        assert_eq!(
            call(
                Host::Substring,
                &[
                    HostArg::String("héllo"),
                    HostArg::Integer(1),
                    HostArg::Integer(3)
                ]
            ),
            HostValue::String("él".into())
        );
        assert!(
            invoke(
                Host::Substring,
                &[
                    HostArg::String("abc"),
                    HostArg::Integer(2),
                    HostArg::Integer(9)
                ]
            )
            .is_err()
        );
        assert_eq!(
            call(
                Host::Join,
                &[HostArg::Strings(vec!["x", "y"]), HostArg::String(" + ")]
            ),
            HostValue::String("x + y".into())
        );
        assert_eq!(
            call(Host::SplitLines, &[HostArg::String("one\r\ntwo\nthree")]),
            HostValue::Strings(vec!["one".into(), "two".into(), "three".into()])
        );
    }

    #[test]
    fn random_is_in_unit_interval() {
        for _ in 0..1000 {
            let HostValue::Float(x) = invoke(Host::Random, &[]).unwrap() else {
                panic!()
            };
            assert!((0.0..1.0).contains(&x));
        }
    }
}
