use super::{EventError, EventState, Timeout};
use std::io;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::{ptr, time::Duration};

#[repr(C)]
struct EventMem {
    state: AtomicU32,
}

pub struct ManualResetEvent {
    mem: *mut EventMem,
}

unsafe impl Send for ManualResetEvent {}
unsafe impl Sync for ManualResetEvent {}

impl ManualResetEvent {
    pub fn size_of() -> usize {
        size_of::<EventMem>()
    }

    pub unsafe fn new(ptr: *mut u8, _manual_reset: bool) -> Result<(Self, usize), EventError> {
        let mem = ptr.cast::<EventMem>();
        ptr::write(
            mem,
            EventMem {
                state: AtomicU32::new(0),
            },
        );
        Ok((Self { mem }, size_of::<EventMem>()))
    }

    pub unsafe fn from_existing(ptr: *mut u8) -> Result<(Self, usize), EventError> {
        let mem = ptr.cast::<EventMem>();
        Ok((Self { mem }, size_of::<EventMem>()))
    }

    #[inline]
    fn state(&self) -> &AtomicU32 {
        unsafe { &(*self.mem).state }
    }

    pub fn set(&self, state: EventState) -> Result<(), EventError> {
        match state {
            EventState::Clear => {
                self.state().store(0, Ordering::Release);
            }
            EventState::Signaled => {
                self.state().store(1, Ordering::Release);
                futex_wake(self.state(), i32::MAX)?;
            }
        }
        Ok(())
    }

    pub fn wait(&self, timeout: Timeout) -> Result<(), EventError> {
        loop {
            if self.state().load(Ordering::Acquire) != 0 {
                return Ok(());
            }

            let futex_timeout = match timeout {
                Timeout::Infinite => None,
                Timeout::Val(dur) => Some(dur),
            };

            match futex_wait(self.state(), futex_timeout) {
                Ok(()) => continue,
                Err(WaitResult::Awoken) => continue,
                Err(WaitResult::Timeout) => return Err(EventError::Timeout),
                Err(WaitResult::Io(err)) => return Err(EventError::Io(err)),
            }
        }
    }
}

enum WaitResult {
    Awoken,
    Timeout,
    Io(io::Error),
}

fn futex_wait(word: &AtomicU32, timeout: Option<Duration>) -> Result<(), WaitResult> {
    let mut timespec_storage = timeout.map(duration_to_timespec);
    let ts_ptr = timespec_storage
        .as_mut()
        .map(|ts| ts as *mut libc::timespec)
        .unwrap_or(ptr::null_mut());

    loop {
        let res = unsafe {
            libc::syscall(
                libc::SYS_futex,
                word as *const AtomicU32 as *const u32,
                libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                0_u32,
                ts_ptr,
            )
        };

        if res == 0 {
            return Ok(());
        }

        let errno = io::Error::last_os_error();
        match errno.raw_os_error() {
            Some(libc::EAGAIN) => return Ok(()),
            Some(libc::EINTR) => continue,
            Some(libc::ETIMEDOUT) => return Err(WaitResult::Timeout),
            _ => return Err(WaitResult::Io(errno)),
        }
    }
}

fn futex_wake(word: &AtomicU32, count: i32) -> Result<(), EventError> {
    let res = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word as *const AtomicU32 as *const u32,
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            count,
        )
    };
    if res >= 0 {
        Ok(())
    } else {
        Err(EventError::Io(io::Error::last_os_error()))
    }
}

fn duration_to_timespec(dur: Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: dur.as_secs() as libc::time_t,
        tv_nsec: dur.subsec_nanos() as libc::c_long,
    }
}
