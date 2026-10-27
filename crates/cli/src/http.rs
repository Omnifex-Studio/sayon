//! HTTP/1.1 requests for the registry commands, over the runtime's
//! connections and the system's TLS: one request for each connection, with
//! the whole answer read before it returns.

use sayon_runtime::host::HostError;
use sayon_runtime::network;
use sayon_runtime::streams;
use std::time::{Duration, Instant};

/// How long connecting, and each wait for the server afterwards, may last.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The most an answer's body may hold.
const LARGEST: usize = 32 * 1024 * 1024;

pub struct Response {
    pub status: u16,
    /// Names in lowercase.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

struct Address {
    secure: bool,
    host: String,
    port: u16,
    /// The path and query, starting with `/`.
    target: String,
}

fn address(url: &str) -> Result<Address, String> {
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(format!("{url} is not an HTTP address"));
    };
    let (authority, target) = match rest.find('/') {
        Some(at) => (&rest[..at], rest[at..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (
            host,
            port.parse()
                .map_err(|_| format!("{url} has no usable port"))?,
        ),
        _ => (authority, if secure { 443 } else { 80 }),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Err(format!("{url} names no server"));
    }
    Ok(Address {
        secure,
        host: host.to_string(),
        port,
        target,
    })
}

/// Runs an operation of the runtime's until it no longer waits, waiting
/// for its socket in between.
fn finish<T>(mut operation: impl FnMut() -> Result<T, HostError>) -> Result<T, String> {
    let started = Instant::now();
    loop {
        match operation() {
            Err((code, _)) if code == network::BLOCKED_CODE => {
                let blocked = network::take_blocked().expect("what the operation waits for");
                let limit = blocked.timeout.unwrap_or(TIMEOUT).min(TIMEOUT);
                let left = limit.saturating_sub(started.elapsed());
                if left.is_zero() {
                    return Err(network::timeout_message(limit));
                }
                network::wait_any(&[blocked.wait], Some(left));
            }
            Err((_, message)) => return Err(message),
            Ok(value) => return Ok(value),
        }
    }
}

/// A connection, closed when dropped.
struct Connection(i64);

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = streams::close(self.0);
    }
}

/// Sends a request and reads the whole answer. `headers` go with the ones
/// every request has: the host, the length of the body, and who asks.
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Response, String> {
    let address = address(url)?;
    let connection = Connection(
        streams::connect(
            &address.host,
            address.port as i64,
            TIMEOUT.as_nanos() as i64,
        )
        .map_err(|(_, message)| message)?,
    );
    let id = connection.0;
    finish(|| streams::connect_finish(id))?;
    let _ = streams::set_timeout(id, TIMEOUT.as_nanos() as i64);
    if address.secure {
        finish(|| streams::secure(id, &address.host))?;
    }
    let host = if address.port == if address.secure { 443 } else { 80 } {
        address.host.clone()
    } else {
        format!("{}:{}", address.host, address.port)
    };
    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: sayon/{}\r\nConnection: close\r\nContent-Length: {}\r\n",
        address.target,
        env!("CARGO_PKG_VERSION"),
        body.len()
    );
    for (name, value) in headers {
        if value.contains(['\r', '\n']) {
            return Err(format!("the header {name} cannot hold a line break"));
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut message = head.into_bytes();
    message.extend_from_slice(body);
    finish(|| streams::write(id, &message))?;
    finish(|| streams::flush(id))?;

    let line = || -> Result<String, String> {
        finish(|| streams::read_line(id, 16 * 1024))?
            .ok_or_else(|| format!("{} closed the connection before answering", address.host))
    };
    let status_line = line()?;
    let status = status_line
        .strip_prefix("HTTP/1.")
        .and_then(|rest| rest.get(2..5))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("{} did not answer in HTTP", address.host))?;
    let mut headers = Vec::new();
    loop {
        let field = line()?;
        if field.is_empty() {
            break;
        }
        if let Some((name, value)) = field.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let mut response = Response {
        status,
        headers,
        body: Vec::new(),
    };
    let too_long = || format!("the answer of {} is longer than Sayon reads", address.host);
    let read_exactly = |count: usize, into: &mut Vec<u8>| -> Result<(), String> {
        let mut left = count;
        while left > 0 {
            let part = finish(|| streams::read_bytes(id, left.min(1 << 16) as i64))?;
            if part.is_empty() {
                return Err(format!("{} ended its answer early", address.host));
            }
            left -= part.len();
            into.extend_from_slice(&part);
        }
        Ok(())
    };
    let chunked = response
        .header("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
    let length = response.header("content-length").map(str::to_string);
    if method == "HEAD" || status == 204 || status == 304 {
        return Ok(response);
    }
    if chunked {
        loop {
            let size_line = line()?;
            let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| format!("{} sent a damaged answer", address.host))?;
            if size == 0 {
                while !line()?.is_empty() {}
                break;
            }
            if response.body.len() + size > LARGEST {
                return Err(too_long());
            }
            read_exactly(size, &mut response.body)?;
            line()?;
        }
    } else if let Some(length) = length {
        let length: usize = length
            .parse()
            .map_err(|_| format!("{} sent a damaged answer", address.host))?;
        if length > LARGEST {
            return Err(too_long());
        }
        read_exactly(length, &mut response.body)?;
    } else {
        loop {
            let part = finish(|| streams::read_bytes(id, 1 << 16))?;
            if part.is_empty() {
                break;
            }
            response.body.extend_from_slice(&part);
            if response.body.len() > LARGEST {
                return Err(too_long());
            }
        }
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_read() {
        let a = address("https://registry.example.com/api/v1/search?q=json").unwrap();
        assert!(a.secure);
        assert_eq!((a.host.as_str(), a.port), ("registry.example.com", 443));
        assert_eq!(a.target, "/api/v1/search?q=json");
        let b = address("http://127.0.0.1:4100").unwrap();
        assert_eq!(
            (b.host.as_str(), b.port, b.target.as_str()),
            ("127.0.0.1", 4100, "/")
        );
        let c = address("http://[::1]:8080/x").unwrap();
        assert_eq!((c.host.as_str(), c.port), ("::1", 8080));
        assert!(address("ftp://x").is_err());
    }
}
