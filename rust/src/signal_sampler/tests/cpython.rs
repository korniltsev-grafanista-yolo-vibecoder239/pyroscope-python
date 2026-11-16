use pyo3::{exceptions::PyAssertionError, prelude::*, types::PyModule, wrap_pyfunction};
use pyroscope_pprof::{StringTable, ffi::FFIFrame};
use pyroscope_python_resolve::{Lines, PYO3Types, Resolver};
use pyroscope_python_unwind::{self as unwind, Frame, MAX_FRAMES};
use std::{
    ffi::CString,
    sync::atomic::{AtomicUsize, Ordering},
};

const SOURCE: &str = include_str!("../../../../scripts/tests/resolve_stacks.py");
const FOREIGN_SOURCE: &str = "def leaf():\n    return assert_stack(expected)\n";
const FOREIGN_FILENAME: &str = "foreign_🐍.py";
static STACKS_CHECKED: AtomicUsize = AtomicUsize::new(0);

// Root-to-leaf name, filename, first line, and current line, supplied by the test.
type ExpectedFrame = (String, String, i32, i32);

#[pyfunction(signature = (expected, count = 1))]
fn assert_stack(expected: Vec<ExpectedFrame>, count: usize) -> PyResult<usize> {
    let mut buffer = [Frame::default(); MAX_FRAMES];
    let raw = unwind::unwind(&mut buffer)
        .map_err(|error| PyAssertionError::new_err(format!("unwind failed: {error:?}")))?;
    for mode in [Lines::LastInstruction, Lines::First, Lines::NoLine] {
        let mut strings = StringTable::new();
        let mut resolver = Resolver::<PYO3Types>::new(mode);
        let resolved = resolver
            .resolve_stack(raw, count, &mut strings)
            .map_err(|error| {
                PyAssertionError::new_err(format!("{mode:?}: {error:?}; raw stack: {raw:?}"))
            })?;
        let mut table = Vec::new();
        strings.clone_pprof_table(&mut table);
        let actual: Vec<_> = resolved
            .iter()
            .rev()
            .map(|frame| {
                (
                    table[frame.function_name.index as usize].clone(),
                    table[frame.file_name.index as usize].clone(),
                    frame.line,
                )
            })
            .collect();
        let wanted: Vec<_> = expected
            .iter()
            .map(|(name, filename, first, current)| {
                (
                    name.clone(),
                    filename.clone(),
                    match mode {
                        Lines::LastInstruction => *current,
                        Lines::First => *first,
                        Lines::NoLine => 0,
                    },
                )
            })
            .collect();
        if actual != wanted {
            return Err(PyAssertionError::new_err(format!(
                "{mode:?}:\nexpected root-to-leaf stack: {wanted:#?}\n\
                 actual root-to-leaf stack: {actual:#?}\nraw stack: {raw:#?}"
            )));
        }
        if count > 1 {
            assert_profile_weight(resolved, strings, &wanted, count)?;
        }
    }
    STACKS_CHECKED.fetch_add(1, Ordering::Relaxed);
    Ok(1)
}

fn assert_profile_weight(
    frames: &[FFIFrame],
    strings: StringTable,
    expected: &[(String, String, i32)],
    count: usize,
) -> PyResult<()> {
    let profile =
        crate::signal_backend::tests::profile_for_resolved_stack(frames, strings, &[2, count - 2]);
    let actual: Vec<_> = profile
        .sample
        .iter()
        .map(|sample| {
            let stack: Vec<_> = sample
                .location_id
                .iter()
                .rev()
                .flat_map(|id| {
                    let location = profile
                        .location
                        .iter()
                        .find(|location| location.id == *id)
                        .unwrap();
                    location.line.iter().map(|line| {
                        let function = profile
                            .function
                            .iter()
                            .find(|function| function.id == line.function_id)
                            .unwrap();
                        (
                            profile.string_table[function.name as usize].as_str(),
                            profile.string_table[function.filename as usize].as_str(),
                            line.line,
                        )
                    })
                })
                .collect();
            (stack, sample.value.clone())
        })
        .collect();
    let stack: Vec<_> = expected
        .iter()
        .map(|(name, filename, line)| (name.as_str(), filename.as_str(), i64::from((*line).max(0))))
        .collect();
    // Two identical weighted samples must aggregate to the full count at 100 Hz.
    let wanted = vec![(stack, vec![count as i64 * 10_000_000])];
    if actual != wanted {
        return Err(PyAssertionError::new_err(format!(
            "expected profile stacks and weights: {wanted:#?}\nactual: {actual:#?}\nfull profile: {profile:#?}"
        )));
    }
    Ok(())
}

fn source_line(marker: &str) -> i32 {
    let lines: Vec<_> = SOURCE
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            line.split_once("# ")
                .is_some_and(|(_, label)| label == marker)
        })
        .map(|(index, _)| index as i32 + 1)
        .collect();
    assert_eq!(lines.len(), 1, "expected one source marker {marker:?}");
    lines[0]
}

fn expected_frame(name: &str, filename: &str, first: &str, current: &str) -> ExpectedFrame {
    // CPython 3.10 has co_name; qualified code names start with 3.11.
    #[cfg(not(Py_3_11))]
    let name = name.rsplit('.').next().unwrap();
    (
        name.to_owned(),
        filename.to_owned(),
        source_line(first),
        source_line(current),
    )
}

fn module<'py>(py: Python<'py>, source: &str, filename: &str, name: &str) -> Bound<'py, PyModule> {
    let module = PyModule::from_code(
        py,
        &CString::new(source).unwrap(),
        &CString::new(filename).unwrap(),
        &CString::new(name).unwrap(),
    )
    .unwrap();
    module
        .add_function(wrap_pyfunction!(assert_stack, &module).unwrap())
        .unwrap();
    module
}

#[test]
fn resolves_real_cpython_stack_shapes() {
    let _guard = crate::signal_sampler::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    #[cfg(Py_3_15)]
    // SAFETY: the embedded interpreter remains alive for the rest of the process.
    unsafe { unwind::prepare_thread_lookup() }.unwrap();
    // SAFETY: these tests preserve recovery handlers throughout all walks.
    unsafe { kindasafe_init::init() }.unwrap();
    Python::attach(|py| {
        let foreign = module(py, FOREIGN_SOURCE, FOREIGN_FILENAME, "resolver_foreign");
        for (index, filename) in ["frames.py", "frames_é.py", "frames_中.py", "frames_🐍.py"]
            .into_iter()
            .enumerate()
        {
            let module = module(py, SOURCE, filename, &format!("resolver_shapes_{index}"));
            module.setattr("foreign", &foreign).unwrap();
            let frame = |name: &str, marker: &str| {
                expected_frame(
                    name,
                    filename,
                    &format!("first: {marker}"),
                    &format!("call: {marker}"),
                )
            };
            let mut comprehension = vec![frame("comprehension", "comprehension")];
            // PEP 709 inlines list comprehensions from CPython 3.12 onward.
            if !cfg!(Py_3_12) {
                comprehension.push(expected_frame(
                    "comprehension.<locals>.<listcomp>",
                    filename,
                    "call: comprehension",
                    "call: comprehension",
                ));
            }
            let cases = [
                ("direct", vec![frame("direct", "direct")]),
                (
                    "nested",
                    vec![
                        frame("nested", "nested"),
                        frame("nested.<locals>.outer", "outer"),
                        frame("nested.<locals>.outer.<locals>.inner", "inner"),
                    ],
                ),
                (
                    "methods",
                    vec![
                        frame("methods", "methods"),
                        frame("Café.café", "café"),
                        frame("Café.中文", "中文"),
                        frame("Café.𐐀", "𐐀"),
                    ],
                ),
                (
                    "generators",
                    vec![
                        frame("generators", "generators"),
                        frame("generator_outer", "generator_outer"),
                        frame("generator_leaf", "generator_leaf"),
                    ],
                ),
                (
                    "coroutines",
                    vec![
                        frame("coroutines", "coroutines"),
                        frame("coroutine_outer", "coroutine_outer"),
                        frame("coroutine_leaf", "coroutine_leaf"),
                    ],
                ),
                (
                    "empty_lines",
                    vec![
                        frame("empty_lines", "empty_lines"),
                        (
                            "empty_lines_leaf".into(),
                            filename.into(),
                            source_line("first: empty_lines_leaf"),
                            -1,
                        ),
                    ],
                ),
                (
                    "unmapped_lines",
                    vec![
                        frame("unmapped_lines", "unmapped_lines"),
                        (
                            "unmapped_lines_middle".into(),
                            filename.into(),
                            source_line("first: unmapped_lines_middle"),
                            -1,
                        ),
                        frame("mapped_lines_leaf", "mapped_lines_leaf"),
                    ],
                ),
                (
                    "malformed_lines",
                    vec![
                        frame("malformed_lines", "malformed_lines"),
                        (
                            "malformed_lines_leaf".into(),
                            filename.into(),
                            source_line("first: malformed_lines_leaf"),
                            -1,
                        ),
                    ],
                ),
                (
                    "long_unicode_names",
                    vec![
                        frame("long_unicode_names", "long_unicode_names"),
                        (
                            "中".repeat(2000),
                            "🐍".repeat(2000),
                            source_line("first: long_unicode_leaf"),
                            source_line("call: long_unicode_leaf"),
                        ),
                    ],
                ),
                ("comprehension", comprehension),
                (
                    "c_reentry",
                    vec![
                        frame("c_reentry", "c_reentry"),
                        frame("map_leaf", "map_leaf"),
                    ],
                ),
                (
                    "exceptions",
                    vec![frame("exceptions", "exceptions"), frame("direct", "direct")],
                ),
                (
                    "cross_module",
                    vec![
                        frame("cross_module", "cross_module"),
                        ("leaf".into(), FOREIGN_FILENAME.into(), 1, 2),
                    ],
                ),
            ];
            for (entry, expected) in cases {
                module.setattr("expected", expected.clone()).unwrap();
                foreign.setattr("expected", expected).unwrap();
                let before = STACKS_CHECKED.load(Ordering::Relaxed);
                let result = module
                    .getattr(entry)
                    .unwrap()
                    .call0()
                    .unwrap_or_else(|error| panic!("{filename}: {entry}: {error}"));
                assert_eq!(result.extract::<usize>().unwrap(), 1, "{filename}: {entry}");
                assert_eq!(
                    STACKS_CHECKED.load(Ordering::Relaxed),
                    before + 1,
                    "{filename}: {entry}"
                );
            }
            for depth in [0, 4, 32] {
                let mut expected = vec![frame("recursive", "recursive"); depth];
                expected.push(expected_frame(
                    "recursive",
                    filename,
                    "first: recursive",
                    "leaf: recursive",
                ));
                module.setattr("expected", expected).unwrap();
                let before = STACKS_CHECKED.load(Ordering::Relaxed);
                let result = module
                    .getattr("recursive")
                    .unwrap()
                    .call1((depth,))
                    .unwrap_or_else(|error| panic!("{filename}: recursive({depth}): {error}"));
                assert_eq!(
                    result.extract::<usize>().unwrap(),
                    1,
                    "{filename}: recursive({depth})"
                );
                assert_eq!(
                    STACKS_CHECKED.load(Ordering::Relaxed),
                    before + 1,
                    "{filename}: recursive({depth})"
                );
            }
        }
    });
}

#[cfg(Py_3_15)]
#[test]
fn fresh_native_thread_does_not_inherit_abandoned_python_state() {
    const CHILD: &str = "PYROSCOPE_TEST_TLS_REUSE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Intentionally abandoned C-API state belongs only to this subprocess.
        // The embedded interpreter is never finalized; process exit reclaims it.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "signal_sampler::cpython_tests::fresh_native_thread_does_not_inherit_abandoned_python_state", "--nocapture"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            output.status.success(),
            "child failed: {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let _guard = crate::signal_sampler::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    unsafe { kindasafe_init::init() }.unwrap();
    unsafe { unwind::prepare_thread_lookup() }.unwrap();

    let (abandoned, old_id) = std::thread::spawn(|| unsafe {
        // Deliberately omit PyGILState_Release. PyEval_SaveThread releases the
        // GIL but leaves a linked, bound state when the OS thread exits.
        pyo3::ffi::PyGILState_Ensure();
        let bound = pyo3::ffi::PyGILState_GetThisThreadState() as usize;
        let saved = pyo3::ffi::PyEval_SaveThread() as usize;
        assert_eq!(bound, saved);
        (bound, libc::pthread_self() as usize)
    })
    .join()
    .unwrap();
    let still_linked = || {
        Python::attach(|_| unsafe {
            let mut state =
                pyo3::ffi::PyInterpreterState_ThreadHead(pyo3::ffi::PyInterpreterState_Main());
            while !state.is_null() {
                if state as usize == abandoned {
                    return true;
                }
                state = pyo3::ffi::PyThreadState_Next(state);
            }
            false
        })
    };
    assert!(
        still_linked(),
        "abandoned state {abandoned:#x} was not retained"
    );
    unwind::METRICS.take_drop_counts();
    let _handler =
        pyroscope_signal_timer::SignalHandler::install(libc::SIGPROF, super::signal_handler)
            .unwrap();
    let mut reused = None;
    for _ in 0..256 {
        let observation = std::thread::spawn(move || {
            // This thread NEVER calls a Python API, even for the test oracle.
            // Joining the previous thread permits pthread/stack-cache reuse.
            let id = unsafe { libc::pthread_self() } as usize;
            if id != old_id {
                return None;
            }
            let mut frames = [Frame::default(); MAX_FRAMES];
            let result = unwind::unwind(&mut frames).map(<[Frame]>::to_vec);
            // Exercise the actual SIGPROF callback, not only the lookup helper.
            for _ in 0..200 {
                assert_eq!(unsafe { libc::raise(libc::SIGPROF) }, 0);
            }
            Some((id, result))
        })
        .join()
        .unwrap();
        if observation.is_some() {
            reused = observation;
            break;
        }
    }
    let (fresh_id, stack) =
        reused.expect("pthread identity was not reused in 256 joined native threads");
    assert_eq!(fresh_id, old_id);
    assert_eq!(
        stack,
        Err(unwind::Error::NoThreadState),
        "abandoned={abandoned:#x}, reused pthread={old_id:#x}, full observed stack={stack:#?}"
    );
    assert!(
        still_linked(),
        "the fixture must retain the abandoned state during the fresh thread's samples"
    );

    // One direct unwind and 200 signal deliveries must all report no Python
    // state. Assert the complete encoded profile and its exact period weight.
    let mut buffer = crate::signal_backend::ProfileBuffer::new();
    let range = crate::utils::TimeRange::new(
        std::time::UNIX_EPOCH,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(3),
    )
    .unwrap();
    let profile = buffer.report(100, &range).unwrap();
    let actual: Vec<_> = profile
        .sample
        .iter()
        .map(|sample| {
            let frames: Vec<_> = sample
                .location_id
                .iter()
                .rev()
                .flat_map(|id| {
                    let location = profile.location.iter().find(|l| l.id == *id).unwrap();
                    location.line.iter().map(|line| {
                        let function = profile
                            .function
                            .iter()
                            .find(|f| f.id == line.function_id)
                            .unwrap();
                        (
                            profile.string_table[function.name as usize].as_str(),
                            profile.string_table[function.filename as usize].as_str(),
                            line.line,
                        )
                    })
                })
                .collect();
            (frames, sample.value.clone())
        })
        .collect();
    assert_eq!(
        actual,
        vec![(
            vec![("[dropped: no_thread_state]", "", 0)],
            vec![2_010_000_000]
        )],
        "full profile: {profile:#?}"
    );
    let mut consumer = unwind::consumer().unwrap();
    assert_eq!(
        consumer.pop_slice(&mut [Frame::default(); MAX_FRAMES]),
        Err(pyroscope_ringbuffer::PopError::Empty),
        "a fresh non-Python thread must publish no Python stack; full profile: {profile:#?}"
    );
    eprintln!(
        "retained state {abandoned:#x}, reused pthread {old_id:#x}: all 201 periods have no Python state"
    );
}
