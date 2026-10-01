//! How fast the importer copies rooms, and how much memory it takes: [`RoomStats`] for each
//! room, totalled into [`ImportStats`] for the copy, each logged when it ends (and told to
//! [`crate::migration::MigrationObserver::room_copied`], which `hs-cli` turns into metrics), with
//! the process's peak resident memory from [`peak_rss_bytes`].

use std::time::Duration;

/// What copying one room took.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomStats {
    /// The room.
    pub room_id: String,
    /// Events read from Synapse.
    pub events_read: u64,
    /// Of those, newly stored here.
    pub events_stored: u64,
    /// The size of the events read, as Synapse stored them, in bytes.
    pub bytes: u64,
    /// How long the room took, from its first page to its announcement.
    pub elapsed: Duration,
    /// This process's peak resident memory when the room was done, in bytes.
    pub peak_rss_bytes: Option<u64>,
}

fn per_second(n: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = n as f64;
    n / seconds
}

impl RoomStats {
    /// Events read per second.
    #[must_use]
    pub fn events_per_second(&self) -> f64 {
        per_second(self.events_read, self.elapsed)
    }

    /// Bytes of events read per second.
    #[must_use]
    pub fn bytes_per_second(&self) -> f64 {
        per_second(self.bytes, self.elapsed)
    }

    /// The log line for the room: `!room: 100000 events in 52.3 s, 1912 events/s, 1.4 MiB/s,
    /// peak memory 412 MiB`.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{}: {} events read ({} newly stored) in {:.1} s: {:.0} events/s, {}/s, peak memory {}",
            self.room_id,
            self.events_read,
            self.events_stored,
            self.elapsed.as_secs_f64(),
            self.events_per_second(),
            mebibytes(self.bytes_per_second()),
            self.peak_rss_bytes
                .map_or_else(|| "unknown".to_owned(), |b| mebibytes(b as f64)),
        )
    }
}

/// Rooms copied so far in one run of the copy, totalled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportStats {
    /// Rooms copied.
    pub rooms: u64,
    /// Events read.
    pub events_read: u64,
    /// Events newly stored.
    pub events_stored: u64,
    /// Bytes of events read.
    pub bytes: u64,
    /// Time spent in rooms.
    pub elapsed: Duration,
}

impl ImportStats {
    /// Adds one room.
    pub fn add(&mut self, room: &RoomStats) {
        self.rooms += 1;
        self.events_read += room.events_read;
        self.events_stored += room.events_stored;
        self.bytes += room.bytes;
        self.elapsed += room.elapsed;
    }

    /// The log line for the whole copy of rooms.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} rooms, {} events read ({} newly stored) in {:.1} s: {:.0} events/s, {}/s, peak memory {}",
            self.rooms,
            self.events_read,
            self.events_stored,
            self.elapsed.as_secs_f64(),
            per_second(self.events_read, self.elapsed),
            mebibytes(per_second(self.bytes, self.elapsed)),
            peak_rss_bytes().map_or_else(|| "unknown".to_owned(), |b| mebibytes(b as f64)),
        )
    }
}

#[allow(clippy::cast_precision_loss)]
fn mebibytes(bytes: f64) -> String {
    format!("{:.1} MiB", bytes / (1024.0 * 1024.0))
}

/// The most memory this process has held resident at once since it started, in bytes; `None`
/// where the operating system does not say.
#[must_use]
pub fn peak_rss_bytes() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: `getrusage` only writes a `struct rusage` through the pointer it is given. The
        // pointer is to a zeroed, properly sized and aligned `rusage` that lives on this stack
        // frame for the whole call, and nothing else refers to it.
        let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if status != 0 {
            return None;
        }
        // SAFETY: the struct was zero-initialized (every field of `rusage` is an integer or a
        // struct of integers, for which zero is a valid value) and `getrusage` succeeded in
        // filling it.
        let usage = unsafe { usage.assume_init() };
        let max = u64::try_from(usage.ru_maxrss).ok()?;
        // macOS reports bytes; Linux and the BSDs report kilobytes.
        if cfg!(target_os = "macos") {
            Some(max)
        } else {
            Some(max.saturating_mul(1024))
        }
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn peak_memory_counts_what_was_touched() {
        let before = peak_rss_bytes().unwrap();
        assert!(before > 1024 * 1024, "{before}");
        // Hold 64 MiB more than ever before, every page touched.
        let held = vec![1_u8; usize::try_from(before).unwrap() + 64 * 1024 * 1024];
        let after = peak_rss_bytes().unwrap();
        assert!(held.iter().step_by(4096).all(|b| *b == 1));
        assert!(after >= before + 32 * 1024 * 1024, "{before} -> {after}");
    }

    #[test]
    fn rates_and_summaries() {
        let room = RoomStats {
            room_id: "!big:x".into(),
            events_read: 1000,
            events_stored: 990,
            bytes: 2 * 1024 * 1024,
            elapsed: Duration::from_secs(2),
            peak_rss_bytes: Some(100 * 1024 * 1024),
        };
        assert!((room.events_per_second() - 500.0).abs() < 1e-9);
        assert!((room.bytes_per_second() - 1024.0 * 1024.0).abs() < 1e-9);
        assert_eq!(
            room.summary(),
            "!big:x: 1000 events read (990 newly stored) in 2.0 s: 500 events/s, 1.0 MiB/s, \
             peak memory 100.0 MiB"
        );
        let mut all = ImportStats::default();
        all.add(&room);
        all.add(&room);
        assert_eq!(all.rooms, 2);
        assert_eq!(all.events_read, 2000);
        assert!(all.summary().starts_with("2 rooms, 2000 events read"));
        let zero = RoomStats {
            elapsed: Duration::ZERO,
            ..room
        };
        assert!(zero.events_per_second().abs() < f64::EPSILON);
    }
}
