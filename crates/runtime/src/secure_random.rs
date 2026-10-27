//! Random bytes from the system's secure source, for `standard/crypto`:
//! `BCryptGenRandom` with the system's preferred generator on Windows,
//! `/dev/urandom` on Linux and Android, `random_get` under WASI, and the
//! browser's `crypto.getRandomValues` in a web page. Unlike `math.random`,
//! they are fit for keys and tokens.

use crate::host::HostError;

/// The most bytes one call gives, so that a mistaken count cannot take
/// all of memory.
pub const LARGEST: usize = 1 << 20;

/// `count` random bytes.
pub fn bytes(count: i64) -> Result<Vec<u8>, HostError> {
    if count < 0 || count as u64 > LARGEST as u64 {
        return Err((
            "ValueError",
            format!("random bytes come 0 to {LARGEST} at a time, not {count}"),
        ));
    }
    let mut out = vec![0u8; count as usize];
    fill(&mut out)?;
    Ok(out)
}

// A web page's source never fails.
#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), allow(dead_code))]
fn failed(detail: impl std::fmt::Display) -> HostError {
    (
        "IOError",
        format!("the system's secure random source failed: {detail}"),
    )
}

#[cfg(windows)]
fn fill(bytes: &mut [u8]) -> Result<(), HostError> {
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x2;
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            algorithm: *mut std::ffi::c_void,
            buffer: *mut u8,
            length: u32,
            flags: u32,
        ) -> i32;
    }
    for chunk in bytes.chunks_mut(u32::MAX as usize) {
        // SAFETY: the chunk is writable for its length, and the system's
        // preferred generator needs no algorithm handle.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            return Err(failed(format!("status {status:#x}")));
        }
    }
    Ok(())
}

#[cfg(all(unix, not(target_family = "wasm")))]
fn fill(bytes: &mut [u8]) -> Result<(), HostError> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(bytes))
        .map_err(failed)
}

#[cfg(all(target_family = "wasm", target_os = "wasi"))]
fn fill(bytes: &mut [u8]) -> Result<(), HostError> {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    unsafe extern "C" {
        fn random_get(buffer: *mut u8, length: usize) -> u16;
    }
    // SAFETY: the bytes are writable for their length.
    let errno = unsafe { random_get(bytes.as_mut_ptr(), bytes.len()) };
    if errno != 0 {
        return Err(failed(format!("WASI error {errno}")));
    }
    Ok(())
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn fill(bytes: &mut [u8]) -> Result<(), HostError> {
    // The browser gives 65536 bytes at most each time it is asked.
    for chunk in bytes.chunks_mut(65536) {
        crate::console::web::random_bytes(chunk);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_come_as_asked() {
        assert!(bytes(0).unwrap().is_empty());
        let first = bytes(32).unwrap();
        let second = bytes(32).unwrap();
        assert_eq!(first.len(), 32);
        // Two draws of 256 bits are equal with probability 2^-256.
        assert_ne!(first, second);
        assert!(bytes(-1).is_err());
        assert!(bytes(LARGEST as i64 + 1).is_err());
    }
}
