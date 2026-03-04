/// Process CPU and memory statistics.
///
/// Linux (including Android): reads /proc/self/stat and /proc/self/status.
/// macOS: uses getrusage(2) via libc.
/// Other platforms: returns 0.

// ── CPU ticks ─────────────────────────────────────────────────────────────────

/// Cumulative CPU ticks (utime+stime) from /proc/self/stat.
/// One tick = 1/CLK_TCK second; CLK_TCK is 100 on all common Linux/Android systems.
#[cfg(target_os = "linux")]
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn read_cpu_ticks() -> Option<u64> {
    None
}

// ── Memory ────────────────────────────────────────────────────────────────────

/// Resident set size in MiB from /proc/self/status.
#[cfg(target_os = "linux")]
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn read_mem_mb() -> u32 {
    0
}
