//! Programs a program starts, with pipes to their standard input, output,
//! and error, for `standard/os`.
//!
//! Reading or writing a pipe can block, so each pipe has a thread of its
//! own: one reads what the program writes into a buffer, and one writes
//! what is queued for the program's input. Another thread waits for the
//! program to exit. An operation that must wait for one of them returns
//! `Blocked`, as a socket's does, and the threads wake the scheduler when
//! they have done something a task may wait for. So a program can read
//! another's output while its other tasks run, and reading both outputs
//! of a program never stops it for want of a reader.

use crate::host::{HostError, io_error};
use crate::network::{Background, IoWait, blocked, prepare_waking, wake};
use std::io::{ErrorKind, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// What a pipe buffers before its thread waits for some of it to be taken.
const LIMIT: usize = 1 << 20;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn utf8(bytes: Vec<u8>, name: &str) -> Result<String, HostError> {
    String::from_utf8(bytes).map_err(|_| ("EncodingError", format!("{name} is not valid UTF-8")))
}

// ----- what a program writes ---------------------------------------------------------------

#[derive(Default)]
struct Received {
    data: Vec<u8>,
    ended: bool,
    error: Option<String>,
    /// Everything is wanted at once, so the thread reads on without a limit.
    unlimited: bool,
}

/// The reading end of a program's output or error output.
pub struct PipeInput {
    received: Mutex<Received>,
    room: Condvar,
    /// Set when all of either output of the program is wanted at once:
    /// then neither pipe waits to be read, so that the program, which may
    /// write both, never waits on the one not being read.
    collecting: Arc<AtomicBool>,
    /// The pipe, for messages: `the output of "git"`.
    pub(crate) name: String,
}

impl Background for PipeInput {
    fn ready(&self) -> bool {
        let received = lock(&self.received);
        !received.data.is_empty() || received.ended
    }
}

impl PipeInput {
    fn start(
        mut source: impl Read + Send + 'static,
        name: String,
        collecting: Arc<AtomicBool>,
    ) -> Arc<PipeInput> {
        let pipe = Arc::new(PipeInput {
            received: Mutex::new(Received::default()),
            room: Condvar::new(),
            collecting,
            name,
        });
        let shared = pipe.clone();
        let started = std::thread::Builder::new()
            .name("pipe".to_string())
            .spawn(move || {
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    let result = source.read(&mut buffer);
                    let mut received = lock(&shared.received);
                    match result {
                        Ok(0) => received.ended = true,
                        Ok(count) => received.data.extend_from_slice(&buffer[..count]),
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) => {
                            received.error = Some(e.to_string());
                            received.ended = true;
                        }
                    }
                    let ended = received.ended;
                    drop(received);
                    wake();
                    if ended {
                        return;
                    }
                    let mut received = lock(&shared.received);
                    while received.data.len() >= LIMIT
                        && !received.unlimited
                        && !shared.collecting.load(Ordering::Relaxed)
                    {
                        // The other pipe can start collecting without
                        // telling this one, so look again now and then.
                        let wait = shared
                            .room
                            .wait_timeout(received, Duration::from_millis(50));
                        received = wait.unwrap_or_else(|e| e.into_inner()).0;
                    }
                }
            });
        if started.is_err() {
            let mut received = lock(&pipe.received);
            received.error = Some("no thread could be started to read it".to_string());
            received.ended = true;
        }
        pipe
    }

    fn wait(self: &Arc<Self>) -> HostError {
        blocked(IoWait::Background(self.clone()), None)
    }

    fn failure(&self, received: &Received) -> Result<(), HostError> {
        match &received.error {
            Some(error) => Err(("IOError", format!("could not read {}: {error}", self.name))),
            None => Ok(()),
        }
    }

    /// Takes the first `count` bytes, letting the thread read on.
    fn take(&self, received: &mut Received, count: usize) -> Vec<u8> {
        let taken = received.data.drain(..count).collect();
        self.room.notify_one();
        taken
    }

    pub fn read_line(self: &Arc<Self>, limit: usize) -> Result<Option<String>, HostError> {
        let mut received = lock(&self.received);
        let too_long = || {
            (
                "IOError",
                format!(
                    "a line of {} is longer than the limit of {limit} bytes",
                    self.name
                ),
            )
        };
        if let Some(end) = received.data.iter().position(|&b| b == b'\n') {
            let mut line = self.take(&mut received, end + 1);
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
            if line.len() > limit {
                return Err(too_long());
            }
            return utf8(line, &self.name).map(Some);
        }
        if received.data.len() > limit + 1 {
            return Err(too_long());
        }
        if received.ended {
            self.failure(&received)?;
            if received.data.is_empty() {
                return Ok(None);
            }
            let count = received.data.len();
            let line = self.take(&mut received, count);
            return utf8(line, &self.name).map(Some);
        }
        drop(received);
        Err(self.wait())
    }

    /// Up to `count` bytes, as many as have arrived; none at the end.
    pub fn read_bytes(self: &Arc<Self>, count: usize) -> Result<Vec<u8>, HostError> {
        let mut received = lock(&self.received);
        if count == 0 {
            return Ok(Vec::new());
        }
        if !received.data.is_empty() {
            let take = count.min(received.data.len());
            return Ok(self.take(&mut received, take));
        }
        if received.ended {
            self.failure(&received)?;
            return Ok(Vec::new());
        }
        drop(received);
        Err(self.wait())
    }

    /// Up to `count` bytes of text, as much as has arrived, ending with a
    /// whole character; "" at the end.
    pub fn read_text(self: &Arc<Self>, count: usize) -> Result<String, HostError> {
        let mut received = lock(&self.received);
        if count == 0 {
            return Ok(String::new());
        }
        let limit = count.max(4).min(received.data.len());
        let end = crate::text::char_boundary(&received.data, limit);
        if end > 0 {
            let text = self.take(&mut received, end);
            return utf8(text, &self.name);
        }
        if received.ended {
            self.failure(&received)?;
            if received.data.is_empty() {
                return Ok(String::new());
            }
            return Err(("EncodingError", format!("{} is not valid UTF-8", self.name)));
        }
        drop(received);
        Err(self.wait())
    }

    /// Everything until the program closes the pipe, as text.
    pub fn read_rest(self: &Arc<Self>) -> Result<String, HostError> {
        let mut received = lock(&self.received);
        if !received.ended {
            received.unlimited = true;
            self.collecting.store(true, Ordering::Relaxed);
            self.room.notify_one();
            drop(received);
            return Err(self.wait_for_end());
        }
        self.failure(&received)?;
        let count = received.data.len();
        let all = self.take(&mut received, count);
        utf8(all, &self.name)
    }

    fn wait_for_end(self: &Arc<Self>) -> HostError {
        struct End(Arc<PipeInput>);
        impl Background for End {
            fn ready(&self) -> bool {
                lock(&self.0.received).ended
            }
        }
        blocked(IoWait::Background(Arc::new(End(self.clone()))), None)
    }
}

// ----- what a program reads ---------------------------------------------------------------

#[derive(Default)]
struct Queued {
    data: Vec<u8>,
    /// The thread has taken data and is writing it.
    writing: bool,
    /// No more is to come: the thread finishes once the queue is written.
    closing: bool,
    /// The thread has finished and closed the pipe.
    finished: bool,
    error: Option<String>,
}

/// The writing end of a program's standard input.
pub struct PipeOutput {
    queued: Mutex<Queued>,
    work: Condvar,
    pub(crate) name: String,
}

#[derive(Clone, Copy)]
enum Until {
    /// Room in the queue for more.
    Room,
    /// Everything queued has been written.
    Written,
    /// The pipe is closed.
    Finished,
}

struct OutputWait(Arc<PipeOutput>, Until);

impl Background for OutputWait {
    fn ready(&self) -> bool {
        let queued = lock(&self.0.queued);
        queued.finished
            || match self.1 {
                Until::Room => queued.data.len() < LIMIT,
                Until::Written => queued.data.is_empty() && !queued.writing,
                Until::Finished => false,
            }
    }
}

impl PipeOutput {
    fn start(mut sink: impl Write + Send + 'static, name: String) -> Arc<PipeOutput> {
        let pipe = Arc::new(PipeOutput {
            queued: Mutex::new(Queued::default()),
            work: Condvar::new(),
            name,
        });
        let shared = pipe.clone();
        let started = std::thread::Builder::new()
            .name("pipe".to_string())
            .spawn(move || {
                loop {
                    let mut queued = lock(&shared.queued);
                    while queued.data.is_empty() && !queued.closing {
                        queued = shared.work.wait(queued).unwrap_or_else(|e| e.into_inner());
                    }
                    if queued.data.is_empty() {
                        break;
                    }
                    let data = std::mem::take(&mut queued.data);
                    queued.writing = true;
                    drop(queued);
                    let result = sink.write_all(&data).and_then(|()| sink.flush());
                    let mut queued = lock(&shared.queued);
                    queued.writing = false;
                    let failed = match result {
                        // A program that stopped reading takes no more; what
                        // was left for it goes nowhere.
                        Err(e) if e.kind() == ErrorKind::BrokenPipe => true,
                        Err(e) => {
                            queued.error = Some(e.to_string());
                            true
                        }
                        Ok(()) => false,
                    };
                    if failed {
                        queued.data.clear();
                        break;
                    }
                    drop(queued);
                    wake();
                }
                drop(sink);
                lock(&shared.queued).finished = true;
                wake();
            });
        if started.is_err() {
            let mut queued = lock(&pipe.queued);
            queued.error = Some("no thread could be started to write it".to_string());
            queued.finished = true;
        }
        pipe
    }

    fn wait(self: &Arc<Self>, until: Until) -> HostError {
        blocked(
            IoWait::Background(Arc::new(OutputWait(self.clone(), until))),
            None,
        )
    }

    fn failure(&self, queued: &Queued) -> Result<(), HostError> {
        match &queued.error {
            Some(error) => Err(("IOError", format!("could not write {}: {error}", self.name))),
            None => Ok(()),
        }
    }

    pub fn write(self: &Arc<Self>, bytes: &[u8]) -> Result<(), HostError> {
        let mut queued = lock(&self.queued);
        self.failure(&queued)?;
        if queued.finished || queued.closing {
            return Ok(());
        }
        if queued.data.len() >= LIMIT {
            drop(queued);
            return Err(self.wait(Until::Room));
        }
        queued.data.extend_from_slice(bytes);
        self.work.notify_one();
        Ok(())
    }

    /// Waits until what was written has gone to the program.
    pub fn flush(self: &Arc<Self>) -> Result<(), HostError> {
        let queued = lock(&self.queued);
        self.failure(&queued)?;
        if queued.finished || (queued.data.is_empty() && !queued.writing) {
            return Ok(());
        }
        drop(queued);
        Err(self.wait(Until::Written))
    }

    /// Ends the program's input once what was written has gone to it.
    pub fn close(self: &Arc<Self>) -> Result<(), HostError> {
        let mut queued = lock(&self.queued);
        queued.closing = true;
        self.work.notify_one();
        if queued.finished {
            return self.failure(&queued);
        }
        drop(queued);
        Err(self.wait(Until::Finished))
    }

    /// Lets the thread finish once it has written what is queued, when
    /// nothing refers to the pipe any more.
    pub(crate) fn release(&self) {
        lock(&self.queued).closing = true;
        self.work.notify_one();
    }
}

// ----- the program -------------------------------------------------------------------------

pub struct Process {
    /// The exit status once the program has finished: its code, or -1 when
    /// it was stopped without one.
    status: Mutex<Option<i64>>,
    /// Kept once the program has finished, so that its handle stays valid
    /// while the process value lives.
    child: Mutex<Option<Child>>,
    id: u32,
    #[cfg(windows)]
    handle: usize,
    pub(crate) name: String,
    pipes: Mutex<Pipes>,
}

#[derive(Default)]
struct Pipes {
    input: Option<Arc<PipeOutput>>,
    output: Option<Arc<PipeInput>>,
    errors: Option<Arc<PipeInput>>,
}

impl Background for Process {
    fn ready(&self) -> bool {
        lock(&self.status).is_some()
    }
}

impl Process {
    /// Starts a program with pipes to its standard input, output, and
    /// error. `environment` holds names and values to set; without
    /// `inherit`, they are the program's whole environment.
    pub fn start(
        program: &str,
        arguments: &[&str],
        directory: &str,
        environment: &[&str],
        inherit: bool,
    ) -> Result<Arc<Process>, HostError> {
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !directory.is_empty() {
            command.current_dir(directory);
        }
        if !inherit {
            command.env_clear();
        }
        for pair in environment.chunks(2) {
            if let [name, value] = pair {
                command.env(name, value);
            }
        }
        prepare_waking();
        // What the program printed comes before what the started one does.
        let _ = crate::console::stdout().flush();
        let mut child = command.spawn().map_err(|e| io_error("run", program, &e))?;
        let name = format!("\"{program}\"");
        let collecting = Arc::new(AtomicBool::new(false));
        let pipes = Pipes {
            input: child
                .stdin
                .take()
                .map(|stdin| PipeOutput::start(stdin, format!("the input of {name}"))),
            output: child.stdout.take().map(|stdout| {
                PipeInput::start(stdout, format!("the output of {name}"), collecting.clone())
            }),
            errors: child.stderr.take().map(|stderr| {
                let name = format!("the error output of {name}");
                PipeInput::start(stderr, name, collecting.clone())
            }),
        };
        #[cfg(windows)]
        let handle = {
            use std::os::windows::io::AsRawHandle;
            child.as_raw_handle() as usize
        };
        let process = Arc::new(Process {
            status: Mutex::new(None),
            child: Mutex::new(None),
            id: child.id(),
            #[cfg(windows)]
            handle,
            name,
            pipes: Mutex::new(pipes),
        });
        let shared = process.clone();
        let waiting = std::thread::Builder::new()
            .name("process".to_string())
            .spawn(move || {
                let status = child
                    .wait()
                    .ok()
                    .and_then(|s| s.code())
                    .map_or(-1, i64::from);
                *lock(&shared.child) = Some(child);
                *lock(&shared.status) = Some(status);
                wake();
            });
        if waiting.is_err() {
            *lock(&process.status) = Some(-1);
        }
        Ok(process)
    }

    pub(crate) fn take_input(&self) -> Option<Arc<PipeOutput>> {
        lock(&self.pipes).input.take()
    }

    pub(crate) fn take_output(&self) -> Option<Arc<PipeInput>> {
        lock(&self.pipes).output.take()
    }

    pub(crate) fn take_errors(&self) -> Option<Arc<PipeInput>> {
        lock(&self.pipes).errors.take()
    }

    pub fn id(&self) -> i64 {
        i64::from(self.id)
    }

    /// The exit status, waiting for the program to finish.
    pub fn wait(self: &Arc<Self>) -> Result<i64, HostError> {
        match *lock(&self.status) {
            Some(status) => Ok(status),
            None => Err(blocked(IoWait::Background(self.clone()), None)),
        }
    }

    /// Stops the program, unless it has finished.
    pub fn kill(&self) -> Result<(), HostError> {
        let status = lock(&self.status);
        if status.is_some() {
            return Ok(());
        }
        // The thread waiting for the program holds it, so its handle is
        // valid until the status is set, which the lock holds off.
        let stopped = platform::stop(self);
        drop(status);
        if stopped {
            Ok(())
        } else {
            Err(("IOError", format!("could not stop {}", self.name)))
        }
    }
}

#[cfg(windows)]
mod platform {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn TerminateProcess(process: usize, code: u32) -> i32;
    }

    pub fn stop(process: &super::Process) -> bool {
        // SAFETY: the handle is the running program's.
        unsafe { TerminateProcess(process.handle, 1) != 0 }
    }
}

#[cfg(unix)]
mod platform {
    unsafe extern "C" {
        fn kill(process: i32, signal: i32) -> i32;
    }

    pub fn stop(process: &super::Process) -> bool {
        const KILL: i32 = 9;
        // SAFETY: the program has not been waited for, so its number is
        // still its own.
        unsafe { kill(process.id as i32, KILL) == 0 }
    }
}

/// Systems that start no programs, such as WASI, never have one to stop.
#[cfg(not(any(windows, unix)))]
mod platform {
    pub fn stop(_process: &super::Process) -> bool {
        false
    }
}
