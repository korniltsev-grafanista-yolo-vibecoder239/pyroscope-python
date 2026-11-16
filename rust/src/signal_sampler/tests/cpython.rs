use pyo3::{exceptions::PyAssertionError, prelude::*, types::PyModule, wrap_pyfunction};
use pyroscope_pprof::{StringTable, ffi::FFIFrame};
use pyroscope_python_resolve::{Lines, PYO3Types, Resolver};
use pyroscope_python_unwind::{self as unwind, Frame, MAX_FRAMES};
use std::{
    collections::BTreeMap,
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

#[pyfunction]
fn assert_detached_stack(py: Python<'_>, expected: Vec<ExpectedFrame>) -> PyResult<usize> {
    // A getter for only the attached state would lose this entire stack.
    py.detach(|| assert_stack(expected, 19))
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
        .add_function(wrap_pyfunction!(assert_detached_stack, &module).unwrap())
        .unwrap();
    module
}

#[cfg(target_os = "linux")]
#[test]
fn signals_on_native_threads_have_no_python_stack() {
    let _guard = crate::signal_sampler::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    // CTimer starts unarmed until a thread is registered. Deliver explicit
    // signals to an OS thread which has never called into Python instead.
    let mut session =
        crate::signal_sampler::SignalHandlerSession::start(&crate::signal_sampler::Config {
            python_version: unwind::PY_VERSION,
            sampling_rate: 100,
            signal_timer: crate::signal_sampler::SignalTimer::CTimer,
            lines: Lines::LastInstruction,
        })
        .unwrap();
    std::thread::spawn(|| {
        for _ in 0..7 {
            // SAFETY: SIGPROF is installed and raise targets this calling thread.
            assert_eq!(unsafe { libc::raise(libc::SIGPROF) }, 0);
        }
    })
    .join()
    .unwrap();
    let mut strings = StringTable::new();
    let mut profile = Vec::new();
    session.drain(&mut strings, |frames, count| {
        profile.push((
            frames
                .iter()
                .rev()
                .map(|frame| (frame.function_name.index, frame.file_name.index, frame.line))
                .collect::<Vec<_>>(),
            count,
        ))
    });
    let mut table = Vec::new();
    strings.clone_pprof_table(&mut table);
    let profile: Vec<_> = profile
        .iter()
        .map(|(frames, count)| {
            (
                frames
                    .iter()
                    .map(|&(name, file, line)| (&table[name as usize], &table[file as usize], line))
                    .collect::<Vec<_>>(),
                count,
            )
        })
        .collect();
    assert!(
        profile.is_empty(),
        "full observed root-to-leaf profile: {profile:#?}"
    );
    assert_eq!(
        unwind::METRICS.take_drop_counts(),
        [
            ("contentions", 0),
            ("buffer_dropped", 0),
            ("no_thread_state", 7),
            ("invalid_code_type", 0),
            ("read_errors", 0),
            ("truncated", 0),
            ("resolve_error", 0),
        ],
        "full observed profile: {profile:#?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn real_signals_profile_python_and_detached_native_work() {
    use crate::signal_sampler::{Config, SignalHandlerSession, SignalTimer, thread_created};

    const LIVE_SOURCE: &str = include_str!("../../../../scripts/tests/signal_stacks.py");
    let _guard = crate::signal_sampler::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    Python::attach(|py| {
        let module = module(py, LIVE_SOURCE, "signal_live.py", "signal_live");
        let mut session = SignalHandlerSession::start(&Config {
            python_version: unwind::PY_VERSION,
            sampling_rate: 100,
            signal_timer: SignalTimer::CTimer,
            lines: Lines::First,
        })
        .unwrap();
        let timer = session.timer();
        thread_created(&timer, pyroscope_signal_timer::gettid()).unwrap();
        module.getattr("entry").unwrap().call0().unwrap();
        // Stop producing before draining the finite profile.
        timer.lock().unwrap().take();
        let mut strings = StringTable::new();
        let mut samples = Vec::new();
        loop {
            let before = samples.len();
            session.drain(&mut strings, |frames, count| {
                samples.push((
                    frames
                        .iter()
                        .rev()
                        .map(|frame| (frame.function_name.index, frame.file_name.index, frame.line))
                        .collect::<Vec<_>>(),
                    count,
                ));
            });
            if samples.len() == before {
                break;
            }
        }
        let mut table = Vec::new();
        strings.clone_pprof_table(&mut table);
        let mut profile = BTreeMap::new();
        for (frames, count) in samples {
            let stack: Vec<_> = frames
                .into_iter()
                .map(|(name, file, line)| {
                    (
                        table[name as usize].as_str(),
                        table[file as usize].as_str(),
                        line,
                    )
                })
                .collect();
            *profile.entry(stack).or_insert(0usize) += count;
        }
        let drops = unwind::METRICS.take_drop_counts();
        let total =
            profile.values().sum::<usize>() + drops.iter().map(|(_, count)| count).sum::<usize>();
        let diagnostics =
            format!("full root-to-leaf profile (100Hz periods): {profile:#?}\ndrops: {drops:?}");
        // Four thread CPU seconds should produce about 400 periods. Include
        // lost periods in the denominator so lookup/resolution failures cannot
        // hide a broken native phase. Allow 20% timer noise, 40–60% per leaf,
        // and at most 5% unexpected or dropped weight.
        assert!(total >= 320, "total={total}; {diagnostics}");
        let frame = |name| {
            let marker = format!("# first: {name}");
            let line = LIVE_SOURCE
                .lines()
                .position(|line| line.ends_with(&marker))
                .unwrap() as i32
                + 1;
            (name, "signal_live.py", line)
        };
        let mut matched = 0;
        for leaf in ["python_leaf", "native_leaf"] {
            let mut expected = vec![frame("entry"), frame(leaf)];
            if leaf == "python_leaf" {
                // These hot calls may be inlined by the 3.15 JIT. Their full
                // Python frame chain must still be visible to SIGPROF.
                expected.extend([frame("python_outer"), frame("python_inner")]);
            }
            let value = profile.get(&expected).copied().unwrap_or(0);
            let share = value as f64 / total as f64;
            assert!(
                (0.40..=0.60).contains(&share),
                "expected {expected:?}: {value}/{total}; {diagnostics}"
            );
            matched += value;
        }
        assert!(
            matched as f64 / total as f64 >= 0.95,
            "matched={matched}/{total}; {diagnostics}"
        );
    });
}

#[test]
fn resolves_real_cpython_stack_shapes() {
    let _guard = crate::signal_sampler::METRICS_LOCK.lock().unwrap();
    Python::initialize();
    // SAFETY: these tests preserve recovery handlers throughout all walks.
    unsafe { kindasafe_init::init() }.unwrap();
    unwind::prepare_thread_lookup();
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
                ("detached", vec![frame("detached", "detached")]),
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
