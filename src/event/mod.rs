//! Manual-reset event implementation for shared memory.
//!
//! The interface mirrors the subset of `raw_sync::events::Event` required by
//! the ring implementation: manual reset, cross-process, and wait with an
//! optional timeout.  Backends are platform-specific; Linux uses futexes,
//! other Unix platforms fall back to a lightweight polling loop.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::ManualResetEvent;

#[cfg(not(target_os = "linux"))]
mod fallback;
#[cfg(not(target_os = "linux"))]
pub use fallback::ManualResetEvent;

use std::error::Error;
use std::fmt;

/// Event error.
#[derive(Debug)]
pub enum EventError {
    Timeout,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Io(std::io::Error),
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "wait timed out"),
            Self::Io(err) => write!(f, "event error: {err}"),
        }
    }
}

impl Error for EventError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeout => None,
            Self::Io(err) => Some(err),
        }
    }
}
