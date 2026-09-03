// perf.rs - Performance timing infrastructure for Raven
//
// This module provides timing instrumentation for diagnosing startup latency
// and performance issues. Controlled via RAVEN_PERF environment variable.
//
// Usage:
//   RAVEN_PERF=1 raven --stdio      # Enable timing logs (any non-empty, non-"0"/"false" value)

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Global flag indicating whether performance timing is enabled
static PERF_ENABLED: OnceLock<bool> = OnceLock::new();

/// Check if performance timing is enabled
pub fn is_enabled() -> bool {
    *PERF_ENABLED.get_or_init(|| {
        std::env::var("RAVEN_PERF")
            .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
            .unwrap_or(false)
    })
}

/// RAII timing guard that logs duration on drop
///
/// Use this to measure the duration of a scope:
/// ```
/// use raven::perf::TimingGuard;
///
/// let _guard = TimingGuard::new("operation_name");
/// // ... do work ...
/// // Duration logged when _guard goes out of scope
/// ```
pub struct TimingGuard {
    start: Instant,
    name: &'static str,
    enabled: bool,
}

impl TimingGuard {
    /// Create a new timing guard with the given name
    ///
    /// Duration will be logged at INFO level when the guard is dropped.
    pub fn new(name: &'static str) -> Self {
        Self {
            start: Instant::now(),
            name,
            enabled: is_enabled(),
        }
    }

    /// Get the elapsed time without consuming the guard
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Manually complete the timing and return the duration
    ///
    /// This consumes the guard without logging (useful when you want to handle
    /// the duration yourself).
    pub fn finish(self) -> Duration {
        let elapsed = self.start.elapsed();
        std::mem::forget(self); // Prevent Drop from running
        elapsed
    }
}

impl Drop for TimingGuard {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }

        let elapsed = self.start.elapsed();
        log::info!("[PERF] {} completed in {:?}", self.name, elapsed);
    }
}

/// Aggregated performance metrics for startup analysis
#[derive(Debug, Default, Clone)]
pub struct PerfMetrics {
    /// Duration of workspace scanning
    pub workspace_scan_duration: Option<Duration>,
    /// Duration of PackageLibrary initialization
    pub package_init_duration: Option<Duration>,
    /// Number of files scanned during workspace initialization
    pub files_scanned: usize,
    /// Number of R subprocess calls made
    pub r_subprocess_calls: usize,
    /// Duration of R subprocess calls (total)
    pub r_subprocess_total_duration: Option<Duration>,
}

impl PerfMetrics {
    /// Create a new empty PerfMetrics
    pub fn new() -> Self {
        Self::default()
    }

    /// Log a summary of the metrics.
    ///
    /// The package-library build runs detached from `initialized` and usually
    /// finishes after this summary; `record_package_init` logs its own line in
    /// that case (and only then, so a build that finished first is not logged
    /// twice). That handoff is tracked in the private `PackageInitReporting`
    /// state rather than on this public struct, whose fields are part of the
    /// crate's API.
    pub fn log_summary(&self) {
        if !is_enabled() {
            return;
        }
        // Held for the whole summary so a concurrent `record_package_init`
        // either lands before this (and is printed here) or after (and prints
        // itself), never both. Lock order everywhere is metrics → reporting:
        // callers hold `STARTUP_METRICS` while calling this, and the
        // `record_*` functions take metrics first too.
        let mut reporting = package_init_reporting()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reporting.summary_logged = true;

        log::info!("[PERF] === Startup Performance Summary ===");

        if let Some(d) = self.workspace_scan_duration {
            log::info!(
                "[PERF] Workspace scan: {:?} ({} files)",
                d,
                self.files_scanned
            );
        }

        match self.package_init_duration {
            Some(d) => log::info!(
                "[PERF] Package init: {:?} ({} R calls)",
                d,
                self.r_subprocess_calls
            ),
            None if reporting.disabled => {
                log::info!("[PERF] Package init: disabled")
            }
            None => log::info!("[PERF] Package init: still running (logged on completion)"),
        }

        if let Some(d) = self.r_subprocess_total_duration {
            log::info!("[PERF] R subprocess total: {:?}", d);
        }
    }
}

/// Global metrics collector for startup timing
static STARTUP_METRICS: OnceLock<std::sync::Mutex<PerfMetrics>> = OnceLock::new();

/// Private bookkeeping for how package-init timing gets reported. Kept off
/// [`PerfMetrics`] so that adding it did not change that public struct's field
/// set (downstream exhaustive literals must keep compiling).
#[derive(Debug, Default, Clone, Copy)]
struct PackageInitReporting {
    /// Package awareness is disabled, so no initialization will run.
    disabled: bool,
    /// `log_summary` has run; a later package-init record logs itself.
    summary_logged: bool,
}

static PACKAGE_INIT_REPORTING: OnceLock<std::sync::Mutex<PackageInitReporting>> = OnceLock::new();

fn package_init_reporting() -> &'static std::sync::Mutex<PackageInitReporting> {
    PACKAGE_INIT_REPORTING.get_or_init(|| std::sync::Mutex::new(PackageInitReporting::default()))
}

/// Get or initialize the global startup metrics
pub fn startup_metrics() -> &'static std::sync::Mutex<PerfMetrics> {
    STARTUP_METRICS.get_or_init(|| std::sync::Mutex::new(PerfMetrics::new()))
}

/// Record workspace scan completion
pub fn record_workspace_scan(duration: Duration, files_scanned: usize) {
    if !is_enabled() {
        return;
    }
    if let Ok(mut metrics) = startup_metrics().lock() {
        metrics.workspace_scan_duration = Some(duration);
        metrics.files_scanned = files_scanned;
    }
}

/// Record package initialization completion.
///
/// Also logs the timing directly. The package library builds on a task
/// detached from `initialized`, so it usually finishes *after*
/// [`PerfMetrics::log_summary`] has already printed the startup summary; the
/// stored metric alone would then never be seen.
pub fn record_package_init(duration: Duration, r_calls: usize) {
    if !is_enabled() {
        return;
    }
    // Metric update and log-once decision under one critical section
    // (metrics → reporting, the same order `log_summary`'s callers use), so a
    // summary racing this call cannot print the completion *and* leave
    // `summary_logged` set for this call to print it again.
    let Ok(mut metrics) = startup_metrics().lock() else {
        return;
    };
    let Ok(reporting) = package_init_reporting().lock() else {
        return;
    };
    metrics.package_init_duration = Some(duration);
    metrics.r_subprocess_calls = r_calls;
    if reporting.summary_logged {
        log::info!("[PERF] Package init: {duration:?} ({r_calls} R calls)");
    }
}

/// Record that package awareness is disabled, so the summary does not report
/// an initialization that will never run as "still running".
pub fn record_package_init_disabled() {
    if !is_enabled() {
        return;
    }
    // Same order as `record_package_init`: metrics (held only for ordering),
    // then reporting; decide and log under both.
    let Ok(_metrics) = startup_metrics().lock() else {
        return;
    };
    let Ok(mut reporting) = package_init_reporting().lock() else {
        return;
    };
    reporting.disabled = true;
    if reporting.summary_logged {
        log::info!("[PERF] Package init: disabled");
    }
}

/// Atomic counter for R subprocess calls
static R_SUBPROCESS_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Increment the R subprocess call counter
pub fn increment_r_subprocess_calls() {
    R_SUBPROCESS_CALLS.fetch_add(1, Ordering::Relaxed);
}

/// Get the current R subprocess call count
pub fn get_r_subprocess_calls() -> usize {
    R_SUBPROCESS_CALLS.load(Ordering::Relaxed)
}

/// Returns the peak resident set size (RSS) of the current process in bytes.
///
/// - **macOS**: Uses `libc::getrusage` (`ru_maxrss`, which is in bytes on macOS).
/// - **Linux**: Reads `/proc/self/status` and parses the `VmHWM` field (reported in kB).
/// - **Other platforms**: Returns `None`.
pub fn peak_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        peak_rss_macos()
    }
    #[cfg(target_os = "linux")]
    {
        peak_rss_linux()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn peak_rss_macos() -> Option<u64> {
    use std::mem::MaybeUninit;
    let mut usage = MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage writes into the provided pointer; we check the return value.
    let ret = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if ret == 0 {
        // SAFETY: getrusage succeeded, so the struct is fully initialized.
        let usage = unsafe { usage.assume_init() };
        // On macOS, ru_maxrss is in bytes.
        Some(usage.ru_maxrss as u64)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn peak_rss_linux() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            // Format: "VmHWM:    12345 kB"
            let trimmed = rest.trim();
            let kb_str = trimmed.strip_suffix("kB").unwrap_or(trimmed).trim();
            let kb: u64 = kb_str.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timing_guard_elapsed() {
        let guard = TimingGuard::new("test");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let elapsed = guard.elapsed();
        assert!(elapsed.as_millis() >= 10);
    }

    #[test]
    fn test_timing_guard_finish() {
        let guard = TimingGuard::new("test");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let duration = guard.finish();
        assert!(duration.as_millis() >= 10);
    }

    #[test]
    fn test_perf_metrics_default() {
        let metrics = PerfMetrics::new();
        assert!(metrics.workspace_scan_duration.is_none());
        assert!(metrics.package_init_duration.is_none());
        assert_eq!(metrics.files_scanned, 0);
    }

    #[test]
    fn test_peak_rss_bytes_returns_value_on_supported_platforms() {
        let rss = peak_rss_bytes();
        // On macOS and Linux, we should get a value; on other platforms, None.
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(
                rss.is_some(),
                "peak_rss_bytes() should return Some on macOS/Linux"
            );
            let bytes = rss.unwrap();
            // A running process should have at least some RSS (> 0)
            assert!(bytes > 0, "peak RSS should be > 0, got {}", bytes);
        } else {
            assert!(
                rss.is_none(),
                "peak_rss_bytes() should return None on unsupported platforms"
            );
        }
    }
}
