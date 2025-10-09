//! Manual-reset event abstraction used by the ring.

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

impl Error for EventError {}
