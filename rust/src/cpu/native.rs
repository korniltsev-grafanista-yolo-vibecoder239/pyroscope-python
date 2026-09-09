//! Sink and FFI surface for the vendored native CPU profiler.
//!
//! The C++ sampler in `cpp/cpu/ddtrace_stack/` pushes into the one global
//! [`StackBuffer`] here, and the agent's snapshot loop drains it through the
//! same `Report` -> `encode::pprof` -> `session` path py-spy uses.

use crate::backend::{BackendConfig, StackBuffer, StackFrame, StackTrace, ThreadTagsSet};
use crate::cpu::{CpuConfig, CpuProfiler};
use crate::encode::pprof::ffi::{FFICpuSample, FFIStringView};
use crate::error::{PyroscopeError, Result};
use crate::utils::ThreadId;
use lazy_static::lazy_static;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

lazy_static! {
    static ref BUFFER: Mutex<StackBuffer> = Mutex::new(StackBuffer::default());
    /// Snapshot of the reporting knobs, published by `start()` before the
    /// sampler is armed and read by the push callback.
    static ref SINK_CONFIG: Mutex<SinkConfig> = Mutex::new(SinkConfig::default());
}

/// Which native sampler is currently armed, as a `CpuProfiler` discriminant, or
/// [`NO_ACTIVE_PROFILER`] when none is.
///
/// Read by `postfork_child()`, which runs in a freshly forked child where the
/// Rust agent state has been leaked wholesale and no lock may be taken.
static ACTIVE_PROFILER: AtomicU8 = AtomicU8::new(NO_ACTIVE_PROFILER);
const NO_ACTIVE_PROFILER: u8 = u8::MAX;

#[derive(Default, Clone)]
struct SinkConfig {
    backend_config: BackendConfig,
    ruleset: Option<ThreadTagsSet>,
}

/// Publish the reporting configuration. Must be called before the sampler is
/// armed, otherwise samples would race an unset config.
pub fn set_sink_config(backend_config: BackendConfig, ruleset: ThreadTagsSet) {
    let mut cfg = SINK_CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    *cfg = SinkConfig {
        backend_config,
        ruleset: Some(ruleset),
    };
}

/// Discard buffered samples and the published config.
///
/// Called after the sampler is disarmed. Without this, samples buffered by a
/// stopped session (or inherited across a fork) would leak into the next
/// session's first profile.
pub fn clear_state() {
    let mut buf = BUFFER.lock().unwrap_or_else(|e| e.into_inner());
    buf.clear();
    drop(buf);
    let mut cfg = SINK_CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    *cfg = SinkConfig::default();
}

/// Drain everything buffered since the last call.
pub fn take_buffer() -> Result<StackBuffer> {
    Ok(std::mem::take(&mut *BUFFER.lock()?))
}

/// Receives one aggregated stack trace from the vendored C++ sampler.
///
/// # Safety
///
/// `sample.frames` must point to `sample.len` initialised `FFICpuFrame`s, and
/// every string view must point to valid UTF-8 for the duration of this call.
///
/// This function allocates and takes a lock, so it must **never** be called
/// from a signal handler. The vendored sampler calls it from its ordinary
/// sampling thread; preserve that when porting another one.
#[unsafe(no_mangle)]
pub extern "C" fn pyroscope_cpu_push_sample(sample: FFICpuSample) {
    if sample.frames.is_null() || sample.len == 0 || sample.cpu_nanos == 0 {
        return;
    }

    let ffi_frames = unsafe { std::slice::from_raw_parts(sample.frames, sample.len) };
    let mut frames = Vec::with_capacity(ffi_frames.len());
    for frame in ffi_frames {
        frames.push(StackFrame {
            name: unsafe { string_view_to_string(&frame.function_name) },
            filename: unsafe { string_view_to_string(&frame.file_name) },
            line: frame.line.max(0) as u32,
        });
    }

    let cfg = match SINK_CONFIG.lock() {
        Ok(cfg) => cfg.clone(),
        Err(_) => return,
    };
    let Some(ruleset) = cfg.ruleset else {
        // Sampler produced a sample before start() published the config, or
        // after clear_state() tore it down. Drop it rather than report it with
        // default (untagged) metadata.
        return;
    };

    // pthread_t is c_ulong on glibc and *mut c_void on musl; an integer->pointer
    // `as` cast covers both.
    let thread_id =
        (sample.thread_id != 0).then(|| ThreadId::from(sample.thread_id as libc::pthread_t));
    let thread_name = unsafe { string_view_to_option_string(&sample.thread_name) };

    let stacktrace = StackTrace::new(
        &cfg.backend_config,
        (sample.pid != 0).then_some(sample.pid),
        thread_id,
        thread_name,
        frames,
    )
    .add_tag_rules(&ruleset);

    if let Ok(mut buffer) = BUFFER.lock() {
        // The "count" slot carries CPU nanoseconds for this path; the reporter
        // tags the batch as ReportsCpuNanos so the encoder does not multiply by
        // the sampling period again.
        let _ = buffer.record_with_count(stacktrace, sample.cpu_nanos as usize);
    }
}

/// # Safety
/// `s` must describe valid UTF-8 or be empty/null.
unsafe fn string_view_to_string(s: &FFIStringView) -> String {
    if s.data.is_null() || s.len == 0 {
        return String::new();
    }
    // `.cast()` rather than `as *const u8`: c_char is i8 on x86_64 but u8 on
    // aarch64, so an `as` cast is required on one and flagged as unnecessary by
    // clippy on the other.
    let bytes = unsafe { std::slice::from_raw_parts(s.data.cast::<u8>(), s.len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// # Safety
/// `s` must describe valid UTF-8 or be empty/null.
unsafe fn string_view_to_option_string(s: &FFIStringView) -> Option<String> {
    if s.data.is_null() || s.len == 0 {
        return None;
    }
    Some(unsafe { string_view_to_string(s) })
}

/* ---------------------------------------------------------------------- *
 * Vendored sampler entry points.
 *
 * Implemented by the static library built from cpp/cpu/ddtrace_stack/ by
 * rust/build.rs. A non-zero return means the sampler failed to start; the C++
 * side sets a Python exception describing why.
 * ---------------------------------------------------------------------- */

/// Whether the implementation's static library is part of this build.
///
/// The cfg is emitted by `rust/build.rs` for exactly the project it actually
/// compiled and linked, so this cannot drift from the build.
pub const fn ddtrace_built() -> bool {
    cfg!(pyroscope_cpu_ddtrace)
}

#[cfg(pyroscope_cpu_ddtrace)]
unsafe extern "C" {
    fn pyroscope_cpu_ddtrace_can_start(sample_rate_hz: u32, cpu_timer_enabled: i32) -> i32;
    fn pyroscope_cpu_ddtrace_min_interval_ms() -> u64;
    fn pyroscope_cpu_ddtrace_start(
        sample_rate_hz: u32,
        max_nframes: u32,
        cpu_timer_enabled: i32,
    ) -> i32;
    fn pyroscope_cpu_ddtrace_stop();
    fn pyroscope_cpu_ddtrace_postfork_child();
}

/// Why the vendored sampler could not start with this configuration, checked
/// without changing any state.
///
/// Used by [`CpuProfiler::check_supported`] so `configure()` raises with the
/// reason. Some of these are only knowable at runtime -- whether this process
/// already ran a CPU profiler, whether the timer engine disabled itself -- which
/// is why they cannot live in the platform/version checks next to it.
pub fn ddtrace_unsupported_reason(profiler: CpuProfiler, sample_rate: u32) -> Option<String> {
    let cpu_timer = match profiler {
        CpuProfiler::Ddtrace => false,
        CpuProfiler::DdtraceCpuTimer => true,
        CpuProfiler::PySpy => return None,
    };
    if !ddtrace_built() {
        // There is nothing to ask: the static library is not linked in. The
        // caller reports that case with the platform detail it has to hand.
        return None;
    }
    match unsafe { ddtrace_can_start(sample_rate, cpu_timer) } {
        0 => None,
        status => Some(ddtrace_start_error(profiler, status)),
    }
}

/// Explains a non-zero status from `pyroscope_cpu_ddtrace_start`.
///
/// Mirrors the `Status` enum in `cpp/cpu/ddtrace_stack/src/pyroscope_entry.cpp`;
/// keep the two in step. The text lives here rather than in C++ so the whole
/// user-facing story of why a profiler refused to start is in one place.
fn ddtrace_start_error(profiler: CpuProfiler, status: i32) -> String {
    let detail = match status {
        1 => "it is already running in this process".to_string(),
        2 => "sample_rate must be greater than zero".to_string(),
        3 => "the sampling thread could not be started".to_string(),
        4 => format!(
            "the per-thread CPU timers could not be armed. A process can run \
             cpu_profiler={} at most once: the timer engine refuses to re-arm \
             after it has been shut down, because a SIGPROF left over from a \
             deleted timer must not find live state again. Configure it once \
             per process, or use cpu_profiler=ddtrace",
            profiler.name()
        ),
        5 => format!(
            "this process has already committed to a different CPU profiler \
             configuration. cpu_profiler={} cannot be selected after another \
             CPU profiler has run here (switching mid-process would mix two ways \
             of measuring CPU time into one profile), and a fork child of a \
             CPU-timer session inherits its sample_rate and cannot change it",
            profiler.name()
        ),
        6 => {
            // The floor comes from the engine rather than being restated here,
            // so the message cannot drift from the limit it describes.
            let floor_ms = unsafe { ddtrace_min_interval_ms() }.max(1);
            format!(
                "sample_rate is too high for cpu_profiler={}: the timer period \
                 has a floor of {floor_ms}ms, so {}Hz is the maximum. Lower \
                 sample_rate, or use cpu_profiler=ddtrace",
                profiler.name(),
                1000 / floor_ms,
            )
        }
        _ => format!("status {status}"),
    };
    format!(
        "native CPU profiler '{}' failed to start: {detail}",
        profiler.name()
    )
}

/// Deepest stack we ask the sampler to capture. Matches py-spy's practical
/// ceiling and the renderer's own backstop.
#[allow(dead_code)]
const MAX_NFRAMES: u32 = 128;

/// A running vendored sampler.
pub struct NativeCpu {
    profiler: CpuProfiler,
    running: bool,
}

impl NativeCpu {
    /// Publish the sink configuration, then arm the sampler.
    ///
    /// Order matters: the sampler can push a sample as soon as it is armed, and
    /// the push callback drops samples that arrive before the config is set.
    pub fn start(
        profiler: CpuProfiler,
        config: &CpuConfig,
        ruleset: ThreadTagsSet,
    ) -> Result<Self> {
        warn_about_ignored_options(profiler, config);

        set_sink_config(config.backend_config, ruleset);

        let status = unsafe {
            match profiler {
                CpuProfiler::Ddtrace => ddtrace_start(config.sample_rate, false),
                CpuProfiler::DdtraceCpuTimer => ddtrace_start(config.sample_rate, true),
                CpuProfiler::PySpy => {
                    clear_state();
                    return Err(PyroscopeError::new(
                        "py-spy is not a native CPU profiler backend",
                    ));
                }
            }
        };

        if status != 0 {
            clear_state();
            return Err(PyroscopeError::new(&ddtrace_start_error(profiler, status)));
        }

        ACTIVE_PROFILER.store(profiler as u8, Ordering::Release);

        log::debug!(
            target: "pyroscope-python",
            "started native CPU profiler '{}' at {}Hz",
            profiler.name(),
            config.sample_rate
        );

        Ok(NativeCpu {
            profiler,
            running: true,
        })
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if !self.running {
            return Ok(());
        }
        self.running = false;
        ACTIVE_PROFILER.store(NO_ACTIVE_PROFILER, Ordering::Release);
        unsafe {
            match self.profiler {
                CpuProfiler::Ddtrace | CpuProfiler::DdtraceCpuTimer => ddtrace_stop(),
                CpuProfiler::PySpy => {}
            }
        }
        // Safe to drop buffered samples only after the sampler is disarmed.
        clear_state();
        Ok(())
    }
}

/// Warn about py-spy-only knobs the native sampler cannot honour.
///
/// `oncpu` and `gil_only` are implemented inside py-spy's sampler; the vendored
/// sampler has no equivalent. It always reports on-CPU work, weighted by each
/// thread's own CPU clock, so silently ignoring these would give the user a
/// profile that does not match what they asked for.
fn warn_about_ignored_options(profiler: CpuProfiler, config: &CpuConfig) {
    // Defaults are oncpu=true (include_idle=false) and gil_only=true.
    if config.pyspy.include_idle {
        log::warn!(
            target: "pyroscope-python",
            "oncpu=False is only supported by cpu_profiler=pyspy; \
             '{}' always samples on-CPU work and will ignore it",
            profiler.name()
        );
    }
    if !config.pyspy.gil_only {
        log::warn!(
            target: "pyroscope-python",
            "gil_only=False is only supported by cpu_profiler=pyspy; \
             '{}' will ignore it",
            profiler.name()
        );
    }
}

impl Drop for NativeCpu {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Disarm the native sampler that was running, in a freshly forked child.
///
/// Only the sampler that was actually armed is touched, so a child that was
/// never profiling does not construct the C++ sampler singleton for the first
/// time inside a fork child.
pub fn postfork_child() {
    let active = ACTIVE_PROFILER.swap(NO_ACTIVE_PROFILER, Ordering::AcqRel);
    if active == NO_ACTIVE_PROFILER {
        return;
    }
    unsafe {
        if active == CpuProfiler::Ddtrace as u8 || active == CpuProfiler::DdtraceCpuTimer as u8 {
            ddtrace_postfork_child();
        }
    }
    clear_state();
}

/* --- thin wrappers so the cfg noise lives in one place --- */

unsafe fn ddtrace_min_interval_ms() -> u64 {
    #[cfg(pyroscope_cpu_ddtrace)]
    {
        unsafe { pyroscope_cpu_ddtrace_min_interval_ms() }
    }
    #[cfg(not(pyroscope_cpu_ddtrace))]
    {
        0
    }
}

unsafe fn ddtrace_can_start(_sample_rate_hz: u32, _cpu_timer: bool) -> i32 {
    #[cfg(pyroscope_cpu_ddtrace)]
    {
        unsafe { pyroscope_cpu_ddtrace_can_start(_sample_rate_hz, _cpu_timer.into()) }
    }
    #[cfg(not(pyroscope_cpu_ddtrace))]
    {
        -1
    }
}

unsafe fn ddtrace_start(_sample_rate_hz: u32, _cpu_timer: bool) -> i32 {
    #[cfg(pyroscope_cpu_ddtrace)]
    {
        unsafe { pyroscope_cpu_ddtrace_start(_sample_rate_hz, MAX_NFRAMES, _cpu_timer.into()) }
    }
    #[cfg(not(pyroscope_cpu_ddtrace))]
    {
        -1
    }
}

unsafe fn ddtrace_stop() {
    #[cfg(pyroscope_cpu_ddtrace)]
    unsafe {
        pyroscope_cpu_ddtrace_stop()
    }
}

unsafe fn ddtrace_postfork_child() {
    #[cfg(pyroscope_cpu_ddtrace)]
    unsafe {
        pyroscope_cpu_ddtrace_postfork_child()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Tag;
    use crate::encode::pprof::ffi::FFICpuFrame;
    use std::ffi::c_char;

    fn view(s: &str) -> FFIStringView {
        FFIStringView {
            data: s.as_ptr() as *const c_char,
            len: s.len(),
        }
    }

    fn frame(name: &str, file: &str, line: i32) -> FFICpuFrame {
        FFICpuFrame {
            function_name: view(name),
            file_name: view(file),
            line,
        }
    }

    /// The sink is process-global, so these tests must not run concurrently
    /// with each other; they are serialized by `SINK_TEST_LOCK`.
    static SINK_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Push one sample and hand back the drained buffer.
    ///
    /// Clears the sink afterwards so one test cannot leave a published config
    /// or buffered samples visible to the next one.
    fn push_and_take(
        frames: &[FFICpuFrame],
        pid: u32,
        thread_id: u64,
        thread_name: &str,
        cpu_nanos: u64,
        backend_config: BackendConfig,
        ruleset: ThreadTagsSet,
    ) -> StackBuffer {
        clear_state();
        set_sink_config(backend_config, ruleset);
        pyroscope_cpu_push_sample(FFICpuSample {
            frames: frames.as_ptr(),
            len: frames.len(),
            pid,
            thread_id,
            thread_name: view(thread_name),
            cpu_nanos,
        });
        let buffer = take_buffer().expect("buffer drains");
        clear_state();
        buffer
    }

    #[test]
    fn push_sample_records_frames_leaf_first() {
        let _guard = SINK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let frames = [
            frame("leaf", "/app/a.py", 10),
            frame("caller", "/app/b.py", 4),
        ];

        let buffer = push_and_take(
            &frames,
            0,
            0,
            "",
            10_000_000,
            BackendConfig::default(),
            ThreadTagsSet::new(),
        );

        assert_eq!(buffer.data.len(), 1);
        let (trace, cpu_nanos) = buffer.data.iter().next().unwrap();
        assert_eq!(*cpu_nanos, 10_000_000);
        assert_eq!(trace.frames.len(), 2);
        assert_eq!(trace.frames[0].name, "leaf");
        assert_eq!(trace.frames[0].filename, "/app/a.py");
        assert_eq!(trace.frames[0].line, 10);
        assert_eq!(trace.frames[1].name, "caller");
    }

    #[test]
    fn push_sample_honours_cpu_nanos() {
        let _guard = SINK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let frames = [frame("leaf", "/app/a.py", 1)];

        let buffer = push_and_take(
            &frames,
            0,
            0,
            "",
            7_000_000,
            BackendConfig::default(),
            ThreadTagsSet::new(),
        );

        // Stored verbatim: the encoder must not multiply by the sampling period
        // again for this path.
        assert_eq!(*buffer.data.values().next().unwrap(), 7_000_000);
    }

    #[test]
    fn push_sample_applies_thread_tag_rules() {
        let _guard = SINK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tid: libc::pthread_t = 0x1234 as libc::pthread_t;
        let ruleset = ThreadTagsSet::new();
        ruleset
            .add(crate::backend::ThreadTag::new(
                ThreadId::from(tid),
                Tag::new("role".into(), "worker".into()),
            ))
            .unwrap();

        let frames = [frame("leaf", "/app/a.py", 1)];
        let buffer = push_and_take(
            &frames,
            42,
            0x1234,
            "worker-0",
            10_000_000,
            BackendConfig {
                report_pid: true,
                report_thread_id: true,
                report_thread_name: true,
            },
            ruleset,
        );

        let (trace, _) = buffer.data.iter().next().unwrap();
        let tags: Vec<String> = trace.metadata.tags.iter().map(|t| t.to_string()).collect();
        assert!(tags.contains(&"role=worker".to_string()), "tags: {tags:?}");
        assert!(tags.contains(&"pid=42".to_string()), "tags: {tags:?}");
        assert!(
            tags.contains(&"thread_name=worker-0".to_string()),
            "tags: {tags:?}"
        );
    }

    #[test]
    fn push_sample_ignores_empty_and_zero_cpu_samples() {
        let _guard = SINK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_state();
        set_sink_config(BackendConfig::default(), ThreadTagsSet::new());

        let frames = [frame("leaf", "/app/a.py", 1)];
        // Zero length.
        pyroscope_cpu_push_sample(FFICpuSample {
            frames: frames.as_ptr(),
            len: 0,
            pid: 0,
            thread_id: 0,
            thread_name: view(""),
            cpu_nanos: 1,
        });
        // Zero CPU: a sample that used no CPU is not a CPU sample.
        pyroscope_cpu_push_sample(FFICpuSample {
            frames: frames.as_ptr(),
            len: 1,
            pid: 0,
            thread_id: 0,
            thread_name: view(""),
            cpu_nanos: 0,
        });
        // Null frame pointer.
        pyroscope_cpu_push_sample(FFICpuSample {
            frames: std::ptr::null(),
            len: 3,
            pid: 0,
            thread_id: 0,
            thread_name: view(""),
            cpu_nanos: 1,
        });

        assert!(take_buffer().unwrap().data.is_empty());
        clear_state();
    }

    #[test]
    fn push_sample_without_config_is_dropped() {
        let _guard = SINK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // clear_state() unpublishes the config, mirroring what happens after the
        // sampler is stopped. A late sample must not be reported with default
        // (untagged) metadata.
        clear_state();
        let frames = [frame("leaf", "/app/a.py", 1)];
        pyroscope_cpu_push_sample(FFICpuSample {
            frames: frames.as_ptr(),
            len: frames.len(),
            pid: 0,
            thread_id: 0,
            thread_name: view(""),
            cpu_nanos: 1,
        });

        assert!(take_buffer().unwrap().data.is_empty());
    }
}
