//! Low-overhead daemon memory sampling for session diagnostics.
//!
//! Fresh has no production RSS sampler (only a Linux test helper that reads
//! `/proc/self/status`). Daemon-process telemetry belongs to the host lifecycle,
//! so this module samples the backend process itself.
//!
//! Samples `VmRSS` / `VmHWM` from `/proc/self/status` on a long interval, then
//! logs average and peak resident memory when the session ends. Child PTY shells
//! are separate processes and are intentionally excluded.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{info, warn};

/// Default sampling interval — a single small `/proc` read every half minute.
pub const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// Background RSS sampler for one daemon / foreground serve lifetime.
pub struct MemoryMonitor {
    inner: Arc<Inner>,
    /// Abort handle for the periodic sampler task (dropped on [`Self::finish`]).
    abort: Option<tokio::task::AbortHandle>,
}

struct Inner {
    started: Instant,
    stats: Mutex<Stats>,
    finalized: AtomicBool,
    warned_read_failure: AtomicBool,
}

#[derive(Debug, Default)]
struct Stats {
    samples: u64,
    sum_rss_kb: u64,
    max_sampled_rss_kb: u64,
    /// Kernel high-water mark (`VmHWM`), when available.
    peak_hwm_kb: u64,
}

/// Parsed memory fields from `/proc/*/status` (kilobytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcStatusMem {
    pub rss_kb: Option<u64>,
    pub hwm_kb: Option<u64>,
}

impl MemoryMonitor {
    /// Start sampling immediately, then on `interval`.
    pub fn start(interval: Duration) -> Self {
        let inner = Arc::new(Inner {
            started: Instant::now(),
            stats: Mutex::new(Stats::default()),
            finalized: AtomicBool::new(false),
            warned_read_failure: AtomicBool::new(false),
        });
        inner.record_sample();

        let sampler = Arc::clone(&inner);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // The first tick completes immediately; we already sampled at start.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                sampler.record_sample();
            }
        });

        Self {
            inner,
            abort: Some(handle.abort_handle()),
        }
    }

    /// Take a final sample and log average / peak once.
    ///
    /// Safe to call multiple times (signal path + post-serve fallback); only the
    /// first call emits the summary.
    pub fn finish(&self) {
        if self
            .inner
            .finalized
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        if let Some(abort) = &self.abort {
            abort.abort();
        }
        self.inner.record_sample();
        self.inner.log_summary();
    }
}

impl Drop for MemoryMonitor {
    fn drop(&mut self) {
        // Best-effort if callers forget `finish` (e.g. unexpected unwind).
        self.finish();
    }
}

impl Inner {
    fn record_sample(&self) {
        match read_self_status_mem() {
            Some(mem) => {
                let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(rss) = mem.rss_kb {
                    stats.samples = stats.samples.saturating_add(1);
                    stats.sum_rss_kb = stats.sum_rss_kb.saturating_add(rss);
                    stats.max_sampled_rss_kb = stats.max_sampled_rss_kb.max(rss);
                }
                if let Some(hwm) = mem.hwm_kb {
                    stats.peak_hwm_kb = stats.peak_hwm_kb.max(hwm);
                }
            }
            None => {
                if !self.warned_read_failure.swap(true, Ordering::SeqCst) {
                    warn!("memory monitor: failed to read /proc/self/status (VmRSS)");
                }
            }
        }
    }

    fn log_summary(&self) {
        let stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        let duration_seconds = self.started.elapsed().as_secs_f64();
        if stats.samples == 0 {
            warn!(
                duration_seconds = (duration_seconds * 10.0).round() / 10.0,
                "session memory usage unavailable (no RSS samples)"
            );
            return;
        }

        let average_rss_kb = (stats.sum_rss_kb as f64) / (stats.samples as f64);
        let peak_rss_kb = if stats.peak_hwm_kb > 0 {
            stats.peak_hwm_kb as f64
        } else {
            stats.max_sampled_rss_kb as f64
        };

        let average_rss_mb = (kb_to_mb(average_rss_kb) * 10.0).round() / 10.0;
        let peak_rss_mb = (kb_to_mb(peak_rss_kb) * 10.0).round() / 10.0;
        let duration_seconds = (duration_seconds * 10.0).round() / 10.0;

        info!(
            average_rss_mb,
            peak_rss_mb,
            samples = stats.samples,
            duration_seconds,
            "session memory usage"
        );
    }
}

fn kb_to_mb(kb: f64) -> f64 {
    kb / 1024.0
}

/// Read resident / peak memory for the current process (Linux `/proc/self/status`).
pub fn read_self_status_mem() -> Option<ProcStatusMem> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        let mem = parse_status_mem(&text);
        if mem.rss_kb.is_none() && mem.hwm_kb.is_none() {
            return None;
        }
        Some(mem)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Parse `VmRSS` / `VmHWM` kilobyte fields from a `/proc/*/status` dump.
pub fn parse_status_mem(text: &str) -> ProcStatusMem {
    let mut mem = ProcStatusMem::default();
    for line in text.lines() {
        if mem.rss_kb.is_none()
            && let Some(kb) = parse_status_kb_line(line, "VmRSS:")
        {
            mem.rss_kb = Some(kb);
        }
        if mem.hwm_kb.is_none()
            && let Some(kb) = parse_status_kb_line(line, "VmHWM:")
        {
            mem.hwm_kb = Some(kb);
        }
        if mem.rss_kb.is_some() && mem.hwm_kb.is_some() {
            break;
        }
    }
    mem
}

fn parse_status_kb_line(line: &str, prefix: &str) -> Option<u64> {
    let rest = line.strip_prefix(prefix)?;
    let trimmed = rest.trim();
    let num = trimmed
        .strip_suffix("kB")
        .or_else(|| trimmed.strip_suffix("KB"))
        .or_else(|| trimmed.strip_suffix("kb"))
        .unwrap_or(trimmed)
        .trim();
    num.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_STATUS: &str = "\
Name:\tfresh-gui
Umask:\t0022
State:\tS (sleeping)
Tgid:\t1234
Ngid:\t0
Pid:\t1234
PPid:\t1
VmPeak:\t  512000 kB
VmSize:\t  400000 kB
VmHWM:\t  250000 kB
VmRSS:\t  180000 kB
RssAnon:\t  100000 kB
";

    #[test]
    fn parse_status_mem_reads_rss_and_hwm() {
        let mem = parse_status_mem(SAMPLE_STATUS);
        assert_eq!(mem.rss_kb, Some(180_000));
        assert_eq!(mem.hwm_kb, Some(250_000));
    }

    #[test]
    fn parse_status_mem_handles_missing_fields() {
        let mem = parse_status_mem("Name:\tfoo\nVmSize:\t10 kB\n");
        assert_eq!(mem.rss_kb, None);
        assert_eq!(mem.hwm_kb, None);
    }

    #[test]
    fn parse_status_mem_tolerates_odd_whitespace_and_suffix() {
        let mem = parse_status_mem("VmRSS:\t\t42 KB\nVmHWM:  7kb\n");
        assert_eq!(mem.rss_kb, Some(42));
        assert_eq!(mem.hwm_kb, Some(7));
    }

    #[test]
    fn parse_status_kb_line_rejects_garbage() {
        assert_eq!(parse_status_kb_line("VmRSS: not-a-number kB", "VmRSS:"), None);
        assert_eq!(parse_status_kb_line("VmSize: 1 kB", "VmRSS:"), None);
    }

    #[test]
    fn kb_to_mb_conversion() {
        assert!((kb_to_mb(1024.0) - 1.0).abs() < f64::EPSILON);
        assert!((kb_to_mb(180_000.0) - 175.78125).abs() < 1e-6);
    }

    #[test]
    fn stats_accumulate_average_and_peak() {
        let inner = Inner {
            started: Instant::now(),
            stats: Mutex::new(Stats::default()),
            finalized: AtomicBool::new(false),
            warned_read_failure: AtomicBool::new(false),
        };
        {
            let mut s = inner.stats.lock().unwrap();
            // Simulate two samples: 100 MB and 200 MB RSS, HWM 250 MB.
            s.samples = 2;
            s.sum_rss_kb = 100 * 1024 + 200 * 1024;
            s.max_sampled_rss_kb = 200 * 1024;
            s.peak_hwm_kb = 250 * 1024;
        }
        let s = inner.stats.lock().unwrap();
        let avg = (s.sum_rss_kb as f64) / (s.samples as f64);
        assert!((kb_to_mb(avg) - 150.0).abs() < 1e-9);
        assert_eq!(s.peak_hwm_kb, 250 * 1024);
        assert!(s.peak_hwm_kb >= s.max_sampled_rss_kb);
    }

    #[tokio::test]
    async fn finish_is_once_only() {
        let monitor = MemoryMonitor::start(Duration::from_secs(3600));
        // Give the spawn a tick to register; sampling itself is sync.
        tokio::task::yield_now().await;
        monitor.finish();
        // Second call must not panic / re-log (finalized guard).
        monitor.finish();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_self_status_mem_returns_positive_rss() {
        let mem = read_self_status_mem().expect("procfs available on Linux CI");
        assert!(mem.rss_kb.unwrap_or(0) > 0, "{mem:?}");
        assert!(mem.hwm_kb.unwrap_or(0) > 0, "{mem:?}");
    }
}
