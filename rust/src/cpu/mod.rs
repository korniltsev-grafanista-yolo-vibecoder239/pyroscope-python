//! CPU profiler selection.
//!
//! py-spy is the default and works everywhere. The others are modes of the
//! vendored native sampler under `cpp/cpu/ddtrace_stack/`, driven over a C ABI:
//! `Ddtrace` runs its wall-clock thread walk, `DdtraceCpuTimer` runs the
//! signal-driven per-thread CPU timers instead.
//!
//! All of them report through the same `StackBuffer` -> `Report` ->
//! `encode::pprof` -> `session` path, so switching implementations changes what
//! is sampled and how much it costs, not how the result is encoded or uploaded.

pub mod native;

use crate::backend::{BackendConfig, Report, ReportBatch, ReportData, ThreadTagsSet};
use crate::error::Result;
use crate::pyspy_backend::Pyspy;
use pyo3::prelude::*;

/// Which CPU profiler implementation to run.
#[pyclass(eq, eq_int, from_py_object)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CpuProfiler {
    /// py-spy: reads CPython structures out of this process's own memory from a
    /// background Rust thread. Works on every supported platform.
    #[default]
    PySpy = 0,
    /// dd-trace-py's `stack` sampler (echion): a dedicated sampler thread walks
    /// every Python thread on a wall-clock interval and weights each stack by
    /// that thread's CPU delta. See `cpp/cpu/ddtrace_stack/VENDOR.md`.
    Ddtrace = 1,
    /// The same vendored sampler, sampling on per-thread CPU time instead of
    /// wall time: every Python thread gets a POSIX CPU timer whose SIGPROF is
    /// delivered on that thread, and the handler captures its own stack. A
    /// thread is only interrupted once it has actually burned another interval
    /// of CPU, so no thread has to be walked to find out whether it ran.
    DdtraceCpuTimer = 2,
}

impl CpuProfiler {
    pub fn name(&self) -> &'static str {
        match self {
            CpuProfiler::PySpy => "pyspy",
            CpuProfiler::Ddtrace => "ddtrace",
            CpuProfiler::DdtraceCpuTimer => "ddtrace-cpu-timer",
        }
    }

    /// Why this profiler cannot run here with this `sample_rate`, or `Ok(())`
    /// if it can.
    ///
    /// Callers surface this as an error rather than silently falling back to
    /// py-spy: a silent fallback would report one implementation's profile
    /// under another's name, and returning "the agent failed to start" would
    /// not tell the user which knob to turn.
    pub fn check_supported(
        &self,
        py: Python<'_>,
        sample_rate: u32,
    ) -> std::result::Result<(), String> {
        let (CpuProfiler::Ddtrace | CpuProfiler::DdtraceCpuTimer) = *self else {
            return Ok(());
        };

        if !native::ddtrace_built() {
            return Err(format!(
                "cpu_profiler={} is not available in this build (platform {}/{}); \
                 it is currently built for Linux only, and not for free-threaded \
                 interpreters",
                self.name(),
                std::env::consts::OS,
                std::env::consts::ARCH,
            ));
        }

        // Both modes discover threads by walking the interpreter's thread list
        // and reading the kernel TID out of PyThreadState::native_thread_id,
        // which only exists from CPython 3.11 on. Without it the thread map
        // stays empty and the profile would come back blank, so refuse up front.
        //
        // The CPU timer additionally reads the running frame out of a thread
        // that is executing bytecode, from a signal handler, so it also needs a
        // frame layout it knows how to walk without the GIL: `instr_ptr` and
        // `f_executable`/`f_code` on `_PyInterpreterFrame`, plus the
        // `FRAME_OWNED_BY_*` discriminants. `DD_CPU_TIMER_SUPPORTED` in
        // `cpp/cpu/ddtrace_stack/src/cpu_timer.cpp` draws that line at 3.12 and
        // this has to agree with it, because on 3.11 the engine compiles to
        // no-ops and would report nothing at all.
        let minimum = match self {
            CpuProfiler::DdtraceCpuTimer => (3, 12),
            _ => (3, 11),
        };
        let v = py.version_info();
        if (v.major, v.minor) < minimum {
            return Err(format!(
                "cpu_profiler={} requires CPython {}.{} or newer (running {}.{})",
                self.name(),
                minimum.0,
                minimum.1,
                v.major,
                v.minor
            ));
        }

        // The rest is only knowable at runtime: whether this process already
        // committed to a CPU accounting mode, and whether the requested rate is
        // within what the sampler can arm. The native side answers both without
        // changing anything.
        if let Some(reason) = native::ddtrace_unsupported_reason(*self, sample_rate) {
            return Err(reason);
        }
        Ok(())
    }
}

/// Everything needed to start a CPU profiler, whichever one was selected.
#[derive(Clone)]
pub struct CpuConfig {
    pub profiler: CpuProfiler,
    pub sample_rate: u32,
    pub backend_config: BackendConfig,
    /// py-spy's own config. Only consulted for [`CpuProfiler::PySpy`], except
    /// that the native path reads `include_idle`/`gil_only` to warn about knobs
    /// it cannot honour.
    pub pyspy: py_spy::config::Config,
}

/// A running CPU profiler.
///
/// `Pyspy` is boxed because it is far larger than `NativeCpu` (which holds only
/// a discriminant and a flag -- the native sampler keeps its state in C++ and in
/// one global buffer), and the agent stores this enum by value.
pub enum CpuBackend {
    PySpy(Box<Pyspy>),
    Native(native::NativeCpu),
}

impl CpuBackend {
    pub fn new(config: CpuConfig, ruleset: ThreadTagsSet) -> Result<Self> {
        match config.profiler {
            CpuProfiler::PySpy => Ok(CpuBackend::PySpy(Box::new(Pyspy::new(
                config.pyspy,
                config.backend_config,
                ruleset,
            )?))),
            other => Ok(CpuBackend::Native(native::NativeCpu::start(
                other, &config, ruleset,
            )?)),
        }
    }

    pub fn reporter(&self) -> CpuReporter {
        match self {
            CpuBackend::PySpy(pyspy) => CpuReporter::PySpy(pyspy.reporter()),
            CpuBackend::Native(_) => CpuReporter::Native,
        }
    }

    pub fn shutdown_thread(&mut self) -> Result<()> {
        match self {
            CpuBackend::PySpy(pyspy) => pyspy.shutdown_thread(),
            CpuBackend::Native(native) => native.shutdown(),
        }
    }
}

/// Drain handle used by the agent's snapshot loop.
pub enum CpuReporter {
    PySpy(crate::pyspy_backend::Reporter),
    /// The native sampler pushes into one global buffer, so there is nothing
    /// per-instance to carry here.
    Native,
}

impl CpuReporter {
    pub fn report(&self) -> Result<ReportBatch> {
        match self {
            CpuReporter::PySpy(reporter) => reporter.report(),
            CpuReporter::Native => {
                let buffer = native::take_buffer()?;
                let reports: Vec<Report> = buffer.into();
                Ok(ReportBatch {
                    profile_type: "process_cpu".into(),
                    // Values are CPU nanoseconds already, not tick counts.
                    data: ReportData::ReportsCpuNanos(reports),
                })
            }
        }
    }
}

/// Disarm the native sampler in a freshly forked child.
///
/// The agent is leaked wholesale in the child (its threads did not survive the
/// fork), so `CpuBackend::drop` never runs and cannot do this.
pub fn postfork_child() {
    native::postfork_child();
}
