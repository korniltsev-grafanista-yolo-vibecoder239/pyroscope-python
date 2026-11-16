//! Read CPython 3.15's bound-state TLS without invoking its allocating TLS resolver.
//! Never infer thread-state ownership from a pthread ID: CPython can retain bound
//! states after thread exit, and pthread IDs can be reused by non-Python threads.

use super::{Error, read_usize};
use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

pub(super) const MAX_TLS_BYTES: usize = 1 << 20;

// Publish the entire location in one lock-free word, never a thread state.
static LOCATION: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareError {
    NoBoundState,
    UnsupportedTls,
    AmbiguousTls,
    Read,
}

/// Locate the bound-state TLS slot on a normal thread, before installing a timer.
/// Discovery rejects unsupported or ambiguous layouts instead of guessing.
///
/// # Safety
/// CPython must be initialized. The calling thread must have a bound state and
/// libpython must remain loaded throughout sampling. If the caller holds the GIL,
/// this temporarily releases it, then restores it before returning.
/// Sampling must be stopped while preparing the lookup.
/// Recovery must already be initialized, with SIGSEGV/SIGBUS unblocked for startup reads.
pub unsafe fn prepare() -> Result<(), PrepareError> {
    struct Restore(*mut pyo3_ffi::PyThreadState);
    impl Drop for Restore {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { pyo3_ffi::PyEval_RestoreThread(self.0) };
            }
        }
    }
    let attached = unsafe { pyo3_ffi::PyThreadState_GetUnchecked() };
    let _restore = Restore(if attached.is_null() {
        core::ptr::null_mut()
    } else {
        unsafe { pyo3_ffi::PyEval_SaveThread() }
    });
    // Only call the TLS-resolving Python API here, on a normal thread. Detaching
    // above distinguishes the bound slot from the now-null attached-state slot.
    let bound = unsafe { pyo3_ffi::PyGILState_GetThisThreadState() } as usize;
    if bound == 0 {
        return Err(PrepareError::NoBoundState);
    }
    let location = platform::locate(bound)?;
    if platform::read(location).map_err(|_| PrepareError::Read)? != bound {
        return Err(PrepareError::UnsupportedTls);
    }
    LOCATION.store(location, Ordering::Release);
    Ok(())
}

pub(super) fn current() -> Result<usize, Error> {
    platform::read(LOCATION.load(Ordering::Acquire))
}

#[cfg(any(target_os = "linux", all(test, not(miri))))]
fn find_offset(base: usize, size: usize, bound: usize) -> Result<usize, PrepareError> {
    if size > MAX_TLS_BYTES {
        return Err(PrepareError::UnsupportedTls);
    }
    let mut found = None;
    for offset in (0..size.saturating_sub(7)).step_by(8) {
        if read_usize(base, offset).map_err(|_| PrepareError::Read)? == bound {
            if found.is_some() {
                return Err(PrepareError::AmbiguousTls);
            }
            found = Some(offset);
        }
    }
    found.ok_or(PrepareError::UnsupportedTls)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn discovery_requires_one_complete_matching_word() {
        unsafe { kindasafe_init::init() }.unwrap();
        let words = [0usize, 42, 0, 42];
        let base = words.as_ptr() as usize;
        assert_eq!(find_offset(base, 24, 42), Ok(8));
        assert_eq!(find_offset(base, 15, 42), Err(PrepareError::UnsupportedTls));
        assert_eq!(find_offset(base, 32, 42), Err(PrepareError::AmbiguousTls));
        assert_eq!(
            find_offset(base, MAX_TLS_BYTES + 1, 42),
            Err(PrepareError::UnsupportedTls)
        );
    }
}
