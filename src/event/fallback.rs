use super::{EventError, EventState, Timeout};
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

    #[allow(clippy::unnecessary_wraps)]
    pub unsafe fn new(ptr: *mut u8, _manual_reset: bool) -> Result<(Self, usize), EventError> {
        let mem = ptr.cast::<EventMem>();
        std::ptr::write(
            mem,
            EventMem {
                state: AtomicU32::new(0),
            },
        );
        Ok((Self { mem }, Self::size_of()))
    }

    #[allow(clippy::unnecessary_wraps)]
    pub unsafe fn from_existing(ptr: *mut u8) -> Result<(Self, usize), EventError> {
        let mem = ptr.cast::<EventMem>();
        Ok((Self { mem }, Self::size_of()))
    }

    #[inline]
    fn state(&self) -> &AtomicU32 {
        unsafe { &(*self.mem).state }
    }

    #[allow(clippy::unnecessary_wraps)]
    pub fn set(&self, state: EventState) -> Result<(), EventError> {
        match state {
            EventState::Clear => self.state().store(0, Ordering::Release),
            EventState::Signaled => self.state().store(1, Ordering::Release),
        }
        Ok(())
    }

    pub fn wait(&self, timeout: Timeout) -> Result<(), EventError> {
        match timeout {
            Timeout::Infinite => loop {
                if self.state().load(Ordering::Acquire) != 0 {
                    return Ok(());
                }
                thread::sleep(SLEEP_STEP);
            },
            Timeout::Val(duration) => {
                let start = Instant::now();
                loop {
                    if self.state().load(Ordering::Acquire) != 0 {
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
