//! The resident memory of the process, which the worker reports with every document so that its
//! caller can replace a worker that has grown (docs/protocol.md).

/// The resident memory of the process, in bytes, as far as the system tells it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Resident {
    /// The resident size now (Linux).
    pub(crate) now: Option<u64>,
    /// The largest resident size the process has had (Linux, macOS and FreeBSD).
    pub(crate) peak: Option<u64>,
}

/// The resident memory of this process: on Linux both sizes, from `/proc/self/status` (in
/// kibibytes, so the page size that `/proc/self/statm` counts in is not needed); on macOS and
/// FreeBSD the peak, from `getrusage`, as the size now needs a call into the kernel (or, on
/// FreeBSD, a `/proc` that is rarely mounted) that only unsafe code can make; elsewhere nothing.
pub(crate) fn resident() -> Resident {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .map_or_else(|_| Resident::default(), |status| parse_status(&status))
    }
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    {
        use nix::sys::resource::{UsageWho, getrusage};
        // In bytes on macOS, in kibibytes on FreeBSD (and Linux).
        let unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
        Resident {
            now: None,
            peak: getrusage(UsageWho::RUSAGE_SELF)
                .ok()
                .and_then(|usage| u64::try_from(usage.max_rss()).ok())
                // FreeBSD samples it at the clock ticks the process runs on, so a process that
                // has hardly run yet reads 0: not known yet rather than empty.
                .filter(|&size| size > 0)
                .map(|size| size * unit),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
    {
        Resident::default()
    }
}

/// Whether the worker hands the memory the allocator keeps free back to the system after each
/// document: on Linux with glibc, unless `FAGUS_MALLOC_TRIM=0` turns it off. After 100-page
/// invoices a worker then keeps about 20 MB instead of 60 to 125 MB, at no measurable cost
/// (`tools/worker-memory.py`, bench/README.md).
pub(crate) fn trims() -> bool {
    cfg!(all(target_os = "linux", target_env = "gnu"))
        && std::env::var_os("FAGUS_MALLOC_TRIM").is_none_or(|value| value != "0")
}

/// Hands the free memory of glibc's allocator back to the system: `malloc_trim(0)` returns the
/// free pages of every arena, not only those at the top of the heap, which `free()` alone gives
/// back. glibc keeps the rest for later allocations, and after a large document a worker would
/// keep it until it ends.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(
    unsafe_code,
    reason = "the only way to call malloc_trim; no safe wrapper exists"
)]
pub(crate) fn trim() {
    // SAFETY: `malloc_trim` takes no pointers and touches only the allocator's own state, under
    // the allocator's locks (glibc documents it as MT-Safe); it is linked with glibc, which this
    // target always has. Its result only says whether memory was released.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Nothing to hand back elsewhere (see the function above).
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn trim() {}

/// The sizes in the text of `/proc/self/status`: `VmRSS` and `VmHWM`, in kibibytes.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_status(status: &str) -> Resident {
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim().strip_suffix("kB"))
            .and_then(|kibibytes| kibibytes.trim().parse::<u64>().ok())
            .map(|kibibytes| kibibytes * 1024)
    };
    Resident {
        now: field("VmRSS:"),
        peak: field("VmHWM:"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_proc_self_status() {
        let status =
            "Name:\tfagus\nVmPeak:\t  420000 kB\nVmHWM:\t   81234 kB\nVmRSS:\t   40960 kB\nThreads:\t3\n";
        assert_eq!(
            parse_status(status),
            Resident {
                now: Some(40960 * 1024),
                peak: Some(81234 * 1024)
            }
        );
        assert_eq!(parse_status("Name:\tfagus\n"), Resident::default());
    }

    #[test]
    fn hands_free_memory_back_without_touching_what_is_in_use() {
        let kept = vec![7_u8; 1 << 20];
        drop(vec![1_u8; 32 << 20]);
        trim();
        assert!(kept.iter().all(|&byte| byte == 7));
    }

    #[test]
    fn knows_its_own_memory_where_the_system_tells_it() {
        // Run for a few clock ticks first, after which FreeBSD knows the peak (see above).
        let start = std::time::Instant::now();
        let mut resident = resident();
        while resident.peak.is_none() && start.elapsed() < std::time::Duration::from_secs(2) {
            std::hint::black_box((0..100_000_u64).sum::<u64>());
            resident = self::resident();
        }
        if cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "freebsd"
        )) {
            // At least the binary's pages are resident.
            assert!(resident.peak.is_some_and(|peak| peak > 1 << 20), "{resident:?}");
        }
        if cfg!(target_os = "linux") {
            assert!(
                resident
                    .now
                    .is_some_and(|now| now > 1 << 20 && Some(now) <= resident.peak)
            );
        }
    }
}
