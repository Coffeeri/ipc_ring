use super::EventError;
use std::convert::TryFrom;
use std::io;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

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

    pub unsafe fn new(ptr: *mut u8, _manual_reset: bool) -> (Self, usize) {
        let mem = ptr.cast::<EventMem>();
        ptr::write(
            mem,
            EventMem {
                state: AtomicU32::new(0),
            },
        );
        (Self { mem }, size_of::<EventMem>())
    }

    pub unsafe fn from_existing(ptr: *mut u8) -> (Self, usize) {
        let mem = ptr.cast::<EventMem>();
        (Self { mem }, size_of::<EventMem>())
    }

    #[inline]
    fn state(&self) -> &AtomicU32 {
        unsafe { &(*self.mem).state }
    }

    pub fn signal(&self) -> Result<(), EventError> {
        self.state().store(1, Ordering::Release);
        futex_wake(self.state(), i32::MAX)
    }

    pub fn wait(&self, timeout: Option<Duration>) -> Result<(), EventError> {
        loop {
            if self.state().swap(0, Ordering::AcqRel) != 0 {
                return Ok(());
            }

            match futex_wait(self.state(), timeout) {
                Ok(()) => {}
                Err(EventError::Timeout) => return Err(EventError::Timeout),
                Err(EventError::Io(err)) => return Err(EventError::Io(err)),
            }
        }
    }
}

fn futex_wait(word: &AtomicU32, timeout: Option<Duration>) -> Result<(), EventError> {
    let mut timespec_storage = timeout.map(duration_to_timespec);
    let ts_ptr = timespec_storage
        .as_mut()
        .map_or(ptr::null_mut(), std::ptr::from_mut);

    loop {
        let res = unsafe {
            libc::syscall(
                libc::SYS_futex,
                std::ptr::from_ref(word).cast::<u32>(),
                libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                0_u32,
                ts_ptr,
            )
        };

        if res == 0 {
            return Ok(());
        }

        let errno = io::Error::last_os_error();
        if matches!(errno.raw_os_error(), Some(libc::EINTR)) {
            continue;
        }

        return match errno.raw_os_error() {
            Some(libc::EAGAIN) => Ok(()),
            Some(libc::ETIMEDOUT) => Err(EventError::Timeout),
            _ => Err(EventError::Io(errno)),
        };
    }
}

fn futex_wake(word: &AtomicU32, count: i32) -> Result<(), EventError> {
    let res = unsafe {
        libc::syscall(
            libc::SYS_futex,
            std::ptr::from_ref(word).cast::<u32>(),
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
    let secs = dur.as_secs();
    let sec_val = libc::time_t::try_from(secs).unwrap_or(libc::time_t::MAX);

    let nanos = dur.subsec_nanos();
    let nanos_u64 = u64::from(nanos);
    let nsec_val = if nanos_u64 > libc::c_long::MAX as u64 {
        libc::c_long::MAX
    } else {
        libc::c_long::try_from(nanos_u64).unwrap_or(libc::c_long::MAX)
    };

    libc::timespec {
        tv_sec: sec_val,
        tv_nsec: nsec_val,
    }
}
