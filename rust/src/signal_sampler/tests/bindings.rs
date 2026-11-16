//! Real signals and CPython lifetimes, including an intentionally abandoned bound state.

use pyo3::{ffi, prelude::*, wrap_pyfunction};
use pyroscope_pprof::StringTable;
use pyroscope_python_resolve::{Lines, PYO3Types, Resolver};
use pyroscope_python_unwind::{self as unwind, Frame, MAX_FRAMES};
use pyroscope_signal_timer::SignalHandler;
use std::{collections::BTreeMap, sync::mpsc};

type Profile = BTreeMap<Vec<String>, usize>;

type EpochPause = (mpsc::Sender<()>, mpsc::Receiver<()>);
static EPOCH_PAUSE: std::sync::Mutex<Option<EpochPause>> = std::sync::Mutex::new(None);

#[pyfunction]
fn epoch_pause(py: Python<'_>) {
    let (ready, resume) = EPOCH_PAUSE.lock().unwrap().take().unwrap();
    py.detach(move || {
        ready.send(()).unwrap();
        resume.recv().unwrap();
        // A Python caller remains active while this native function spans restart.
        emit_samples(11);
    });
}

#[pyfunction]
fn native_reentry(py: Python<'_>, callback: &Bound<'_, PyAny>) -> PyResult<()> {
    callback.call0()?;
    // No Python line/start event occurs between the nested return and this work.
    py.detach(|| emit_samples(13));
    Ok(())
}

fn observed() -> Profile {
    let mut profile = Profile::new();
    let mut consumer = unwind::consumer().unwrap();
    let mut frames = [Frame::default(); MAX_FRAMES];
    let mut strings = StringTable::new();
    let mut resolver = Resolver::<PYO3Types>::new(Lines::NoLine);
    let mut failures = Vec::new();
    while let Ok((len, metadata)) = consumer.pop_slice(&mut frames) {
        let resolved = match resolver.resolve_stack(&frames[..len], metadata.count, &mut strings) {
            Ok(resolved) => resolved,
            Err(error) => {
                failures.push(format!(
                    "{error:?}: {:?}, count={}",
                    &frames[..len],
                    metadata.count
                ));
                continue;
            }
        };
        let mut table = Vec::new();
        strings.clone_pprof_table(&mut table);
        let stack = resolved
            .iter()
            .rev()
            .map(|f| table[f.function_name.index as usize].clone())
            .collect();
        *profile.entry(stack).or_default() += metadata.count;
    }
    for (reason, count) in unwind::METRICS.take_drop_counts() {
        if count != 0 {
            profile.insert(vec![format!("[dropped: {reason}]")], count);
        }
    }
    assert!(
        failures.is_empty(),
        "resolution failures: {failures:#?}; full observed profile: {profile:#?}"
    );
    profile
}

fn expect(expected: &[(&[&str], usize)]) {
    let profile = observed();
    let expected: Profile = expected
        .iter()
        .map(|(s, n)| (s.iter().map(|s| s.to_string()).collect(), *n))
        .collect();
    // Explicit signal deliveries have no sampling noise: every whole stack and
    // every lost period must match exactly. Keep every observed stack on failure.
    assert_eq!(profile, expected, "full observed profile: {profile:#?}");
    eprintln!("full observed profile: {profile:#?}");
}

#[cfg(target_os = "linux")]
static LOOP_READY: std::sync::Mutex<Option<mpsc::Sender<libc::pid_t>>> =
    std::sync::Mutex::new(None);

#[cfg(target_os = "linux")]
#[pyfunction]
fn loop_ready() {
    LOOP_READY
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .send(pyroscope_signal_timer::gettid())
        .unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn preexisting_call_free_loop_binds_on_line_events() {
    use std::time::{Duration, Instant};
    let _guard = super::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    unsafe { kindasafe_init::init() }.unwrap();
    let (ready, wait_ready) = mpsc::channel();
    *LOOP_READY.lock().unwrap() = Some(ready);
    let (module, function) = Python::attach(|py| {
        let module = PyModule::from_code(py, c"running = True\ndef spin():\n    loop_ready()\n    value = 0\n    while running:\n        value = (value + 1) & 65535\n", c"existing_loop.py", c"existing_loop").unwrap();
        module
            .add_function(wrap_pyfunction!(loop_ready, &module).unwrap())
            .unwrap();
        let function = module.getattr("spin").unwrap().unbind();
        (module.unbind(), function)
    });
    let worker = std::thread::spawn(move || Python::attach(|py| function.call0(py).unwrap()));
    let tid = wait_ready.recv().unwrap();
    // spin is already in its call-free loop: there will be no PY_START/PY_RESUME.
    let _monitor = super::monitoring::Monitor::start().unwrap();
    let _signal = SignalHandler::install(libc::SIGPROF, super::signal_handler).unwrap();
    observed();
    let timer = pyroscope_signal_timer::Timer::start(
        pyroscope_signal_timer::Mode::CTimer,
        Duration::from_millis(5),
    )
    .unwrap();
    timer.thread_created(tid).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut profile = Profile::new();
    while profile.values().sum::<usize>() < 160 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
        for (stack, count) in observed() {
            *profile.entry(stack).or_default() += count;
        }
    }
    drop(timer);
    Python::attach(|py| module.bind(py).setattr("running", false).unwrap());
    worker.join().unwrap();
    for (stack, count) in observed() {
        *profile.entry(stack).or_default() += count;
    }
    let total: usize = profile.values().sum();
    let expected = profile.get(&vec!["spin".to_string()]).copied().unwrap_or(0);
    // At least 160 genuine timer periods, >=95% on the one complete stack.
    // Allow at most 5% initial binding/delivery noise, including all lost samples.
    assert!(
        total >= 160 && expected * 100 >= total * 95,
        "full observed profile: {profile:#?}"
    );
    eprintln!("full observed timer profile: {profile:#?}");
}

#[pyfunction]
fn emit_samples(count: usize) {
    for _ in 0..count {
        assert_eq!(unsafe { libc::raise(libc::SIGPROF) }, 0);
    }
}

fn function(py: Python<'_>) -> Py<PyAny> {
    let module = PyModule::from_code(
        py,
        c"def outer():\n    leaf()\ndef leaf():\n    emit_samples(7)\n",
        c"binding_lifetime.py",
        c"binding_lifetime",
    )
    .unwrap();
    module
        .add_function(wrap_pyfunction!(emit_samples, &module).unwrap())
        .unwrap();
    module.getattr("outer").unwrap().unbind()
}

#[test]
fn deleting_a_state_invalidates_binding_even_when_its_dictionary_is_retained() {
    let _guard = super::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    unsafe { kindasafe_init::init() }.unwrap();
    let _monitor = super::monitoring::Monitor::start().unwrap();
    let _signal = SignalHandler::install(libc::SIGPROF, super::signal_handler).unwrap();
    observed();
    let function = Python::attach(function);
    let callable = function.as_ptr() as usize;
    let (dictionary, stale) = std::thread::spawn(move || unsafe {
        let gil = ffi::PyGILState_Ensure();
        let result = ffi::PyObject_CallNoArgs(callable as *mut ffi::PyObject);
        assert!(!result.is_null());
        ffi::Py_DECREF(result);
        let dictionary = ffi::PyThreadState_GetDict();
        assert!(!dictionary.is_null());
        ffi::Py_INCREF(dictionary); // A legitimate extension-owned strong reference.
        ffi::PyGILState_Release(gil); // The state is deleted, but the dictionary survives.
        assert!(ffi::PyGILState_GetThisThreadState().is_null());
        // Check before any memory read can hide the bug behind a finalizing flag
        // left in freed storage. The profiler must have *no address* to dereference.
        let stale = unwind::bindings::current();
        emit_samples(13);
        (dictionary as usize, stale)
    })
    .join()
    .unwrap();
    Python::attach(|_| unsafe { ffi::Py_DECREF(dictionary as *mut ffi::PyObject) });
    let profile = observed();
    assert_eq!(
        stale, 0,
        "freed state retained in binding; full observed profile: {profile:#?}"
    );
    let expected: Profile = [
        (vec!["outer".into(), "leaf".into()], 7),
        (vec!["[dropped: no_thread_state]".into()], 13),
    ]
    .into_iter()
    .collect();
    assert_eq!(profile, expected, "full observed profile: {profile:#?}");
}

#[test]
fn bindings_expire_at_stop_and_require_a_new_execution_event() {
    let _guard = super::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    unsafe { kindasafe_init::init() }.unwrap();
    let monitor = super::monitoring::Monitor::start().unwrap();
    let _signal = SignalHandler::install(libc::SIGPROF, super::signal_handler).unwrap();
    observed();
    let (ready, wait_ready) = mpsc::channel();
    let (resume, wait_resume) = mpsc::channel();
    *EPOCH_PAUSE.lock().unwrap() = Some((ready, wait_resume));
    let function = Python::attach(|py| {
        let module = PyModule::from_code(py, c"def outer():\n    leaf()\ndef leaf():\n    emit_samples(7)\n    epoch_pause()\n    emit_samples(7)\n", c"across_restart.py", c"across_restart").unwrap();
        module
            .add_function(wrap_pyfunction!(emit_samples, &module).unwrap())
            .unwrap();
        module
            .add_function(wrap_pyfunction!(epoch_pause, &module).unwrap())
            .unwrap();
        module.getattr("outer").unwrap().unbind()
    });
    let worker = std::thread::spawn(move || {
        // Keep active Python frames across stop/restart, without new events during
        // the native call. The old binding must not become eligible in a new epoch.
        Python::attach(|py| {
            function.call0(py).unwrap();
        });
    });
    wait_ready.recv().unwrap();
    expect(&[(&["outer", "leaf"], 7)]);
    drop(monitor);
    let _monitor = super::monitoring::Monitor::start().unwrap();
    resume.send(()).unwrap();
    worker.join().unwrap();
    expect(&[
        (&["[dropped: no_thread_state]"], 11),
        (&["outer", "leaf"], 7),
    ]);
}

#[test]
fn execution_scope_covers_yield_throw_unwind_and_native_reentry() {
    let _guard = super::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    unsafe { kindasafe_init::init() }.unwrap();
    let _monitor = super::monitoring::Monitor::start().unwrap();
    let _signal = SignalHandler::install(libc::SIGPROF, super::signal_handler).unwrap();
    observed();
    Python::attach(|py| {
        let module = PyModule::from_code(py, c"def generator():\n    try:\n        emit_samples(7)\n        yield 1\n    except ValueError:\n        emit_samples(11)\n        raise\ndef raises():\n    emit_samples(5)\n    raise ValueError()\ndef callback():\n    emit_samples(3)\ndef parent():\n    native_reentry(callback)\n", c"scope_boundaries.py", c"scope_boundaries").unwrap();
        module
            .add_function(wrap_pyfunction!(emit_samples, &module).unwrap())
            .unwrap();
        module
            .add_function(wrap_pyfunction!(native_reentry, &module).unwrap())
            .unwrap();
        let generator = module.getattr("generator").unwrap().call0().unwrap();
        assert_eq!(
            unwind::bindings::current(),
            0,
            "binding survived generator creation; full observed profile: {:#?}",
            observed()
        );
        generator.call_method0("__next__").unwrap();
        assert_eq!(
            unwind::bindings::current(),
            0,
            "binding survived root yield; full observed profile: {:#?}",
            observed()
        );
        emit_samples(17);
        let error = generator
            .call_method1("throw", (py.get_type::<pyo3::exceptions::PyValueError>(),))
            .unwrap_err();
        assert!(error.is_instance_of::<pyo3::exceptions::PyValueError>(py));
        assert_eq!(
            unwind::bindings::current(),
            0,
            "binding survived root unwind after throw; full observed profile: {:#?}",
            observed()
        );
        emit_samples(19);
        assert!(module.getattr("raises").unwrap().call0().is_err());
        assert_eq!(
            unwind::bindings::current(),
            0,
            "binding survived root exception; full observed profile: {:#?}",
            observed()
        );
        emit_samples(23);
        module.getattr("parent").unwrap().call0().unwrap();
        assert_eq!(
            unwind::bindings::current(),
            0,
            "binding survived root return; full observed profile: {:#?}",
            observed()
        );
    });
    expect(&[
        (&["generator"], 18),
        (&["raises"], 5),
        (&["parent", "callback"], 3),
        (&["parent"], 13),
        (&["[dropped: no_thread_state]"], 59),
    ]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_reused_pthread_cannot_inherit_a_retained_python_state() {
    // An intentionally abandoned *bound* state cannot be deleted on a different
    // thread: CPython's unbind routine clears that caller's GIL-state TLS. Isolate
    // this embedding misuse in a subprocess and leave the retained state alive.
    const CHILD: &str = "PYROSCOPE_BINDING_REUSE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "signal_sampler::binding_tests::a_reused_pthread_cannot_inherit_a_retained_python_state", "--nocapture"])
            .env(CHILD, "1")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let _guard = super::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    unsafe { kindasafe_init::init() }.unwrap();
    let _monitor = super::monitoring::Monitor::start().unwrap();
    let _signal = SignalHandler::install(libc::SIGPROF, super::signal_handler).unwrap();
    observed();
    let function = Python::attach(function);
    let callable = function.as_ptr() as usize;
    let (abandoned, identity) = std::thread::spawn(move || unsafe {
        ffi::PyGILState_Ensure();
        let result = ffi::PyObject_CallNoArgs(callable as *mut ffi::PyObject);
        assert!(!result.is_null());
        ffi::Py_DECREF(result);
        let state = ffi::PyEval_SaveThread();
        // Deliberately omit PyGILState_Release. CPython retains this bound state
        // in its list, while pthread teardown destroys our per-OS-thread slot.
        (state as usize, libc::pthread_self())
    })
    .join()
    .unwrap();
    expect(&[(&["outer", "leaf"], 7)]);
    let mut reused = false;
    for _ in 0..512 {
        if std::thread::spawn(move || unsafe {
            if libc::pthread_self() != identity {
                return false;
            }
            assert!(ffi::PyGILState_GetThisThreadState().is_null());
            emit_samples(11);
            let gil = ffi::PyGILState_Ensure();
            let result = ffi::PyObject_CallNoArgs(callable as *mut ffi::PyObject);
            assert!(!result.is_null());
            ffi::Py_DECREF(result);
            ffi::PyGILState_Release(gil); // Clears/deletes the newly created state.
            assert!(ffi::PyGILState_GetThisThreadState().is_null());
            emit_samples(13);
            true
        })
        .join()
        .unwrap()
        {
            reused = true;
            break;
        }
    }
    // Prove the old state remains linked after actual native identity reuse.
    // This inspection is outside SIGPROF with the GIL, only in the regression test.
    Python::attach(|_| unsafe {
        let mut state = ffi::PyInterpreterState_ThreadHead(ffi::PyInterpreterState_Main());
        while !state.is_null() && state as usize != abandoned {
            state = ffi::PyThreadState_Next(state);
        }
        assert_eq!(state as usize, abandoned);
    });
    assert!(
        reused,
        "the test must exercise actual pthread identity reuse"
    );
    expect(&[
        (&["[dropped: no_thread_state]"], 24),
        (&["outer", "leaf"], 7),
    ]);
    std::process::exit(0);
}
