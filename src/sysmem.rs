//! How much physical memory the machine has.
//!
//! This exists only to size the in-memory frame cache, so a wrong answer costs
//! throughput and never correctness. When the machine cannot be asked, the
//! caller is expected to fall back to something small rather than to guess
//! high: an over-large cache on a small machine is the one failure here that
//! hurts, because it is paid in swapping.
//!
//! This is the only platform-specific code in the crate. `std` has no memory
//! API, and the alternative was a dependency tree for a single number. Windows
//! goes through `GlobalMemoryStatusEx`, Linux reads `/proc/meminfo`, and
//! anything else - macOS included, which has neither - answers `None` and gets
//! the conservative default.

/// Total physical memory in bytes, or `None` if this platform cannot say.
///
/// Total rather than free, deliberately. The cache is a ceiling that fills over
/// minutes of rendering, so sizing it against a free figure sampled once at
/// startup would pin a long-lived decision to a momentary reading.
pub fn total_bytes() -> Option<u64> {
    imp::total_bytes()
}

#[cfg(windows)]
mod imp {
    /// `MEMORYSTATUSEX`. The field order and widths are load-bearing: the API
    /// is told the size of this struct and writes it back in place.
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[allow(non_snake_case)]
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    pub fn total_bytes() -> Option<u64> {
        let mut status = MemoryStatusEx {
            length: std::mem::size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_phys: 0,
            avail_phys: 0,
            total_page_file: 0,
            avail_page_file: 0,
            total_virtual: 0,
            avail_virtual: 0,
            avail_extended_virtual: 0,
        };
        // SAFETY: `status` is a live, correctly laid out MEMORYSTATUSEX with
        // `length` set to its own size, which is the whole contract of the
        // call. It writes into the struct and nowhere else, and borrows it only
        // for the duration of the call.
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        if ok == 0 || status.total_phys == 0 {
            return None;
        }
        Some(status.total_phys)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn total_bytes() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        super::parse_meminfo(&text)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn total_bytes() -> Option<u64> {
        None
    }
}

/// Pull `MemTotal` out of `/proc/meminfo` and return it in bytes.
///
/// Split from the read so it can be tested on a machine that has no `/proc`,
/// which is most of the ones this is developed on.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_meminfo(text: &str) -> Option<u64> {
    let rest = text
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?;
    let mut parts = rest.split_whitespace();
    let value: u64 = parts.next()?.parse().ok()?;
    // Linux has always written kB on this line and it is unlikely ever to write
    // anything else, but it does write it rather than leave it implied, so read
    // it rather than assume it.
    match parts.next() {
        Some("kB") => value.checked_mul(1024),
        None => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_meminfo;

    const SAMPLE: &str = "\
MemTotal:       16316576 kB
MemFree:         1234567 kB
MemAvailable:    8765432 kB
";

    #[test]
    fn reads_memtotal_in_kb() {
        assert_eq!(parse_meminfo(SAMPLE), Some(16_316_576 * 1024));
    }

    #[test]
    fn memtotal_need_not_be_first() {
        let text = "MemFree: 100 kB\nMemTotal: 2048 kB\n";
        assert_eq!(parse_meminfo(text), Some(2048 * 1024));
    }

    #[test]
    fn a_bare_number_is_taken_as_bytes() {
        assert_eq!(parse_meminfo("MemTotal: 4096\n"), Some(4096));
    }

    #[test]
    fn an_unknown_unit_is_not_guessed_at() {
        assert_eq!(parse_meminfo("MemTotal: 16 GB\n"), None);
    }

    #[test]
    fn absent_or_unparsable_is_none() {
        assert_eq!(parse_meminfo(""), None);
        assert_eq!(parse_meminfo("MemFree: 100 kB\n"), None);
        assert_eq!(parse_meminfo("MemTotal:\n"), None);
        assert_eq!(parse_meminfo("MemTotal: lots kB\n"), None);
    }

    #[test]
    fn an_absurd_value_does_not_wrap() {
        // Not reachable from a real kernel, but the multiply is there and this
        // is what it is guarded for.
        let text = format!("MemTotal: {} kB\n", u64::MAX);
        assert_eq!(parse_meminfo(&text), None);
    }
}
