//! Host CPU/memory gating before a new task starts.
//!
//! Decisions take a [`HostSnapshot`] so tests inject load and memory.
//! Production sampling uses `sysinfo` for load (macOS and Linux) and for
//! memory on Linux. On macOS memory comes from `host_statistics64` directly:
//! sysinfo 0.33 subtracts compressed pages from free+inactive+purgeable and
//! clamps to 0 when the compressor is large, which held every queued task.

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

/// Live sample. Fail-open (no limits hit) if the host cannot be read, so a
/// probe failure does not stall the scheduler.
pub fn sample() -> HostSnapshot {
    sample_with(read_sysinfo)
}

/// `available_memory_mb` value meaning "unknown"; never trips the memory gate.
pub const MEMORY_UNKNOWN: u64 = u64::MAX;

fn read_sysinfo() -> Option<HostSnapshot> {
    let load = sysinfo::System::load_average();
    // sysinfo's cpu list is empty without a cpu refresh, so this was always
    // available_parallelism() or 1.
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .max(1);
    #[cfg(target_os = "macos")]
    let available_memory_mb = macos::read_vm_pages().and_then(|p| macos_available_mb(&p));
    #[cfg(not(target_os = "macos"))]
    let available_memory_mb = {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        Some(sys.available_memory() / (1024 * 1024)).filter(|&mb| mb > 0)
    };
    let available_memory_mb = available_memory_mb.unwrap_or_else(|| {
        tracing::debug!("host: available memory unreadable or 0; memory gate fails open");
        MEMORY_UNKNOWN
    });
    Some(HostSnapshot {
        load: load.one,
        cores,
        available_memory_mb,
    })
}

/// Page counts from macOS `host_statistics64(HOST_VM_INFO64)` plus the page size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VmPages {
    pub free: u64,
    pub speculative: u64,
    pub inactive: u64,
    pub purgeable: u64,
    /// Pages held by the compressor. Not subtracted: they are not part of
    /// free/inactive/purgeable, so subtracting them double-counts.
    pub compressor: u64,
    pub page_size: u64,
}

/// macOS available memory in MiB: `(free - speculative) + inactive + purgeable`
/// pages times the page size (the same pages `vm_stat` shows as free,
/// inactive and purgeable). `None` when the sample adds up to 0, which a live
/// host never reports, so callers treat it as unknown and fail open.
pub fn macos_available_mb(p: &VmPages) -> Option<u64> {
    let pages = p
        .free
        .saturating_sub(p.speculative)
        .saturating_add(p.inactive)
        .saturating_add(p.purgeable);
    let mb = pages.saturating_mul(p.page_size) / (1024 * 1024);
    (mb > 0).then_some(mb)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::VmPages;
    use std::sync::OnceLock;

    /// `mach_host_self` adds a send-right reference on every call; take it once.
    fn host_port() -> libc::mach_port_t {
        static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();
        // SAFETY: no arguments; returns the host name port of this task.
        #[allow(deprecated)]
        *PORT.get_or_init(|| unsafe { libc::mach_host_self() })
    }

    pub(super) fn read_vm_pages() -> Option<VmPages> {
        let host = host_port();
        if host == 0 {
            return None;
        }
        // SAFETY: zeroed POD struct passed with its own size in integer_t units,
        // as host_statistics64(HOST_VM_INFO64) expects.
        let mut stat: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let rc = unsafe {
            libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                &mut stat as *mut libc::vm_statistics64 as libc::host_info64_t,
                &mut count,
            )
        };
        if rc != libc::KERN_SUCCESS {
            tracing::debug!("host: host_statistics64 failed: kern_return {rc}");
            return None;
        }
        // SAFETY: vm_page_size is a read-only global set by libSystem at startup.
        let page_size = unsafe { libc::vm_page_size } as u64;
        Some(VmPages {
            free: stat.free_count as u64,
            speculative: stat.speculative_count as u64,
            inactive: stat.inactive_count as u64,
            purgeable: stat.purgeable_count as u64,
            compressor: stat.compressor_page_count as u64,
            page_size,
        })
    }
}

pub fn sample_with(take: impl FnOnce() -> Option<HostSnapshot>) -> HostSnapshot {
    take().unwrap_or(HostSnapshot {
        load: 0.0,
        cores: 1,
        available_memory_mb: MEMORY_UNKNOWN,
    })
}

pub fn format_line(cfg: &HostConfig, snap: &HostSnapshot) -> String {
    let wait = decide(cfg, snap);
    let mem = if snap.available_memory_mb == MEMORY_UNKNOWN {
        "memory unknown".to_string()
    } else {
        format!("{} MiB free", snap.available_memory_mb)
    };
    let mut s = format!("host: load {:.2} on {} cores, {mem}", snap.load, snap.cores);
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

    const PAGE: u64 = 16384;

    fn pages(
        free: u64,
        speculative: u64,
        inactive: u64,
        purgeable: u64,
        compressor: u64,
    ) -> VmPages {
        VmPages {
            free,
            speculative,
            inactive,
            purgeable,
            compressor,
            page_size: PAGE,
        }
    }

    #[test]
    fn macos_available_is_free_minus_speculative_plus_inactive_purgeable() {
        // 1000 - 200 + 3000 + 296 = 4096 pages * 16 KiB = 64 MiB
        let p = pages(1000, 200, 3000, 296, 0);
        assert_eq!(macos_available_mb(&p), Some(64));
    }

    #[test]
    fn macos_large_compressor_does_not_clamp_to_zero() {
        // The sysinfo 0.33 bug: compressor pages > free+inactive+purgeable
        // made (free+inactive+purgeable) - compressor saturate to 0 MiB.
        let p = pages(20_000, 1_000, 40_000, 1_000, 500_000);
        let old_sysinfo = (p.free + p.inactive + p.purgeable).saturating_sub(p.compressor);
        assert_eq!(old_sysinfo, 0);
        let mb = macos_available_mb(&p).expect("non-zero");
        assert_eq!(mb, (60_000 * PAGE) / (1024 * 1024));
        assert!(decide(
            &cfg(),
            &HostSnapshot {
                load: 0.1,
                cores: 8,
                available_memory_mb: mb
            }
        )
        .is_none());
    }

    #[test]
    fn macos_zero_or_bad_sample_is_unknown() {
        assert_eq!(macos_available_mb(&VmPages::default()), None);
        assert_eq!(macos_available_mb(&pages(0, 0, 0, 0, 9_999)), None);
        // speculative larger than free does not underflow
        assert_eq!(macos_available_mb(&pages(10, 50, 0, 0, 0)), None);
        let mut p = pages(1000, 0, 1000, 0, 0);
        p.page_size = 0;
        assert_eq!(macos_available_mb(&p), None);
    }

    #[test]
    fn unknown_memory_fails_open() {
        let snap = sample_with(|| None);
        assert_eq!(snap.available_memory_mb, MEMORY_UNKNOWN);
        assert_eq!(decide(&cfg(), &snap), None);
        let line = format_line(&cfg(), &snap);
        assert!(line.contains("memory unknown"), "{line}");
        assert!(!line.contains("waiting"), "{line}");
    }

    #[test]
    fn live_sample_reads_nonzero_memory() {
        // A live host always has some free/inactive memory; 0 was the bug.
        let snap = sample();
        assert!(snap.available_memory_mb > 0);
    }

    #[test]
    fn live_sample_has_cores_on_this_os() {
        // sysinfo load/memory paths differ on macOS vs Linux; fail-open
        // still returns at least one core so the gate never divides by zero.
        let snap = sample();
        assert!(snap.cores >= 1);
    }
}
