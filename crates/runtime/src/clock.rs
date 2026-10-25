//! Clocks: the system's, or in a web page the host's, since Rust's clocks
//! are not there (`std::time::Instant::now` fails on a bare WebAssembly
//! module).

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use std::time::Instant;

/// Nanoseconds since the start of 1970 in UTC, negative before it.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn unix_nanoseconds() -> i64 {
    let now = std::time::SystemTime::now();
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => after.as_nanos().min(i64::MAX as u128) as i64,
        Err(before) => -(before.duration().as_nanos().min(i64::MAX as u128) as i64),
    }
}

/// A seed for the program's random numbers, never 0.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn random_seed() -> u64 {
    unix_nanoseconds() as u64 | 1
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub use web::{Instant, random_seed, unix_nanoseconds};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod web {
    use crate::console::web::{monotonic_milliseconds, random_bytes, unix_milliseconds};
    use std::time::Duration;

    /// A moment on the host's monotonic clock: the time since its origin.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(Duration);

    impl Instant {
        pub fn now() -> Instant {
            Instant(Duration::from_secs_f64(
                monotonic_milliseconds().max(0.0) / 1000.0,
            ))
        }

        pub fn duration_since(&self, earlier: Instant) -> Duration {
            self.0.saturating_sub(earlier.0)
        }

        pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            self.0.saturating_sub(earlier.0)
        }

        pub fn elapsed(&self) -> Duration {
            Instant::now().saturating_duration_since(*self)
        }
    }

    impl std::ops::Add<Duration> for Instant {
        type Output = Instant;

        fn add(self, duration: Duration) -> Instant {
            Instant(self.0 + duration)
        }
    }

    pub fn unix_nanoseconds() -> i64 {
        (unix_milliseconds() * 1_000_000.0) as i64
    }

    pub fn random_seed() -> u64 {
        let mut bytes = [0u8; 8];
        random_bytes(&mut bytes);
        u64::from_le_bytes(bytes) | 1
    }
}
