//! Real SIGPROF deliveries, including a recycled native pthread whose previous
//! bound Python state remains allocated. No timers/no sampling tolerance here:
//! every delivered period must appear as a complete stack or one exact drop.

use super::*;
use pyroscope_ringbuffer::PopError;
use pyroscope_signal_timer::{SignalHandler, Timer};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;

static PERIODS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn sample(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    unwind::handler(unwind::SampleMetadata {
        count: PERIODS.load(Ordering::Relaxed),
    });
}

fn emit(periods: usize) {
    PERIODS.store(periods, Ordering::Relaxed);
    assert_eq!(unsafe { libc::raise(Timer::SIGNAL) }, 0);
}

#[pyfunction]
fn native_sample(py: Python<'_>) {
    // A signal inside native work with the GIL released must retain both frames.
    py.detach(|| emit(11));
}

fn expect_drop(periods: usize) {
    emit(periods);
    let mut frames = [Frame::default(); MAX_FRAMES];
    let mut reader = unwind::consumer().unwrap();
    assert_eq!(
        reader.pop_slice(&mut frames),
        Err(PopError::Empty),
        "unexpected raw profile: {frames:?}"
    );
    let drops = unwind::METRICS.take_drop_counts();
    assert_eq!(
        drops,
        drops.map(|(reason, _)| (
            reason,
            if reason == "no_thread_state" {
                periods
            } else {
                0
            }
        )),
        "full drops: {drops:?}"
    );
    // Also exercise the actual drop-to-pprof accounting at a known sample rate.
    emit(periods);
    let mut buffer = crate::signal_backend::ProfileBuffer::new();
    let range = crate::utils::TimeRange::new(
        std::time::UNIX_EPOCH,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1),
    )
    .unwrap();
    let profile = buffer.report(100, &range).unwrap();
    let stacks: Vec<_> = profile
        .sample
        .iter()
        .map(|s| {
            let frames: Vec<_> = s
                .location_id
                .iter()
                .rev()
                .flat_map(|id| {
                    profile
                        .location
                        .iter()
                        .find(|l| l.id == *id)
                        .unwrap()
                        .line
                        .iter()
                        .map(|l| {
                            let f = profile
                                .function
                                .iter()
                                .find(|f| f.id == l.function_id)
                                .unwrap();
                            (
                                profile.string_table[f.name as usize].as_str(),
                                profile.string_table[f.filename as usize].as_str(),
                                l.line,
                            )
                        })
                })
                .collect();
            (frames, s.value.clone())
        })
        .collect();
    assert_eq!(
        stacks,
        [(
            vec![("[dropped: no_thread_state]", "", 0)],
            vec![periods as i64 * 10_000_000]
        )],
        "full profile: {profile:#?}"
    );
}

unsafe fn call_and_check(callable: usize) {
    let result = unsafe { pyo3::ffi::PyObject_CallNoArgs(callable as *mut _) };
    assert!(!result.is_null());
    unsafe { pyo3::ffi::Py_DECREF(result) };
    let mut raw = [Frame::default(); MAX_FRAMES];
    let mut reader = unwind::consumer().unwrap();
    let (n, metadata) = reader.pop_slice(&mut raw).unwrap();
    assert_eq!(metadata.count, 11, "raw profile: {:?}", &raw[..n]);
    let mut strings = StringTable::new();
    let mut resolver = Resolver::<PYO3Types>::new(Lines::NoLine);
    let resolved = resolver
        .resolve_stack(&raw[..n], metadata.count, &mut strings)
        .unwrap();
    let expected = [
        ("foreign_root".to_owned(), "mirror.py".to_owned(), 0),
        ("foreign_leaf".to_owned(), "mirror.py".to_owned(), 0),
    ];
    assert_profile_weight(resolved, strings, &expected, 11).unwrap();
    assert_eq!(
        reader.pop_slice(&mut raw),
        Err(PopError::Empty),
        "full raw profile: {raw:?}"
    );
    let drops = unwind::METRICS.take_drop_counts();
    assert!(
        drops.iter().all(|(_, value)| *value == 0),
        "full drops: {drops:?}"
    );
}

#[test]
fn signals_do_not_reuse_abandoned_or_deleted_thread_states() {
    let _guard = crate::signal_sampler::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    unsafe { kindasafe_init::init() }.unwrap();
    Python::attach(|py| {
        let _mirror = crate::signal_sampler::EvalMirror::install().unwrap();
        let _handler = SignalHandler::install(Timer::SIGNAL, sample).unwrap();
        unwind::METRICS.take_drop_counts();
        let module = module(
            py,
            "def foreign_root():\n    return foreign_leaf()\ndef foreign_leaf():\n    return native_sample()\n",
            "mirror.py",
            "signal_mirror",
        );
        module
            .add_function(wrap_pyfunction!(native_sample, &module).unwrap())
            .unwrap();
        let callable = module.getattr("foreign_root").unwrap();
        let address = callable.as_ptr() as usize;
        let (identity, abandoned) = py.detach(|| {
            std::thread::spawn(move || unsafe {
                expect_drop(3);
                let _gil = pyo3::ffi::PyGILState_Ensure();
                call_and_check(address);
                // Intentionally retain a bound state when the OS thread exits.
                let state = pyo3::ffi::PyEval_SaveThread();
                (libc::pthread_self() as usize, state as usize)
            })
            .join()
            .unwrap()
        });
        let mut reused = false;
        for _ in 0..128 {
            reused = py.detach(|| {
                std::thread::spawn(move || unsafe {
                    // A new thread starts with empty SDK TSD even if its IDs and stack
                    // allocation are recycled while the old Python state is linked.
                    expect_drop(5);
                    if libc::pthread_equal(libc::pthread_self(), identity as libc::pthread_t) == 0 {
                        return false;
                    }
                    for _ in 0..16 {
                        let gil = pyo3::ffi::PyGILState_Ensure();
                        call_and_check(address);
                        pyo3::ffi::PyGILState_Release(gil);
                        // The native thread is still alive; the capsule must have
                        // invalidated its mirror before the Python state was freed.
                        expect_drop(7);
                    }
                    true
                })
                .join()
                .unwrap()
            });
            if reused {
                break;
            }
        }
        assert!(
            reused,
            "test must observe actual native pthread identity reuse"
        );
        // Deliberately keep abandoned linked until process exit. CPython requires
        // a bound state's owning thread to unbind it; deleting it here would
        // clear this thread's GILState TLS and corrupt the fixture itself.
        assert_ne!(abandoned, 0);
        eprintln!(
            "Verified actual pthread reuse, 17 complete foreign_root;foreign_leaf stacks at 11 periods each, and exact native-only/deleted-state drops"
        );
    });
}

#[test]
fn retained_thread_dictionary_cannot_keep_a_deleted_state_bound() {
    let _guard = crate::signal_sampler::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    unsafe { kindasafe_init::init() }.unwrap();
    Python::attach(|py| {
        let _mirror = crate::signal_sampler::EvalMirror::install().unwrap();
        let _handler = SignalHandler::install(Timer::SIGNAL, sample).unwrap();
        unwind::METRICS.take_drop_counts();
        let module = module(
            py,
            "def foreign_root():\n    return foreign_leaf()\ndef foreign_leaf():\n    return native_sample()\n",
            "mirror.py",
            "retained_dict_mirror",
        );
        module
            .add_function(wrap_pyfunction!(native_sample, &module).unwrap())
            .unwrap();
        let callable = module.getattr("foreign_root").unwrap();
        let address = callable.as_ptr() as usize;
        py.detach(|| std::thread::spawn(move || unsafe {
            let mut retained = Vec::new();
            let mut states = std::collections::HashSet::new();
            let mut reused_addresses = 0;
            for _ in 0..64 {
                let gil = pyo3::ffi::PyGILState_Ensure();
                let state = pyo3::ffi::PyThreadState_Get() as usize;
                reused_addresses += usize::from(!states.insert(state));
                call_and_check(address);
                // A legal extension-owned strong reference outlives Clear/Delete.
                let dict = pyo3::ffi::PyThreadState_GetDict();
                assert!(!dict.is_null());
                pyo3::ffi::Py_INCREF(dict);
                retained.push(dict as usize);
                pyo3::ffi::PyGILState_Release(gil);
                assert!(pyo3::ffi::PyGILState_GetThisThreadState().is_null(), "fixture must delete the Python state while the OS thread and its dictionary survive");
                expect_drop(13);
            }
            let gil = pyo3::ffi::PyGILState_Ensure();
            for dict in retained {
                pyo3::ffi::Py_DECREF(dict as *mut _);
            }
            pyo3::ffi::PyGILState_Release(gil);
            expect_drop(17);
            eprintln!("Retained 64 thread dictionaries past state deletion; observed {reused_addresses} recycled state addresses; checked 64 full 11-period stacks and exact 13/17-period no-state profiles");
        }).join().unwrap());
    });
}

static READY: AtomicBool = AtomicBool::new(false);
static PROCEED: AtomicBool = AtomicBool::new(false);

#[pyfunction]
fn wait_for_installation(py: Python<'_>) {
    py.detach(|| {
        READY.store(true, Ordering::Release);
        while !PROCEED.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        // This call predates installation. Until another Python evaluation
        // supplies the state, it must be an explicit loss, never a guessed stack.
        expect_drop(19);
    });
}

#[test]
fn preexisting_native_call_registers_on_next_python_evaluation() {
    let _guard = crate::signal_sampler::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    unsafe { kindasafe_init::init() }.unwrap();
    Python::attach(|py| {
        let module = module(
            py,
            "def foreign_root():\n    wait_for_installation()\n    return foreign_leaf()\ndef foreign_leaf():\n    return native_sample()\n",
            "mirror.py",
            "preexisting_mirror",
        );
        module
            .add_function(wrap_pyfunction!(native_sample, &module).unwrap())
            .unwrap();
        module
            .add_function(wrap_pyfunction!(wait_for_installation, &module).unwrap())
            .unwrap();
        let callable = module.getattr("foreign_root").unwrap();
        let address = callable.as_ptr() as usize;
        let worker = std::thread::spawn(move || unsafe {
            let gil = pyo3::ffi::PyGILState_Ensure();
            call_and_check(address);
            pyo3::ffi::PyGILState_Release(gil);
            expect_drop(23);
        });
        py.detach(|| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !READY.load(Ordering::Acquire) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "worker never entered native wait"
                );
                std::thread::yield_now();
            }
        });
        let _mirror = crate::signal_sampler::EvalMirror::install().unwrap();
        let _handler = SignalHandler::install(Timer::SIGNAL, sample).unwrap();
        unwind::METRICS.take_drop_counts();
        PROCEED.store(true, Ordering::Release);
        py.detach(|| worker.join().unwrap());
    });
}

#[test]
fn conflicting_evaluator_is_refused_and_shutdown_restores_default() {
    unsafe extern "C" fn other(
        _: *mut pyo3::ffi::PyThreadState,
        _: *mut pyo3::ffi::_PyInterpreterFrame,
        _: libc::c_int,
    ) -> *mut pyo3::ffi::PyObject {
        unreachable!("the conflict fixture must never execute Python");
    }
    let _guard = crate::signal_sampler::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    Python::attach(|_| unsafe {
        let interpreter = pyo3::ffi::PyInterpreterState_Main();
        let original = pyo3::ffi::_PyInterpreterState_GetEvalFrameFunc(interpreter);
        pyo3::ffi::_PyInterpreterState_SetEvalFrameFunc(interpreter, Some(other));
        let error = unwind::eval_mirror::install();
        let after = pyo3::ffi::_PyInterpreterState_GetEvalFrameFunc(interpreter);
        pyo3::ffi::_PyInterpreterState_SetEvalFrameFunc(interpreter, Some(original));
        assert_eq!(
            error,
            Err("Signal on Python 3.15 requires the default frame evaluator (PEP 523 conflict)")
        );
        assert!(std::ptr::fn_addr_eq(
            after,
            other as pyo3::ffi::_PyFrameEvalFunction
        ));
        for _ in 0..2 {
            unwind::eval_mirror::install().unwrap();
            assert!(!std::ptr::fn_addr_eq(
                pyo3::ffi::_PyInterpreterState_GetEvalFrameFunc(interpreter),
                original
            ));
            unwind::eval_mirror::uninstall();
            assert!(std::ptr::fn_addr_eq(
                pyo3::ffi::_PyInterpreterState_GetEvalFrameFunc(interpreter),
                original
            ));
        }
    });
}

#[cfg(target_os = "linux")]
#[test]
fn fork_child_does_not_wait_for_a_vanished_reader() {
    // Simulate a walk already in flight when shutdown removes the handler.
    // Only the reader pin is prolonged; real walks do bounded, nonblocking work.
    unsafe extern "C" {
        fn pyroscope_mirror_acquire(token: *mut *mut libc::c_void)
        -> *mut pyo3::ffi::PyThreadState;
        fn pyroscope_mirror_release(token: *mut libc::c_void);
    }
    let _guard = crate::signal_sampler::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    unsafe { kindasafe_init::init() }.unwrap();
    Python::attach(|py| {
        let mirror = crate::signal_sampler::EvalMirror::install().unwrap();
        let handler = SignalHandler::install(Timer::SIGNAL, sample).unwrap();
        unwind::METRICS.take_drop_counts();
        let module = module(
            py,
            "def foreign_root():\n    return foreign_leaf()\ndef foreign_leaf():\n    return native_sample()\n",
            "mirror.py",
            "fork_mirror",
        );
        module
            .add_function(wrap_pyfunction!(native_sample, &module).unwrap())
            .unwrap();
        let callable = module.getattr("foreign_root").unwrap();
        let address = callable.as_ptr() as usize;
        let state_ready = std::sync::Arc::new(AtomicUsize::new(0));
        let release = std::sync::Arc::new(AtomicBool::new(false));
        let worker = {
            let state_ready = state_ready.clone();
            let release = release.clone();
            std::thread::spawn(move || unsafe {
                let gil = pyo3::ffi::PyGILState_Ensure();
                call_and_check(address);
                let state = pyo3::ffi::PyEval_SaveThread();
                let mut token = std::ptr::null_mut();
                assert_eq!(pyroscope_mirror_acquire(&mut token), state);
                state_ready.store(state as usize, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                pyroscope_mirror_release(token);
                pyo3::ffi::PyEval_RestoreThread(state);
                pyo3::ffi::PyGILState_Release(gil);
            })
        };
        let state = py.detach(|| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let state = state_ready.load(Ordering::Acquire);
                if state != 0 {
                    break state;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
        });
        drop(handler);
        drop(mirror);
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            unsafe {
                libc::alarm(5);
                // Clear runs the sentinel callback. Its parent-side reader
                // does not exist here, and must neither block nor leak an owner.
                pyo3::ffi::PyThreadState_Clear(state as *mut _);
                libc::_exit(0);
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        release.store(true, Ordering::Release);
        py.detach(|| worker.join().unwrap());
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "child status {status}: inherited reader blocked teardown"
        );
    });
}
