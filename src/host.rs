//! Host CPU/memory gating before a new task starts.
//!
//! Decisions take a [`HostSnapshot`] so tests inject load and memory.
//! Production sampling uses `sysinfo` (macOS and Linux).

use crate::config::HostConfig;
use std::time::{Duration, Instant};

/// One sample of host pressure.
#[derive(Debug, Clone, PartialEq)]
pub struct HostSnapshot {
    /// 1-minute load average.
    pub load: f64,
    pub cores: u32,
    /// Available RAM in MiB.
    pub available_memory_mb: u64,
}

/// Why a start should wait, for a log / status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateWait {
    pub reason: String,
}

/// `None` = may start. `Some` = keep the task queued.
pub fn decide(cfg: &HostConfig, snap: &HostSnapshot) -> Option<GateWait> {
    if !cfg.enabled {
        return None;
    }
    let cores = snap.cores.max(1) as f64;
    if cfg.max_load > 0.0 {
        let per_core = snap.load / cores;
        if per_core > cfg.max_load {
            return Some(GateWait {
                reason: format!(
                    "CPU load {:.2} on {} cores > {:.2} per core",
                    snap.load, snap.cores, cfg.max_load
                ),
            });
        }
    }
    if cfg.min_memory_mb > 0 && snap.available_memory_mb < cfg.min_memory_mb {
        return Some(GateWait {
            reason: format!(
                "available memory {} MiB < {} MiB",
                snap.available_memory_mb, cfg.min_memory_mb
            ),
        });
    }
    None
}

/// While gating is on, start at most one task per settle window.
pub fn settle_ready(cfg: &HostConfig, last_start: Option<Instant>, now: Instant) -> bool {
    if !cfg.enabled {
        return true;
    }
    match last_start {
        None => true,
        Some(t) => now.duration_since(t) >= cfg.settle,
    }
}

/// Live sample. Fail-open (zeros) if sysinfo cannot read, so a probe
/// failure does not stall the scheduler.
pub fn sample() -> HostSnapshot {
    sample_with(read_sysinfo)
}

fn read_sysinfo() -> Option<HostSnapshot> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let load = sysinfo::System::load_average();
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(sys.cpus().len() as u32)
        .max(1);
    let available_memory_mb = sys.available_memory() / (1024 * 1024);
    Some(HostSnapshot {
        load: load.one,
        cores,
        available_memory_mb,
    })
}

pub fn sample_with(take: impl FnOnce() -> Option<HostSnapshot>) -> HostSnapshot {
    take().unwrap_or(HostSnapshot {
        load: 0.0,
        cores: 1,
        available_memory_mb: u64::MAX,
    })
}

pub fn format_line(cfg: &HostConfig, snap: &HostSnapshot) -> String {
    let wait = decide(cfg, snap);
    let mut s = format!(
        "host: load {:.2} on {} cores, {} MiB free",
        snap.load, snap.cores, snap.available_memory_mb
    );
    if !cfg.enabled {
        s.push_str(" (gating off)");
    } else if let Some(w) = wait {
        s.push_str(&format!(" — waiting: {}", w.reason));
    }
    s
}

pub fn settle_remaining(cfg: &HostConfig, last_start: Option<Instant>, now: Instant) -> Duration {
    if !cfg.enabled {
        return Duration::ZERO;
    }
    let Some(t) = last_start else {
        return Duration::ZERO;
    };
    cfg.settle.saturating_sub(now.duration_since(t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg() -> HostConfig {
        HostConfig {
            enabled: true,
            max_load: 0.85,
            min_memory_mb: 512,
            settle: Duration::from_secs(30),
        }
    }

    #[test]
    fn under_limits_starts() {
        let snap = HostSnapshot {
            load: 1.0,
            cores: 8,
            available_memory_mb: 2048,
        };
        assert_eq!(decide(&cfg(), &snap), None);
    }

    #[test]
    fn over_cpu_waits() {
        let snap = HostSnapshot {
            load: 10.0,
            cores: 8,
            available_memory_mb: 4096,
        };
        let w = decide(&cfg(), &snap).expect("cpu");
        assert!(w.reason.contains("CPU load"), "{}", w.reason);
    }

    #[test]
    fn low_memory_waits() {
        let snap = HostSnapshot {
            load: 0.1,
            cores: 8,
            available_memory_mb: 128,
        };
        let w = decide(&cfg(), &snap).expect("mem");
        assert!(w.reason.contains("available memory"), "{}", w.reason);
    }

    #[test]
    fn disabled_or_zero_thresholds_never_block() {
        let snap = HostSnapshot {
            load: 99.0,
            cores: 1,
            available_memory_mb: 1,
        };
        let mut c = cfg();
        c.enabled = false;
        assert_eq!(decide(&c, &snap), None);
        c.enabled = true;
        c.max_load = 0.0;
        c.min_memory_mb = 0;
        assert_eq!(decide(&c, &snap), None);
    }

    #[test]
    fn settle_gap_is_one_at_a_time() {
        let c = cfg();
        let t0 = Instant::now();
        assert!(settle_ready(&c, None, t0));
        assert!(!settle_ready(&c, Some(t0), t0 + Duration::from_secs(5)));
        assert!(settle_ready(&c, Some(t0), t0 + Duration::from_secs(30)));
        let mut off = c;
        off.enabled = false;
        assert!(settle_ready(&off, Some(t0), t0));
    }

    #[test]
    fn inject_fake_sample() {
        let snap = sample_with(|| {
            Some(HostSnapshot {
                load: 3.2,
                cores: 4,
                available_memory_mb: 900,
            })
        });
        assert_eq!(snap.cores, 4);
        assert_eq!(snap.available_memory_mb, 900);
    }

    #[test]
    fn live_sample_has_cores_on_this_os() {
        // sysinfo load/memory paths differ on macOS vs Linux; fail-open
        // still returns at least one core so the gate never divides by zero.
        let snap = sample();
        assert!(snap.cores >= 1);
    }
}
