// perf.rs - Performance timing infrastructure for Raven
//
// This module provides timing instrumentation for diagnosing startup latency
// and performance issues. Controlled via RAVEN_PERF environment variable.
//
// Usage:
//   RAVEN_PERF=1 raven --stdio      # Enable basic timing logs
//   RAVEN_PERF=verbose raven --stdio # Enable detailed timing with thresholds

// Allow unused items that are part of the public API for benchmarks/diagnostics
#![allow(dead_code)]

use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Global flag indicating whether performance timing is enabled
static PERF_ENABLED: OnceLock<bool> = OnceLock::new();

/// Global flag indicating verbose mode (includes threshold warnings)
static PERF_VERBOSE: OnceLock<bool> = OnceLock::new();

/// Check if performance timing is enabled
pub fn is_enabled() -> bool {
    *PERF_ENABLED.get_or_init(|| {
        std::env::var("RAVEN_PERF")
            .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
            .unwrap_or(false)
    })
}

/// Check if verbose mode is enabled
pub fn is_verbose() -> bool {
    *PERF_VERBOSE.get_or_init(|| {
        std::env::var("RAVEN_PERF")
            .map(|v| v.to_lowercase() == "verbose")
            .unwrap_or(false)
    })
}

/// RAII timing guard that logs duration on drop
///
/// Use this to measure the duration of a scope:
/// ```
/// let _guard = TimingGuard::new("operation_name");
/// // ... do work ...
/// // Duration logged when _guard goes out of scope
/// ```
pub struct TimingGuard {
    start: Instant,
    name: &'static str,
    threshold_warn_ms: Option<u64>,
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
            threshold_warn_ms: None,
            enabled: is_enabled(),
        }
    }

    /// Create a timing guard with a warning threshold
    ///
    /// If the operation takes longer than `threshold_ms`, a warning will be logged.
    pub fn with_threshold(name: &'static str, threshold_ms: u64) -> Self {
        Self {
            start: Instant::now(),
            name,
            threshold_warn_ms: Some(threshold_ms),
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

        if let Some(threshold) = self.threshold_warn_ms {
            if elapsed.as_millis() > threshold as u128 && is_verbose() {
                log::warn!(
                    "[PERF] {} exceeded threshold ({}ms > {}ms)",
                    self.name,
                    elapsed.as_millis(),
                    threshold
                );
            }
        }
    }
}

/// Aggregated performance metrics for startup analysis
#[derive(Debug, Default, Clone)]
pub struct PerfMetrics {
    /// Duration of workspace scanning
    pub workspace_scan_duration: Option<Duration>,
    /// Duration of PackageLibrary initialization
    pub package_init_duration: Option<Duration>,
    /// Duration until first diagnostic is published
    pub first_diagnostic_duration: Option<Duration>,
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

    /// Log a summary of the metrics
    pub fn log_summary(&self) {
        if !is_enabled() {
            return;
        }

        log::info!("[PERF] === Startup Performance Summary ===");

        if let Some(d) = self.workspace_scan_duration {
            log::info!(
                "[PERF] Workspace scan: {:?} ({} files)",
                d,
                self.files_scanned
            );
        }

        if let Some(d) = self.package_init_duration {
            log::info!(
                "[PERF] Package init: {:?} ({} R calls)",
                d,
                self.r_subprocess_calls
            );
        }

        if let Some(d) = self.r_subprocess_total_duration {
            log::info!("[PERF] R subprocess total: {:?}", d);
        }

        if let Some(d) = self.first_diagnostic_duration {
            log::info!("[PERF] First diagnostic: {:?}", d);
        }
    }
}

/// Global metrics collector for startup timing
static STARTUP_METRICS: OnceLock<std::sync::Mutex<PerfMetrics>> = OnceLock::new();

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

/// Record package initialization completion
pub fn record_package_init(duration: Duration, r_calls: usize) {
    if !is_enabled() {
        return;
    }
    if let Ok(mut metrics) = startup_metrics().lock() {
        metrics.package_init_duration = Some(duration);
        metrics.r_subprocess_calls = r_calls;
    }
}

/// Record first diagnostic publish
pub fn record_first_diagnostic(duration: Duration) {
    if !is_enabled() {
        return;
    }
    if let Ok(mut metrics) = startup_metrics().lock() {
        if metrics.first_diagnostic_duration.is_none() {
            metrics.first_diagnostic_duration = Some(duration);
        }
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
}
