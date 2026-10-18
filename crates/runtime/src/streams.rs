//! Open files, standard input, and sockets, for `standard/io`'s `File` and
//! `standard/network`'s connections.
//!
//! A stream is a number naming an entry of a process-wide table, so that the
//! interpreter and compiled programs share one implementation. Each value
//! holding the number is a reference: `share` adds one (for a file's line
//! iterator), dropping a value removes one, and the file is closed when none
//! are left. `close` closes the file at once and reports what writing the
//! rest of it failed with; the references that remain then refer to a closed
//! stream. Numbers are never reused, so a stale reference can never reach a
//! different file.

use crate::host::{HostError, io_error};
use crate::network::{BLOCKED_CODE, Connecting, Connection, Datagrams, Listener};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::{LazyLock, Mutex, MutexGuard};

enum Handle {
    Reader(BufReader<File>),
    Writer(BufWriter<File>),
    /// Standard input, read through the process's own buffer, which
    /// `io.read_line` shares.
    Input,
    Socket(Socket),
    Closed,
}

enum Socket {
    Connection(Box<Connection>),
    Listener(Listener),
    Datagrams(Datagrams),
    Connecting(Connecting),
}

struct Entry {
    handle: Handle,
    path: String,
    references: usize,
}

#[derive(Default)]
struct Table {
    last: i64,
    entries: HashMap<i64, Entry>,
}

static STREAMS: LazyLock<Mutex<Table>> = LazyLock::new(Mutex::default);

fn table() -> MutexGuard<'static, Table> {
    STREAMS.lock().unwrap_or_else(|e| e.into_inner())
}

fn closed(path: &str) -> HostError {
    ("IOError", format!("\"{path}\" is closed"))
}

fn unpositioned(path: &str) -> HostError {
    ("IOError", format!("{path} has no positions to move to"))
}

/// Runs `f` on the open reader of a stream.
fn reader<T>(
    id: i64,
    f: impl FnOnce(&mut dyn BufRead, &str) -> Result<T, HostError>,
) -> Result<T, HostError> {
    let mut table = table();
    let entry = table
        .entries
        .get_mut(&id)
        .ok_or(("IOError", "the file is closed".to_string()))?;
    match &mut entry.handle {
        Handle::Reader(reader) => f(reader, &entry.path),
        Handle::Input => {
            // What the program wrote shows before it waits for input.
            let _ = std::io::stdout().flush();
            f(&mut std::io::stdin().lock(), &entry.path)
        }
        Handle::Writer(_) => Err((
            "IOError",
            format!("\"{}\" was opened for writing, not reading", entry.path),
        )),
        Handle::Socket(_) => Err(not_connection(&entry.path)),
        Handle::Closed => Err(closed(&entry.path)),
    }
}

fn not_connection(name: &str) -> HostError {
    ("IOError", format!("{name} is not a connection"))
}

/// Runs `f` on the socket of a stream.
fn socket<T>(
    id: i64,
    f: impl FnOnce(&mut Socket, &str) -> Result<T, HostError>,
) -> Result<T, HostError> {
    let mut table = table();
    let entry = table
        .entries
        .get_mut(&id)
        .ok_or(("IOError", "the socket is closed".to_string()))?;
    match &mut entry.handle {
        Handle::Socket(socket) => f(socket, &entry.path),
        Handle::Closed => Err((
            "IOError",
            format!("the socket for {} is closed", entry.path),
        )),
        _ => Err(not_connection(&entry.path)),
    }
}

/// Runs `f` on the connection of a stream, when it is one.
fn connection<T>(
    id: i64,
    f: impl FnOnce(&mut Connection) -> Result<T, HostError>,
) -> Option<Result<T, HostError>> {
    let mut table = table();
    match table.entries.get_mut(&id).map(|entry| &mut entry.handle) {
        Some(Handle::Socket(Socket::Connection(connection))) => Some(f(connection)),
        _ => None,
    }
}

/// Runs `f` on a connection, or fails for a stream that is none.
fn with_connection<T>(
    id: i64,
    f: impl FnOnce(&mut Connection) -> Result<T, HostError>,
) -> Result<T, HostError> {
    socket(id, |socket, name| match socket {
        Socket::Connection(connection) => f(connection),
        _ => Err(not_connection(name)),
    })
}

/// Runs `f` on the open writer of a stream.
fn writer<T>(
    id: i64,
    f: impl FnOnce(&mut BufWriter<File>, &str) -> Result<T, HostError>,
) -> Result<T, HostError> {
    let mut table = table();
    let entry = table
        .entries
        .get_mut(&id)
        .ok_or(("IOError", "the file is closed".to_string()))?;
    match &mut entry.handle {
        Handle::Writer(writer) => f(writer, &entry.path),
        Handle::Reader(_) | Handle::Input => Err((
            "IOError",
            format!("\"{}\" was opened for reading, not writing", entry.path),
        )),
        Handle::Socket(_) => Err(not_connection(&entry.path)),
        Handle::Closed => Err(closed(&entry.path)),
    }
}

fn insert(handle: Handle, path: &str) -> i64 {
    let mut table = table();
    table.last += 1;
    let id = table.last;
    table.entries.insert(
        id,
        Entry {
            handle,
            path: path.to_string(),
            references: 1,
        },
    );
    id
}

/// A stream reading standard input.
pub fn standard_input() -> i64 {
    insert(Handle::Input, "standard input")
}

/// Opens a file: `"r"` to read, `"w"` to write from the start (creating or
/// emptying it), `"a"` to write at its end (creating it when needed).
pub fn open(path: &str, mode: &str) -> Result<i64, HostError> {
    let handle = match mode {
        "r" => Handle::Reader(BufReader::new(
            File::open(path).map_err(|e| io_error("open", path, &e))?,
        )),
        "w" | "a" => {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(mode == "w")
                .append(mode == "a")
                .open(path)
                .map_err(|e| io_error("open", path, &e))?;
            Handle::Writer(BufWriter::new(file))
        }
        _ => {
            return Err((
                "ValueError",
                format!(
                    "\"{mode}\" is not a file mode; use \"r\" to read, \"w\" to write, or \"a\" to add to the end"
                ),
            ));
        }
    };
    Ok(insert(handle, path))
}

fn port(value: i64) -> Result<u16, HostError> {
    u16::try_from(value).map_err(|_| {
        (
            "ValueError",
            format!("{value} is not a port; ports are 0 to 65535"),
        )
    })
}

fn timeout(nanoseconds: i64) -> Result<Option<std::time::Duration>, HostError> {
    match nanoseconds {
        0 => Ok(None),
        n if n < 0 => Err(("ValueError", "a timeout cannot be negative".to_string())),
        n => Ok(Some(std::time::Duration::from_nanos(n as u64))),
    }
}

/// Starts connecting to a host. `connect_finish` waits for the connection,
/// or fails with the reason there is none.
pub fn connect(host: &str, port_number: i64, timeout_ns: i64) -> Result<i64, HostError> {
    let port = port(port_number)?;
    let timeout = timeout(timeout_ns)?;
    let connecting = Connecting::start(host, port, timeout);
    Ok(insert(
        Handle::Socket(Socket::Connecting(connecting)),
        &format!("{host}:{port}"),
    ))
}

pub fn connect_finish(id: i64) -> Result<(), HostError> {
    let mut table = table();
    let entry = table
        .entries
        .get_mut(&id)
        .ok_or(("IOError", "the socket is closed".to_string()))?;
    let Handle::Socket(Socket::Connecting(connecting)) = &entry.handle else {
        return Ok(());
    };
    match connecting.finish() {
        Ok(connection) => {
            entry.handle = Handle::Socket(Socket::Connection(Box::new(connection)));
            Ok(())
        }
        Err(error) => {
            if error.0 != BLOCKED_CODE {
                entry.handle = Handle::Closed;
            }
            Err(error)
        }
    }
}

/// A listener for connections to a port of this computer.
pub fn listen(host: &str, port_number: i64) -> Result<i64, HostError> {
    let listener = Listener::bind(host, port(port_number)?)?;
    Ok(insert(
        Handle::Socket(Socket::Listener(listener)),
        &format!("the listener on {host}:{port_number}"),
    ))
}

/// The next connection made to a listener, as a stream of its own.
pub fn accept(id: i64) -> Result<i64, HostError> {
    let connection = socket(id, |socket, name| match socket {
        Socket::Listener(listener) => listener.accept(),
        _ => Err(("IOError", format!("{name} is not a listener"))),
    })?;
    let name = connection.name.clone();
    Ok(insert(
        Handle::Socket(Socket::Connection(Box::new(connection))),
        &name,
    ))
}

/// A socket for datagrams, sent from and received at a port of this
/// computer.
pub fn datagram_bind(host: &str, port_number: i64) -> Result<i64, HostError> {
    let socket = Datagrams::bind(host, port(port_number)?)?;
    Ok(insert(
        Handle::Socket(Socket::Datagrams(socket)),
        &format!("the datagram socket on {host}:{port_number}"),
    ))
}

fn datagrams<T>(
    id: i64,
    f: impl FnOnce(&mut Datagrams) -> Result<T, HostError>,
) -> Result<T, HostError> {
    socket(id, |socket, name| match socket {
        Socket::Datagrams(datagrams) => f(datagrams),
        _ => Err(("IOError", format!("{name} is not a datagram socket"))),
    })
}

pub fn datagram_send(id: i64, bytes: &[u8], host: &str, port_number: i64) -> Result<(), HostError> {
    let port = port(port_number)?;
    datagrams(id, |socket| socket.send(bytes, host, port))
}

pub fn datagram_receive(id: i64, size: i64) -> Result<Vec<u8>, HostError> {
    if size < 0 {
        return Err(("ValueError", format!("cannot receive {size} bytes")));
    }
    datagrams(id, |socket| socket.receive(size as usize))
}

pub fn datagram_sender(id: i64) -> Result<String, HostError> {
    datagrams(id, |socket| Ok(socket.sender()))
}

/// How long each operation of a socket may wait; 0 for no limit.
pub fn set_timeout(id: i64, timeout_ns: i64) -> Result<(), HostError> {
    let limit = timeout(timeout_ns)?;
    socket(id, |socket, _| {
        match socket {
            Socket::Connection(connection) => connection.timeout = limit,
            Socket::Listener(listener) => listener.timeout = limit,
            Socket::Datagrams(datagrams) => datagrams.timeout = limit,
            Socket::Connecting(_) => {}
        }
        Ok(())
    })
}

/// This computer's address and port of a socket.
pub fn local_address(id: i64) -> Result<String, HostError> {
    socket(id, |socket, name| match socket {
        Socket::Connection(connection) => connection.local_address(),
        Socket::Listener(listener) => listener.address(),
        Socket::Datagrams(datagrams) => datagrams.address(),
        Socket::Connecting(_) => Err(not_connection(name)),
    })
}

/// The other side's address and port of a connection.
pub fn peer_address(id: i64) -> Result<String, HostError> {
    with_connection(id, |connection| connection.peer_address())
}

pub fn finish_sending(id: i64) -> Result<(), HostError> {
    with_connection(id, |connection| connection.finish_sending())
}

/// Sends what connections hold of their output, as their sockets take it.
pub(crate) fn send_output() {
    for entry in table().entries.values_mut() {
        if let Handle::Socket(Socket::Connection(connection)) = &mut entry.handle
            && connection.has_output()
        {
            let _ = connection.send_some();
        }
    }
}

/// The sockets of the connections that still hold output.
pub(crate) fn sockets_with_output() -> Vec<u64> {
    table()
        .entries
        .values()
        .filter_map(|entry| match &entry.handle {
            Handle::Socket(Socket::Connection(connection)) if connection.has_output() => {
                Some(connection.raw())
            }
            _ => None,
        })
        .collect()
}

/// Adds a reference to a stream, for a clone of a value holding it.
pub fn share(id: i64) {
    if let Some(entry) = table().entries.get_mut(&id) {
        entry.references += 1;
    }
}

/// Removes a reference; the last one closes the file. Writing what is still
/// buffered can fail here only silently, which is why `close` exists.
pub fn release(id: i64) {
    let mut table = table();
    let Some(entry) = table.entries.get_mut(&id) else {
        return;
    };
    entry.references -= 1;
    if entry.references == 0 {
        let entry = table.entries.remove(&id);
        drop(table);
        drop(entry);
    }
}

/// Writes what every open stream still buffers, when the program ends.
pub fn flush_all() {
    for entry in table().entries.values_mut() {
        match &mut entry.handle {
            Handle::Writer(writer) => {
                let _ = writer.flush();
            }
            Handle::Socket(Socket::Connection(connection)) => connection.send_rest(),
            _ => {}
        }
    }
}

/// The next line without its line ending (LF or CRLF), or `None` at the
/// end. A line longer than `limit` bytes is an error, and the rest of it
/// stays unread.
pub fn read_line(id: i64, limit: i64) -> Result<Option<String>, HostError> {
    let limit = limit.max(0) as usize;
    if let Some(result) = connection(id, |connection| connection.read_line(limit)) {
        return result;
    }
    reader(id, |reader, path| {
        let too_long = || {
            (
                "IOError",
                format!("a line of \"{path}\" is longer than the limit of {limit} bytes"),
            )
        };
        let mut bytes = Vec::new();
        let mut ended = false;
        loop {
            let available = reader.fill_buf().map_err(|e| io_error("read", path, &e))?;
            if available.is_empty() {
                break;
            }
            if let Some(position) = available.iter().position(|&b| b == b'\n') {
                bytes.extend_from_slice(&available[..position]);
                reader.consume(position + 1);
                ended = true;
                break;
            }
            // Keeps no more than the limit and a carriage return.
            let take = available.len().min(limit + 2 - bytes.len());
            bytes.extend_from_slice(&available[..take]);
            reader.consume(take);
            if bytes.len() == limit + 2 {
                return Err(too_long());
            }
        }
        if !ended && bytes.is_empty() {
            return Ok(None);
        }
        if ended && bytes.ends_with(b"\r") {
            bytes.pop();
        }
        if bytes.len() > limit {
            return Err(too_long());
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| ("EncodingError", format!("\"{path}\" is not valid UTF-8")))
    })
}

/// The rest of the stream as text.
pub fn read_rest(id: i64) -> Result<String, HostError> {
    if let Some(result) = connection(id, |connection| connection.read_rest()) {
        return result;
    }
    reader(id, |reader, path| {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|e| io_error("read", path, &e))?;
        String::from_utf8(bytes)
            .map_err(|_| ("EncodingError", format!("\"{path}\" is not valid UTF-8")))
    })
}

/// Up to `count` bytes; fewer only at the end of the stream.
pub fn read_bytes(id: i64, count: i64) -> Result<Vec<u8>, HostError> {
    if count < 0 {
        return Err(("ValueError", format!("cannot read {count} bytes")));
    }
    if let Some(result) = connection(id, |connection| connection.read_bytes(count as usize)) {
        return result;
    }
    reader(id, |reader, path| {
        let mut bytes = Vec::new();
        reader
            .take(count as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| io_error("read", path, &e))?;
        Ok(bytes)
    })
}

pub fn write(id: i64, bytes: &[u8]) -> Result<(), HostError> {
    if let Some(result) = connection(id, |connection| connection.write(bytes)) {
        return result;
    }
    writer(id, |writer, path| {
        writer
            .write_all(bytes)
            .map_err(|e| io_error("write", path, &e))
    })
}

/// Passes buffered output to the system. It does not wait for the data to
/// reach the disk.
pub fn flush(id: i64) -> Result<(), HostError> {
    if let Some(result) = connection(id, |connection| connection.flush()) {
        return result;
    }
    writer(id, |writer, path| {
        writer.flush().map_err(|e| io_error("write", path, &e))
    })
}

/// The position in bytes from the start.
pub fn position(id: i64) -> Result<i64, HostError> {
    let mut table = table();
    let entry = table
        .entries
        .get_mut(&id)
        .ok_or(("IOError", "the file is closed".to_string()))?;
    let path = entry.path.clone();
    let result = match &mut entry.handle {
        Handle::Reader(reader) => reader.stream_position(),
        Handle::Writer(writer) => writer.stream_position(),
        Handle::Input | Handle::Socket(_) => return Err(unpositioned(&path)),
        Handle::Closed => return Err(closed(&path)),
    };
    result
        .map(|position| position as i64)
        .map_err(|e| io_error("read the position in", &path, &e))
}

/// Moves to a position in bytes from the start.
pub fn seek(id: i64, position: i64) -> Result<(), HostError> {
    if position < 0 {
        return Err((
            "ValueError",
            format!("{position} is not a position in a file"),
        ));
    }
    let mut table = table();
    let entry = table
        .entries
        .get_mut(&id)
        .ok_or(("IOError", "the file is closed".to_string()))?;
    let path = entry.path.clone();
    let target = SeekFrom::Start(position as u64);
    let result = match &mut entry.handle {
        Handle::Reader(reader) => reader.seek(target),
        Handle::Writer(writer) => writer.seek(target),
        Handle::Input | Handle::Socket(_) => return Err(unpositioned(&path)),
        Handle::Closed => return Err(closed(&path)),
    };
    result
        .map(|_| ())
        .map_err(|e| io_error("move in", &path, &e))
}

/// Finishes a stream: writes what it buffers and closes the file. A
/// connection's output goes first, which can wait.
pub fn close(id: i64) -> Result<(), HostError> {
    if let Some(result) = connection(id, |connection| connection.flush()) {
        result?;
    }
    let mut table = table();
    let entry = table
        .entries
        .get_mut(&id)
        .ok_or(("IOError", "the file is closed".to_string()))?;
    let handle = std::mem::replace(&mut entry.handle, Handle::Closed);
    let path = entry.path.clone();
    drop(table);
    match handle {
        Handle::Writer(writer) => writer
            .into_inner()
            .map(drop)
            .map_err(|e| io_error("write", &path, e.error())),
        Handle::Reader(_) | Handle::Input | Handle::Socket(_) => Ok(()),
        Handle::Closed => Err(("IOError", format!("\"{path}\" is already closed"))),
    }
}

/// Copies the rest of one stream to another, a part at a time, and returns
/// the number of bytes copied.
pub fn copy(from: i64, to: i64) -> Result<i64, HostError> {
    let mut table = table();
    let Table { entries, .. } = &mut *table;
    let [Some(source), Some(target)] = entries.get_disjoint_mut([&from, &to]) else {
        return Err(if from == to {
            ("ValueError", "cannot copy a file onto itself".to_string())
        } else {
            ("IOError", "the file is closed".to_string())
        });
    };
    let mut input;
    let reader: &mut dyn Read = match &mut source.handle {
        Handle::Reader(reader) => reader,
        Handle::Input => {
            let _ = std::io::stdout().flush();
            input = std::io::stdin().lock();
            &mut input
        }
        Handle::Writer(_) => {
            return Err((
                "IOError",
                format!("\"{}\" was opened for writing, not reading", source.path),
            ));
        }
        Handle::Socket(_) => return Err(("IOError", "io.copy copies between files".to_string())),
        Handle::Closed => return Err(closed(&source.path)),
    };
    let writer = match &mut target.handle {
        Handle::Writer(writer) => writer,
        Handle::Reader(_) | Handle::Input => {
            return Err((
                "IOError",
                format!("\"{}\" was opened for reading, not writing", target.path),
            ));
        }
        Handle::Socket(_) => return Err(("IOError", "io.copy copies between files".to_string())),
        Handle::Closed => return Err(closed(&target.path)),
    };
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0i64;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|e| io_error("read", &source.path, &e))?;
        if count == 0 {
            return Ok(total);
        }
        writer
            .write_all(&buffer[..count])
            .map_err(|e| io_error("write", &target.path, &e))?;
        total += count as i64;
    }
}
