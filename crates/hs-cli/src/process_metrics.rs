//! The process's own memory, on `/metrics`: `process_resident_memory_bytes` and
//! `process_virtual_memory_bytes`, the names every Prometheus client library exports them under.
//!
//! Before this module existed the only view of this server's memory was the kubelet's
//! `container_memory_working_set_bytes`, which counts the page cache with the heap and is read
//! from outside the process: on 2026-10-10 the demo pod's memory crept 13 MiB an hour while idle
//! and nothing on `/metrics` said whether the process or the page cache was growing
//! (`docs/status/06-federation.md`, "2026-10-10: the leak hunt"). These two gauges are read at
//! scrape time from the operating system -- `/proc/self/statm` on Linux, `task_info` on macOS --
//! and are absent, not zero, on a platform where neither works, so a dashboard never mistakes "not
//! measured" for "no memory".

use prometheus_client::collector::Collector;
use prometheus_client::encoding::DescriptorEncoder;
use prometheus_client::metrics::MetricType;
use prometheus_client::registry::Registry;

/// This process's memory as the operating system accounts it, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessMemory {
    /// The resident set: pages of this process in physical memory right now (what `ps` prints
    /// as `RSS`, what the kubelet's working set is mostly made of for a process with little
    /// file I/O).
    pub resident_bytes: u64,
    /// The virtual address space mapped. On macOS this is tens or hundreds of gigabytes for any
    /// process (shared caches and reservations) and says little; on Linux it is the `VSZ` of
    /// `ps`.
    pub virtual_bytes: u64,
}

/// Reads this process's memory from the operating system. `None` where it cannot be read: a
/// platform this module has no reader for, or a reader that failed.
#[must_use]
pub fn process_memory() -> Option<ProcessMemory> {
    read_process_memory()
}

#[cfg(target_os = "linux")]
fn read_process_memory() -> Option<ProcessMemory> {
    // `/proc/self/statm`: "size resident shared text lib data dt", in pages.
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let mut fields = statm.split_whitespace();
    let size_pages: u64 = fields.next()?.parse().ok()?;
    let resident_pages: u64 = fields.next()?.parse().ok()?;
    // SAFETY: `sysconf` takes an integer name and reads no memory of ours; it returns -1 for a
    // name it does not know, which is checked below rather than used.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = u64::try_from(page_size).ok().filter(|size| *size > 0)?;
    Some(ProcessMemory {
        resident_bytes: resident_pages.saturating_mul(page_size),
        virtual_bytes: size_pages.saturating_mul(page_size),
    })
}

#[cfg(target_os = "macos")]
fn read_process_memory() -> Option<ProcessMemory> {
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::uninit();
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    // SAFETY: `task_info` fills at most `count` words of the buffer it is given, and the buffer
    // is a `mach_task_basic_info` with `count` set to exactly that struct's size in words
    // (`MACH_TASK_BASIC_INFO_COUNT`), as the Mach interface asks. `mach_task_self()` names this
    // process and needs no deallocation. The struct is read only after the call reported
    // success, which is when the kernel has written all of it.
    // `libc` deprecates `mach_task_self` in favour of the `mach2` crate; one symbol is not worth
    // a dependency, and the function is a read of the task port the kernel gave this process.
    #[allow(deprecated)]
    let kr = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast::<libc::integer_t>(),
            &mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return None;
    }
    // SAFETY: the kernel reported success, so the whole struct was written (see above).
    let info = unsafe { info.assume_init() };
    Some(ProcessMemory {
        resident_bytes: info.resident_size,
        virtual_bytes: info.virtual_size,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_process_memory() -> Option<ProcessMemory> {
    None
}

/// Puts `process_resident_memory_bytes` and `process_virtual_memory_bytes` on a registry, read
/// from the operating system at every scrape. On a platform where they cannot be read, neither
/// is rendered.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessMemoryCollector;

impl Collector for ProcessMemoryCollector {
    fn encode(&self, mut encoder: DescriptorEncoder) -> Result<(), std::fmt::Error> {
        let Some(memory) = process_memory() else {
            return Ok(());
        };
        encoder
            .encode_descriptor(
                "process_resident_memory_bytes",
                "Resident memory size in bytes",
                None,
                MetricType::Gauge,
            )?
            .encode_gauge(&as_gauge(memory.resident_bytes))?;
        encoder
            .encode_descriptor(
                "process_virtual_memory_bytes",
                "Virtual memory size in bytes",
                None,
                MetricType::Gauge,
            )?
            .encode_gauge(&as_gauge(memory.virtual_bytes))?;
        Ok(())
    }
}

/// A byte count as the `i64` a gauge is encoded from; a count past `i64::MAX` is pinned there.
fn as_gauge(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// Registers [`ProcessMemoryCollector`] on `registry` (the shared one, in `hs serve`).
pub fn register_metrics(registry: &mut Registry) {
    registry.register_collector(Box::new(ProcessMemoryCollector));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On the platforms CI and the desktop run (Linux, macOS), memory is read and is plausible:
    /// a resident set of at least a few megabytes for a test binary, and a virtual size no
    /// smaller than it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn reads_this_process_memory() {
        let memory = process_memory().expect("memory is readable here");
        assert!(
            memory.resident_bytes > 1024 * 1024,
            "resident {} bytes",
            memory.resident_bytes
        );
        assert!(memory.virtual_bytes >= memory.resident_bytes, "{memory:?}");
    }

    /// The reader follows the heap: 64 MiB allocated and touched shows up in the resident set
    /// (at least half of it, allowing for the compressor and the allocator's own reuse).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn resident_memory_follows_a_large_allocation() {
        let before = process_memory().expect("memory is readable here");
        let mut block = vec![0u8; 64 * 1024 * 1024];
        for (i, byte) in block.iter_mut().enumerate().step_by(4096) {
            *byte = (i % 251) as u8;
        }
        let after = process_memory().expect("memory is readable here");
        let grew = after.resident_bytes.saturating_sub(before.resident_bytes);
        assert!(
            grew >= 32 * 1024 * 1024,
            "resident grew by {grew} bytes (before {before:?}, after {after:?})"
        );
        assert_eq!(block[4096], (4096usize % 251) as u8);
    }

    /// What `/metrics` renders: both gauges, under the standard names, with values.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn renders_both_gauges_under_the_standard_names() {
        let mut registry = Registry::default();
        register_metrics(&mut registry);
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();
        let resident = text
            .lines()
            .find(|line| line.starts_with("process_resident_memory_bytes "))
            .unwrap_or_else(|| panic!("no resident gauge in {text}"));
        let value: f64 = resident.split(' ').nth(1).unwrap().parse().unwrap();
        assert!(value > 1024.0 * 1024.0, "{resident}");
        assert!(
            text.contains("# TYPE process_virtual_memory_bytes gauge"),
            "{text}"
        );
        assert!(
            text.lines()
                .any(|line| line.starts_with("process_virtual_memory_bytes ")),
            "{text}"
        );
    }

    /// Through `hs_telemetry::Metrics`, the way `hs serve` registers it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn registers_on_the_shared_metrics() {
        let metrics = hs_telemetry::metrics::Metrics::new();
        metrics.with_registry(register_metrics);
        let text = metrics.encode_to_string().unwrap();
        assert!(text.contains("process_resident_memory_bytes "), "{text}");
    }
}
