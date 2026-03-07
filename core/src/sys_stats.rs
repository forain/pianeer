use crate::types::ProcStats;

/// Collects CPU / VU / voice stats each tick.
/// Holds the inter-tick state (CPU counters, clip timer, last values) so
/// callers don't have to maintain those variables themselves.
pub struct StatsCollector {
    prev_cpu_ticks:   u64,
    prev_cpu_instant: std::time::Instant,
    clip_until:       Option<std::time::Instant>,
    last_stats:       ProcStats,
    last_sustain:     bool,
}

impl StatsCollector {
    pub fn new() -> Self {
        Self {
            prev_cpu_ticks:   read_cpu_ticks().unwrap_or(0),
            prev_cpu_instant: std::time::Instant::now(),
            clip_until:       None,
            last_stats:       ProcStats::default(),
            last_sustain:     false,
        }
    }

    /// Sample the sampler state + OS counters and return `(new_stats, sustain)`.
    pub fn tick(&mut self, state: &std::sync::Mutex<crate::sampler::SamplerState>) -> (ProcStats, bool) {
        let (sustain, voices, peak_l, peak_r, clip_now) = state.try_lock()
            .map(|mut s| {
                let clip = s.clip_l || s.clip_r;
                s.clip_l = false; s.clip_r = false;
                (s.sustain_pedal, s.active_voice_count(), s.peak_l, s.peak_r, clip)
            })
            .unwrap_or((
                self.last_sustain,
                self.last_stats.voices,
                self.last_stats.peak_l,
                self.last_stats.peak_r,
                false,
            ));

        let now = std::time::Instant::now();
        if clip_now { self.clip_until = Some(now + std::time::Duration::from_secs(2)); }
        let clip = self.clip_until.map(|t| now < t).unwrap_or(false);

        let elapsed = now.duration_since(self.prev_cpu_instant).as_secs_f32().max(0.001);
        let ticks_now = read_cpu_ticks().unwrap_or(self.prev_cpu_ticks);
        let cpu_pct = (((ticks_now.saturating_sub(self.prev_cpu_ticks)) as f32 / 100.0)
            / elapsed * 100.0)
            .round().clamp(0.0, 100.0) as u8;
        self.prev_cpu_ticks = ticks_now;
        self.prev_cpu_instant = now;

        let stats = ProcStats { cpu_pct, mem_mb: read_mem_mb(), voices, peak_l, peak_r, clip };
        self.last_stats = stats.clone();
        self.last_sustain = sustain;
        (stats, sustain)
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Process CPU and memory statistics.
///
/// Linux (including Android): reads /proc/self/stat and /proc/self/status.
/// macOS: uses getrusage(2) via libc.
/// Other platforms: returns 0.

// ── CPU ticks ─────────────────────────────────────────────────────────────────

/// Cumulative CPU ticks (utime+stime) from /proc/self/stat.
/// One tick = 1/CLK_TCK second; CLK_TCK is 100 on all common Linux/Android systems.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn read_cpu_ticks() -> Option<u64> {
    let data = std::fs::read_to_string("/proc/self/stat").ok()?;
    // comm field may contain spaces; find the last ')' to skip it.
    let after_comm = data.rfind(')')?.checked_add(1)?;
    let rest = data[after_comm..].trim_start();
    // Fields after ')': state ppid pgrp session tty_nr tpgid flags
    //                    minflt cminflt majflt cmajflt utime(11) stime(12)
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Cumulative CPU centiseconds (utime+stime) via getrusage.
/// Returns centiseconds (100ths of a second) to match the Linux CLK_TCK=100
/// convention so the same `delta / 100.0 / elapsed * 100` formula applies.
#[cfg(target_os = "macos")]
pub fn read_cpu_ticks() -> Option<u64> {
    use std::mem::MaybeUninit;
    let mut usage = MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    let u = unsafe { usage.assume_init() };
    let utime_cs = u.ru_utime.tv_sec as u64 * 100 + u.ru_utime.tv_usec as u64 / 10_000;
    let stime_cs = u.ru_stime.tv_sec as u64 * 100 + u.ru_stime.tv_usec as u64 / 10_000;
    Some(utime_cs + stime_cs)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub fn read_cpu_ticks() -> Option<u64> {
    None
}

// ── Memory ────────────────────────────────────────────────────────────────────

/// Resident set size in MiB from /proc/self/status.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn read_mem_mb() -> u32 {
    let data = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in data.lines() {
        if line.starts_with("VmRSS:") {
            if let Some(kb) = line.split_whitespace().nth(1).and_then(|s| s.parse::<u32>().ok()) {
                return kb / 1024;
            }
        }
    }
    0
}

/// Peak RSS in MiB via getrusage.
/// On macOS ru_maxrss is in bytes and reflects peak RSS.
#[cfg(target_os = "macos")]
pub fn read_mem_mb() -> u32 {
    use std::mem::MaybeUninit;
    let mut usage = MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0;
    }
    let u = unsafe { usage.assume_init() };
    (u.ru_maxrss as u64 / 1024 / 1024) as u32
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub fn read_mem_mb() -> u32 {
    0
}
