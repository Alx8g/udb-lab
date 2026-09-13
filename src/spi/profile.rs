//! Diagnostic-only process-wide work attribution. Not performance evidence.
//! Counters include all instrumented callers, including verification/maintenance.
//! Timers overlap where operations nest. Never sum all categories as elapsed time.

#[derive(Clone, Copy)]
pub(crate) enum Event {
    ReadCalls,
    ReadBytes,
    ReadNs,
    WriteCalls,
    WriteBytes,
    WriteNs,
    ChecksumCalls,
    #[cfg_attr(not(feature = "spi-profile"), allow(dead_code))]
    ChecksumBytes,
    ChecksumNs,
    NodeCacheHits,
    NodeCacheMisses,
    ValueCacheHits,
    ValueCacheMisses,
    NodeSaves,
    ValueRecords,
    BuildNs,
    DataSyncNs,
    ManifestWriteNs,
    ManifestSyncNs,
    ManifestReplaceNs,
    DirectorySyncNs,
    FaultChecks,
    FaultCheckNs,
}

#[cfg(feature = "spi-profile")]
mod enabled {
    use super::Event;
    use std::sync::atomic::{AtomicU64, Ordering};
    pub(super) static COUNTERS: [AtomicU64; 23] = [const { AtomicU64::new(0) }; 23];
    pub(super) fn add(event: Event, n: u64) {
        COUNTERS[event as usize].fetch_add(n, Ordering::Relaxed);
    }
    pub(super) fn snapshot() -> serde_json::Value {
        let names = [
            "read_calls",
            "read_bytes",
            "read_ns",
            "write_calls",
            "write_bytes",
            "write_ns",
            "checksum_calls",
            "checksum_bytes",
            "checksum_ns",
            "node_cache_hits",
            "node_cache_misses",
            "value_cache_hits",
            "value_cache_misses",
            "node_saves",
            "value_records",
            "build_ns",
            "data_sync_ns",
            "manifest_write_ns",
            "manifest_sync_ns",
            "manifest_replace_ns",
            "directory_sync_ns",
            "fault_checks",
            "fault_check_ns",
        ];
        names
            .iter()
            .zip(COUNTERS.iter())
            .map(|(name, counter)| {
                (
                    (*name).into(),
                    serde_json::json!(counter.load(Ordering::Relaxed)),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
}

#[inline]
pub(crate) fn add(event: Event, n: u64) {
    #[cfg(feature = "spi-profile")]
    enabled::add(event, n);
    #[cfg(not(feature = "spi-profile"))]
    let _ = (event, n);
}

pub(crate) struct Scope {
    #[cfg(feature = "spi-profile")]
    start: std::time::Instant,
    #[cfg(feature = "spi-profile")]
    event: Event,
}
#[inline]
pub(crate) fn scope(event: Event) -> Scope {
    #[cfg(feature = "spi-profile")]
    {
        Scope {
            start: std::time::Instant::now(),
            event,
        }
    }
    #[cfg(not(feature = "spi-profile"))]
    {
        let _ = event;
        Scope {}
    }
}
impl Drop for Scope {
    #[inline]
    fn drop(&mut self) {
        #[cfg(feature = "spi-profile")]
        enabled::add(
            self.event,
            self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        );
    }
}

/// Monotone process-global diagnostic counters, or null in a normal build.
/// Read deltas around a phase, not across different processes. Counter timers
/// are wall time inside instrumented regions, not CPU sampling or disk latency.
pub fn snapshot() -> serde_json::Value {
    #[cfg(feature = "spi-profile")]
    {
        enabled::snapshot()
    }
    #[cfg(not(feature = "spi-profile"))]
    {
        serde_json::Value::Null
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn profile_build_contract() {
        assert_eq!(super::snapshot().is_object(), cfg!(feature = "spi-profile"));
        #[cfg(not(feature = "spi-profile"))]
        assert_eq!(std::mem::size_of::<super::Scope>(), 0);
    }
}
