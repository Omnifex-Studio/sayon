//! HTTP in a web page, where the browser makes each request with its
//! `fetch`, through the host functions of the import module `sayon` that the
//! page's generated glue gives. There `standard/network`'s `request` calls
//! `host_fetch` in place of its own client over sockets: the task waits while
//! the browser fetches, and the other tasks run, then the response becomes a
//! stream whose head and body `host_fetch_head` and `host_fetch_body` read.

/// Whether the browser has settled a request. Nothing is fetched elsewhere.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn settled(_request: i32) -> bool {
    false
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub use web::{abandon, finish, settled, start};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod web {
    use crate::host::{HostArg, HostError};
    use crate::network::timeout_message;
    use crate::streams::Fetched;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::Duration;

    #[link(wasm_import_module = "sayon")]
    unsafe extern "C" {
        /// Starts fetching a request and gives its number. `fields` holds the
        /// address and length of each header's name and value in turn, `count`
        /// strings in all. Each wait, for the response and then for each part
        /// of its body, may last `timeout` milliseconds (zero for no limit).
        fn host_fetch(
            method: *const u8,
            method_length: usize,
            url: *const u8,
            url_length: usize,
            fields: *const u32,
            count: usize,
            body: *const u8,
            body_length: usize,
            timeout: f64,
            redirects: f64,
            limit: f64,
        ) -> i32;
        /// [`WAITING`], [`ANSWERED`], or 2 once it failed.
        fn host_fetch_state(request: i32) -> i32;
        /// The response's status; for a failed request, why it failed.
        fn host_fetch_status(request: i32) -> i32;
        /// The length in bytes of a part of the response, or of the failure.
        fn host_fetch_length(request: i32, part: i32) -> usize;
        /// Copies a part of the response, or of the failure, to `into`.
        fn host_fetch_copy(request: i32, part: i32, into: *mut u8);
        /// Forgets the request, and stops fetching it if it has not settled.
        fn host_fetch_close(request: i32);
    }

    const WAITING: i32 = 0;
    const ANSWERED: i32 = 1;

    /// The parts of a response: its reason, its URL after any redirects, its
    /// headers' lowercase names and values in turn, each ended by a line
    /// break, and its body. A failure's one part is what went wrong.
    const REASON: i32 = 0;
    const URL: i32 = 1;
    const HEADERS: i32 = 2;
    const BODY: i32 = 3;
    const DETAIL: i32 = 0;

    /// Why a request failed, besides the browser's being unable to fetch it
    /// (0, told by the browser's message): a wait lasted longer than the
    /// timeout, the body was longer than the limit (the part is then the
    /// response's URL), or it was redirected when it was not to be.
    const TIMED_OUT: i32 = 1;
    const TOO_LONG: i32 = 2;
    const REDIRECTED: i32 = 3;

    /// What a request's failure is told with.
    struct Request {
        url: String,
        timeout: Duration,
        redirects: i64,
        limit: i64,
    }

    thread_local! {
        static REQUESTS: RefCell<HashMap<i32, Request>> = RefCell::new(HashMap::new());
    }

    /// Starts fetching the request of `host_fetch`'s arguments: its method,
    /// URL, headers' names and values in turn, body, timeout in nanoseconds,
    /// redirects to follow, and the longest body to take.
    pub fn start(args: &[HostArg]) -> Result<i32, HostError> {
        let [
            HostArg::String(method),
            HostArg::String(url),
            HostArg::Strings(headers),
            HostArg::Bytes(body),
            HostArg::Integer(timeout),
            HostArg::Integer(redirects),
            HostArg::Integer(limit),
        ] = args
        else {
            panic!("host_fetch's arguments: {args:?}");
        };
        let fields: Vec<u32> = headers
            .iter()
            .flat_map(|text| [text.as_ptr() as u32, text.len() as u32])
            .collect();
        let timeout = Duration::from_nanos((*timeout).max(0) as u64);
        // SAFETY: each address is readable for its length during the call,
        // in which the glue copies what it needs.
        let request = unsafe {
            host_fetch(
                method.as_ptr(),
                method.len(),
                url.as_ptr(),
                url.len(),
                fields.as_ptr(),
                headers.len(),
                body.as_ptr(),
                body.len(),
                timeout.as_secs_f64() * 1000.0,
                *redirects as f64,
                *limit as f64,
            )
        };
        let details = Request {
            url: url.to_string(),
            timeout,
            redirects: *redirects,
            limit: *limit,
        };
        REQUESTS.with(|requests| requests.borrow_mut().insert(request, details));
        Ok(request)
    }

    /// Whether the browser has the response to a request, or has given up.
    pub fn settled(request: i32) -> bool {
        // SAFETY: the glue only looks the request up.
        unsafe { host_fetch_state(request) != WAITING }
    }

    /// The response to a settled request, as a new stream holding it, or
    /// why there is none. Either way the request is done with.
    pub fn finish(request: i32) -> Result<i64, HostError> {
        let details = REQUESTS
            .with(|requests| requests.borrow_mut().remove(&request))
            .expect("a request being fetched");
        // SAFETY: the glue copies each part into a buffer of its length.
        let part = |index: i32| unsafe {
            let mut bytes = vec![0u8; host_fetch_length(request, index)];
            host_fetch_copy(request, index, bytes.as_mut_ptr());
            bytes
        };
        let text = |index: i32| String::from_utf8_lossy(&part(index)).into_owned();
        // SAFETY: the glue only looks the request up.
        let (state, status) = unsafe { (host_fetch_state(request), host_fetch_status(request)) };
        let result = if state == ANSWERED {
            let url = text(URL);
            let mut head = vec![status.to_string(), text(REASON), url.clone()];
            let headers = text(HEADERS);
            head.extend(headers.lines().map(str::to_string));
            let body = part(BODY);
            Ok(crate::streams::fetched(&url, Fetched { head, body }))
        } else {
            let detail = text(DETAIL);
            let Request {
                url,
                timeout,
                redirects,
                limit,
            } = details;
            Err(match status {
                TIMED_OUT => ("TimeoutError", timeout_message(timeout)),
                TOO_LONG => (
                    "HttpError",
                    format!("the body from {detail} is longer than the limit of {limit} bytes"),
                ),
                REDIRECTED => (
                    "HttpError",
                    format!("{url} redirected more than {redirects} times"),
                ),
                _ => ("NetworkError", format!("could not fetch {url}: {detail}")),
            })
        };
        // SAFETY: the request is not used again.
        unsafe { host_fetch_close(request) };
        result
    }

    /// Stops fetching a request whose task no longer waits for it.
    pub fn abandon(request: i32) {
        REQUESTS.with(|requests| requests.borrow_mut().remove(&request));
        // SAFETY: the request is not used again.
        unsafe { host_fetch_close(request) };
    }
}
