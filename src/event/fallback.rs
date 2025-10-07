use super::EventError;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[repr(C)]
struct EventMem {
    state: AtomicU32,
}

pub struct ManualResetEvent {
    mem: *mut EventMem,
}

unsafe impl Send for ManualResetEvent {}
unsafe impl Sync for ManualResetEvent {}

const SLEEP_STEP: Duration = Duration::from_micros(200);

impl ManualResetEvent {
    pub fn size_of() -> usize {
        std::mem::size_of::<EventMem>()
    }

    pub unsafe fn new(ptr: *mut u8, _manual_reset: bool) -> (Self, usize) {
        let mem = ptr.cast::<EventMem>();
        std::ptr::write(
            mem,
            EventMem {
                state: AtomicU32::new(0),
            },
        );
        (Self { mem }, Self::size_of())
    }

    pub unsafe fn from_existing(ptr: *mut u8) -> (Self, usize) {
        let mem = ptr.cast::<EventMem>();
        (Self { mem }, Self::size_of())
    }

    #[inline]
    fn state(&self) -> &AtomicU32 {
        unsafe { &(*self.mem).state }
    }

    #[allow(clippy::unnecessary_wraps)]
    pub fn signal(&self) -> Result<(), EventError> {
        self.state().store(1, Ordering::Release);
        Ok(())
    }

    pub fn wait(&self, timeout: Option<Duration>) -> Result<(), EventError> {
        match timeout {
            None => loop {
                if self.state().swap(0, Ordering::AcqRel) != 0 {
                    return Ok(());
                }
                thread::sleep(SLEEP_STEP);
            },
            Some(duration) => {
                let start = Instant::now();
                loop {
                    if self.state().swap(0, Ordering::AcqRel) != 0 {
                        return Ok(());
                    }
                    if start.elapsed() >= duration {
                        return Err(EventError::Timeout);
                    }
                    let remaining = duration.saturating_sub(start.elapsed());
                    thread::sleep(remaining.min(SLEEP_STEP));
                }
            }
        }
    }
}
