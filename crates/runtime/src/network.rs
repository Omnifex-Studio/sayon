//! Sockets for `standard/network`: TCP connections and listeners, and UDP
//! sockets. They are entries of the stream table, beside files, so that a
//! connection is read and written like one.
//!
//! Every socket is non-blocking. An operation that cannot finish yet
//! returns the error `Blocked`, after recording what it waits for (see
//! [`take_blocked`]); the scheduler of the interpreter or of compiled
//! programs runs other tasks until the socket is ready, then runs the
//! operation again. So each operation is written to be run again: a read
//! takes data only when it gives it out, and a write adds its bytes to the
//! connection's output only once, when there is room. The output goes out as
//! the socket takes it, on every operation of the connection and whenever
//! the scheduler waits.
//!
//! Making a connection blocks, to find the host's addresses and then to
//! connect, so it runs on a thread of its own, which wakes the scheduler
//! when it is done.

use crate::host::HostError;
use std::cell::RefCell;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The code of the error an operation returns when it must wait.
pub const BLOCKED_CODE: &str = "Blocked";

/// Output a connection holds before a write waits for some of it to go.
const OUTPUT_LIMIT: usize = 1 << 20;

/// What an operation that returned `Blocked` waits for, and for how long.
pub struct Blocked {
    pub wait: IoWait,
    pub timeout: Option<Duration>,
}

#[derive(Clone)]
pub enum IoWait {
    /// A socket to have data to read, or room to write.
    Socket { raw: u64, read: bool, write: bool },
    /// A connection being made on a thread of its own.
    Job(Arc<Job>),
}

thread_local! {
    static BLOCKED: RefCell<Option<Blocked>> = const { RefCell::new(None) };
}

fn blocked(wait: IoWait, timeout: Option<Duration>) -> HostError {
    BLOCKED.with(|cell| *cell.borrow_mut() = Some(Blocked { wait, timeout }));
    (BLOCKED_CODE, String::new())
}

/// What the operation that last returned `Blocked` waits for.
pub fn take_blocked() -> Option<Blocked> {
    BLOCKED.with(|cell| cell.borrow_mut().take())
}

/// Whether what a task waits for has come.
pub fn ready(wait: &IoWait) -> bool {
    match wait {
        IoWait::Socket { raw, read, write } => {
            let mut fds = [platform::entry(*raw, *read, *write)];
            platform::poll(&mut fds, Some(Duration::ZERO)) > 0 && fds[0].revents != 0
        }
        IoWait::Job(job) => job.finished(),
    }
}

/// Waits until one of `waits` may be ready, or `timeout` passes, sending the
/// output connections hold as their sockets take it.
pub fn wait_any(waits: &[IoWait], timeout: Option<Duration>) {
    crate::streams::send_output();
    let mut fds = Vec::new();
    let mut jobs = false;
    for wait in waits {
        match wait {
            IoWait::Socket { raw, read, write } => fds.push(platform::entry(*raw, *read, *write)),
            IoWait::Job(_) => jobs = true,
        }
    }
    for raw in crate::streams::sockets_with_output() {
        fds.push(platform::entry(raw, false, true));
    }
    let mut timeout = timeout;
    if jobs {
        match waker() {
            Some(waker) => fds.push(platform::entry(waker.raw(), true, false)),
            // Without a way to be woken, look again soon.
            None => {
                let soon = Duration::from_millis(10);
                timeout = Some(timeout.map_or(soon, |t| t.min(soon)));
            }
        }
    }
    if fds.is_empty() {
        if let Some(timeout) = timeout {
            std::thread::sleep(timeout);
        }
        return;
    }
    platform::poll(&mut fds, timeout);
    if jobs && let Some(waker) = waker() {
        waker.drain();
    }
}

/// The message of the error for an operation that waited longer than its
/// timeout.
pub fn timeout_message(timeout: Duration) -> String {
    let milliseconds = timeout.as_millis();
    if milliseconds < 1000 {
        format!("no answer within {milliseconds} milliseconds")
    } else if milliseconds.is_multiple_of(1000) {
        let seconds = milliseconds / 1000;
        format!(
            "no answer within {seconds} second{}",
            if seconds == 1 { "" } else { "s" }
        )
    } else {
        format!("no answer within {} seconds", milliseconds as f64 / 1000.0)
    }
}

/// The error for an operating-system failure while doing `what`.
fn failure(error: &std::io::Error, what: &str) -> HostError {
    match error.kind() {
        ErrorKind::ConnectionRefused => (
            "ConnectionError",
            format!("{what}: the connection was refused"),
        ),
        ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::BrokenPipe
        | ErrorKind::NotConnected
        | ErrorKind::UnexpectedEof => (
            "ConnectionError",
            format!("{what}: the other side closed the connection"),
        ),
        ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable | ErrorKind::NetworkDown => (
            "ConnectionError",
            format!("{what}: the network cannot reach it"),
        ),
        ErrorKind::TimedOut => ("TimeoutError", format!("{what}: no answer in time")),
        ErrorKind::AddrInUse => (
            "AddressError",
            format!("{what}: the address is already in use"),
        ),
        ErrorKind::AddrNotAvailable => (
            "AddressError",
            format!("{what}: the address is not one of this computer's"),
        ),
        _ => ("NetworkError", format!("{what}: {error}")),
    }
}

fn unknown_host(host: &str) -> HostError {
    (
        "AddressError",
        format!("cannot find the address of \"{host}\""),
    )
}

fn addresses(host: &str, port: u16) -> Result<Vec<SocketAddr>, HostError> {
    let found: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| unknown_host(host))?
        .collect();
    if found.is_empty() {
        return Err(unknown_host(host));
    }
    Ok(found)
}

/// The addresses of a host, as text, in the order the system gives them.
pub fn resolve(host: &str) -> Result<Vec<String>, HostError> {
    let mut found: Vec<String> = Vec::new();
    for address in addresses(host, 0)? {
        let text = address.ip().to_string();
        if !found.contains(&text) {
            found.push(text);
        }
    }
    Ok(found)
}

/// An address and port as text: `127.0.0.1:80`, or `[::1]:80`.
fn address_text(address: SocketAddr) -> String {
    address.to_string()
}

fn utf8(bytes: Vec<u8>, name: &str) -> Result<String, HostError> {
    String::from_utf8(bytes).map_err(|_| {
        (
            "EncodingError",
            format!("the text from {name} is not valid UTF-8"),
        )
    })
}

// ----- connections ---------------------------------------------------------------------------

pub struct Connection {
    stream: TcpStream,
    /// What has arrived and not been read yet.
    input: Vec<u8>,
    /// What was written and has not gone yet.
    output: Vec<u8>,
    /// The other side has finished sending.
    ended: bool,
    pub(crate) timeout: Option<Duration>,
    /// The other side's address, for messages.
    pub(crate) name: String,
}

impl Connection {
    fn new(stream: TcpStream, timeout: Option<Duration>) -> Connection {
        let _ = stream.set_nodelay(true);
        let name = stream
            .peer_addr()
            .map(address_text)
            .unwrap_or_else(|_| "the connection".to_string());
        Connection {
            stream,
            input: Vec::new(),
            output: Vec::new(),
            ended: false,
            timeout,
            name,
        }
    }

    pub(crate) fn raw(&self) -> u64 {
        platform::raw(&self.stream)
    }

    pub(crate) fn has_output(&self) -> bool {
        !self.output.is_empty()
    }

    fn what(&self) -> String {
        format!("the connection with {}", self.name)
    }

    /// Sends what the socket takes of the output.
    pub(crate) fn send_some(&mut self) -> Result<(), HostError> {
        while !self.output.is_empty() {
            match self.stream.write(&self.output) {
                Ok(0) => return Err(failure(&ErrorKind::BrokenPipe.into(), &self.what())),
                Ok(count) => {
                    self.output.drain(..count);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(failure(&e, &self.what())),
            }
        }
        Ok(())
    }

    /// Takes in what has arrived. False when nothing has, yet.
    fn receive(&mut self) -> Result<bool, HostError> {
        let mut buffer = [0u8; 16 * 1024];
        match self.stream.read(&mut buffer) {
            Ok(0) => {
                self.ended = true;
                Ok(true)
            }
            Ok(count) => {
                self.input.extend_from_slice(&buffer[..count]);
                Ok(true)
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(false),
            Err(e) if e.kind() == ErrorKind::Interrupted => Ok(true),
            Err(e) => Err(failure(&e, &self.what())),
        }
    }

    fn wait_to_read(&self) -> HostError {
        let wait = IoWait::Socket {
            raw: self.raw(),
            read: true,
            write: self.has_output(),
        };
        blocked(wait, self.timeout)
    }

    fn wait_to_write(&self) -> HostError {
        let wait = IoWait::Socket {
            raw: self.raw(),
            read: false,
            write: true,
        };
        blocked(wait, self.timeout)
    }

    /// The next line without its line ending, or `None` once the other side
    /// has finished sending.
    pub fn read_line(&mut self, limit: usize) -> Result<Option<String>, HostError> {
        self.send_some()?;
        let too_long = |name: &str| {
            (
                "IOError",
                format!("a line from {name} is longer than the limit of {limit} bytes"),
            )
        };
        loop {
            if let Some(end) = self.input.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.input.drain(..=end).collect();
                line.pop();
                if line.ends_with(b"\r") {
                    line.pop();
                }
                if line.len() > limit {
                    return Err(too_long(&self.name));
                }
                return utf8(line, &self.name).map(Some);
            }
            if self.input.len() > limit + 1 {
                return Err(too_long(&self.name));
            }
            if self.ended {
                if self.input.is_empty() {
                    return Ok(None);
                }
                let line = std::mem::take(&mut self.input);
                return utf8(line, &self.name).map(Some);
            }
            if !self.receive()? {
                return Err(self.wait_to_read());
            }
        }
    }

    /// Up to `count` bytes, as many as have arrived; none once the other
    /// side has finished sending.
    pub fn read_bytes(&mut self, count: usize) -> Result<Vec<u8>, HostError> {
        self.send_some()?;
        if count == 0 {
            return Ok(Vec::new());
        }
        loop {
            if !self.input.is_empty() {
                let take = count.min(self.input.len());
                return Ok(self.input.drain(..take).collect());
            }
            if self.ended {
                return Ok(Vec::new());
            }
            if !self.receive()? {
                return Err(self.wait_to_read());
            }
        }
    }

    /// Everything the other side sends until it finishes, as text.
    pub fn read_rest(&mut self) -> Result<String, HostError> {
        self.send_some()?;
        while !self.ended {
            if !self.receive()? {
                return Err(self.wait_to_read());
            }
        }
        utf8(std::mem::take(&mut self.input), &self.name)
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        if self.output.len() >= OUTPUT_LIMIT {
            self.send_some()?;
            if self.output.len() >= OUTPUT_LIMIT {
                return Err(self.wait_to_write());
            }
        }
        self.output.extend_from_slice(bytes);
        self.send_some()
    }

    /// Waits until the output has gone to the system.
    pub fn flush(&mut self) -> Result<(), HostError> {
        self.send_some()?;
        if self.has_output() {
            return Err(self.wait_to_write());
        }
        Ok(())
    }

    /// Finishes sending, after the output has gone; the other side's reads
    /// then end.
    pub fn finish_sending(&mut self) -> Result<(), HostError> {
        self.flush()?;
        match self.stream.shutdown(Shutdown::Write) {
            Err(e) if e.kind() != ErrorKind::NotConnected => Err(failure(&e, &self.what())),
            _ => Ok(()),
        }
    }

    pub fn local_address(&self) -> Result<String, HostError> {
        self.stream
            .local_addr()
            .map(address_text)
            .map_err(|e| failure(&e, &self.what()))
    }

    pub fn peer_address(&self) -> Result<String, HostError> {
        self.stream
            .peer_addr()
            .map(address_text)
            .map_err(|e| failure(&e, &self.what()))
    }
}

impl Connection {
    /// Sends the rest of the output, waiting a little for the other side
    /// to take it, when the connection is about to close.
    pub(crate) fn send_rest(&mut self) {
        if self.has_output() && self.stream.set_nonblocking(false).is_ok() {
            let _ = self.stream.set_write_timeout(Some(Duration::from_secs(2)));
            let _ = self.stream.write_all(&self.output);
            self.output.clear();
            let _ = self.stream.set_nonblocking(true);
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.send_rest();
    }
}

// ----- listeners and making connections -----------------------------------------------------

pub struct Listener {
    listener: TcpListener,
    pub(crate) timeout: Option<Duration>,
}

impl Listener {
    pub fn bind(host: &str, port: u16) -> Result<Listener, HostError> {
        let what = format!("cannot listen on {host}:{port}");
        let listener =
            TcpListener::bind(addresses(host, port)?.as_slice()).map_err(|e| failure(&e, &what))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| failure(&e, &what))?;
        Ok(Listener {
            listener,
            timeout: None,
        })
    }

    /// The next connection made to the listener.
    pub fn accept(&mut self) -> Result<Connection, HostError> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let what = "cannot accept a connection";
                    stream
                        .set_nonblocking(true)
                        .map_err(|e| failure(&e, what))?;
                    return Ok(Connection::new(stream, None));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    let wait = IoWait::Socket {
                        raw: platform::raw(&self.listener),
                        read: true,
                        write: false,
                    };
                    return Err(blocked(wait, self.timeout));
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                // A connection that failed before it was accepted does not
                // stop the listener.
                Err(e) if e.kind() == ErrorKind::ConnectionAborted => {}
                Err(e) => return Err(failure(&e, "cannot accept a connection")),
            }
        }
    }

    pub fn address(&self) -> Result<String, HostError> {
        self.listener
            .local_addr()
            .map(address_text)
            .map_err(|e| failure(&e, "the listener"))
    }
}

/// A connection being made on a thread of its own.
pub struct Job {
    result: Mutex<Option<Result<TcpStream, HostError>>>,
}

impl Job {
    fn finished(&self) -> bool {
        self.result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

pub struct Connecting {
    job: Arc<Job>,
    timeout: Option<Duration>,
}

impl Connecting {
    /// Starts connecting to a host, trying each of its addresses in turn,
    /// for at most `timeout` each.
    pub fn start(host: &str, port: u16, timeout: Option<Duration>) -> Connecting {
        let job = Arc::new(Job {
            result: Mutex::new(None),
        });
        // Made before the thread can need it.
        let _ = waker();
        let host = host.to_string();
        let shared = job.clone();
        let started = std::thread::Builder::new()
            .name("connect".to_string())
            .spawn(move || {
                let result = connect(&host, port, timeout);
                *shared.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                if let Some(waker) = waker() {
                    waker.wake();
                }
            });
        if let Err(e) = started {
            *job.result.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Err(failure(&e, "cannot start connecting")));
        }
        Connecting { job, timeout }
    }

    /// The connection once it is made, its error if it could not be, or
    /// `Blocked` while it is being made.
    pub fn finish(&self) -> Result<Connection, HostError> {
        let mut result = self.job.result.lock().unwrap_or_else(|e| e.into_inner());
        match result.take() {
            Some(Ok(stream)) => Ok(Connection::new(stream, self.timeout)),
            Some(Err(error)) => Err(error),
            None => {
                drop(result);
                Err(blocked(IoWait::Job(self.job.clone()), self.timeout))
            }
        }
    }
}

fn connect(host: &str, port: u16, timeout: Option<Duration>) -> Result<TcpStream, HostError> {
    let what = format!("cannot connect to {host}:{port}");
    let mut last = None;
    for address in addresses(host, port)? {
        let attempt = match timeout {
            Some(timeout) => TcpStream::connect_timeout(&address, timeout),
            None => TcpStream::connect(address),
        };
        match attempt {
            Ok(stream) => {
                stream
                    .set_nonblocking(true)
                    .map_err(|e| failure(&e, &what))?;
                return Ok(stream);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        Some(e) if e.kind() == ErrorKind::TimedOut => (
            "TimeoutError",
            format!("{what}: {}", timeout_message(timeout.unwrap_or_default())),
        ),
        Some(e) => failure(&e, &what),
        None => unknown_host(host),
    })
}

// ----- datagrams ---------------------------------------------------------------------------------

pub struct Datagrams {
    socket: UdpSocket,
    pub(crate) timeout: Option<Duration>,
    /// Where the last datagram received came from.
    sender: String,
}

impl Datagrams {
    pub fn bind(host: &str, port: u16) -> Result<Datagrams, HostError> {
        let what = format!("cannot use {host}:{port} for datagrams");
        let socket =
            UdpSocket::bind(addresses(host, port)?.as_slice()).map_err(|e| failure(&e, &what))?;
        socket
            .set_nonblocking(true)
            .map_err(|e| failure(&e, &what))?;
        Ok(Datagrams {
            socket,
            timeout: None,
            sender: String::new(),
        })
    }

    fn wait(&self, read: bool) -> HostError {
        let wait = IoWait::Socket {
            raw: platform::raw(&self.socket),
            read,
            write: !read,
        };
        blocked(wait, self.timeout)
    }

    /// Sends one datagram to a host.
    pub fn send(&mut self, bytes: &[u8], host: &str, port: u16) -> Result<(), HostError> {
        let what = format!("cannot send to {host}:{port}");
        let ipv6 = self
            .socket
            .local_addr()
            .map(|a| a.is_ipv6())
            .unwrap_or(false);
        let found = addresses(host, port)?;
        let target = found
            .iter()
            .find(|a| a.is_ipv6() == ipv6)
            .copied()
            .unwrap_or(found[0]);
        loop {
            match self.socket.send_to(bytes, target) {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Err(self.wait(false)),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(failure(&e, &what)),
            }
        }
    }

    /// The next datagram, up to `size` bytes of it.
    pub fn receive(&mut self, size: usize) -> Result<Vec<u8>, HostError> {
        let mut buffer = vec![0u8; size.max(1)];
        loop {
            match self.socket.recv_from(&mut buffer) {
                Ok((count, from)) => {
                    self.sender = address_text(from);
                    buffer.truncate(count.min(size));
                    return Ok(buffer);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Err(self.wait(true)),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                // Windows reports a datagram that did not arrive elsewhere,
                // or one larger than the buffer, which is cut to its size.
                Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
                Err(e) if platform::truncated(&e) => {
                    buffer.truncate(size);
                    return Ok(buffer);
                }
                Err(e) => return Err(failure(&e, "cannot receive a datagram")),
            }
        }
    }

    pub fn sender(&self) -> String {
        self.sender.clone()
    }

    pub fn address(&self) -> Result<String, HostError> {
        self.socket
            .local_addr()
            .map(address_text)
            .map_err(|e| failure(&e, "the datagram socket"))
    }
}

// ----- waking the scheduler ----------------------------------------------------------------------

/// A socket the threads making connections send a byte to when they are
/// done, which the scheduler waits on with the others.
struct Waker {
    socket: UdpSocket,
    address: SocketAddr,
}

impl Waker {
    fn raw(&self) -> u64 {
        platform::raw(&self.socket)
    }

    fn wake(&self) {
        let _ = self.socket.send_to(&[1], self.address);
    }

    fn drain(&self) {
        let mut buffer = [0u8; 64];
        while self.socket.recv(&mut buffer).is_ok() {}
    }
}

static WAKER: OnceLock<Option<Waker>> = OnceLock::new();

fn waker() -> Option<&'static Waker> {
    WAKER
        .get_or_init(|| {
            let socket = UdpSocket::bind("127.0.0.1:0").ok()?;
            socket.set_nonblocking(true).ok()?;
            let address = socket.local_addr().ok()?;
            Some(Waker { socket, address })
        })
        .as_ref()
}

// ----- the system's poll --------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use std::os::windows::io::AsRawSocket;
    use std::time::Duration;

    #[repr(C)]
    pub struct Entry {
        fd: usize,
        events: i16,
        pub revents: i16,
    }

    const READ: i16 = 0x0300;
    const WRITE: i16 = 0x0010;

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAPoll(fds: *mut Entry, count: u32, timeout: i32) -> i32;
    }

    pub fn raw(socket: &impl AsRawSocket) -> u64 {
        socket.as_raw_socket()
    }

    pub fn entry(raw: u64, read: bool, write: bool) -> Entry {
        let mut events = 0;
        if read {
            events |= READ;
        }
        if write {
            events |= WRITE;
        }
        Entry {
            fd: raw as usize,
            events,
            revents: 0,
        }
    }

    pub fn poll(fds: &mut [Entry], timeout: Option<Duration>) -> i32 {
        // SAFETY: the entries name open sockets.
        unsafe {
            WSAPoll(
                fds.as_mut_ptr(),
                fds.len() as u32,
                super::milliseconds(timeout),
            )
        }
    }

    /// Windows fails a receive into a buffer smaller than the datagram.
    pub fn truncated(error: &std::io::Error) -> bool {
        const WSAEMSGSIZE: i32 = 10040;
        error.raw_os_error() == Some(WSAEMSGSIZE)
    }
}

#[cfg(unix)]
mod platform {
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    #[repr(C)]
    pub struct Entry {
        fd: i32,
        events: i16,
        pub revents: i16,
    }

    const READ: i16 = 0x0001;
    const WRITE: i16 = 0x0004;

    unsafe extern "C" {
        fn poll(fds: *mut Entry, count: std::ffi::c_ulong, timeout: i32) -> i32;
    }

    pub fn raw(socket: &impl AsRawFd) -> u64 {
        socket.as_raw_fd() as u64
    }

    pub fn entry(raw: u64, read: bool, write: bool) -> Entry {
        let mut events = 0;
        if read {
            events |= READ;
        }
        if write {
            events |= WRITE;
        }
        Entry {
            fd: raw as i32,
            events,
            revents: 0,
        }
    }

    pub fn poll(fds: &mut [Entry], timeout: Option<Duration>) -> i32 {
        // SAFETY: the entries name open sockets.
        unsafe {
            poll(
                fds.as_mut_ptr(),
                fds.len() as std::ffi::c_ulong,
                super::milliseconds(timeout),
            )
        }
    }

    /// Other systems cut a datagram to the buffer without failing.
    pub fn truncated(_error: &std::io::Error) -> bool {
        false
    }
}

/// A timeout for the system's poll: whole milliseconds, rounded up so that
/// a wait never ends early, or -1 for none.
fn milliseconds(timeout: Option<Duration>) -> i32 {
    match timeout {
        None => -1,
        Some(timeout) => {
            let milliseconds = timeout.as_nanos().div_ceil(1_000_000);
            milliseconds.min(i32::MAX as u128) as i32
        }
    }
}
