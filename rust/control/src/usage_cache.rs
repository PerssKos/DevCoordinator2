use devcoordinator2_api::results::{UsageRepository, UsageSnapshot};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const FRESH_FOR: Duration = Duration::from_secs(30);
const MAX_ENTRIES: usize = 128;
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING: usize = 32;
const MAX_WAIT: Duration = Duration::from_millis(650);

type Loader = Box<dyn FnOnce() -> Result<UsageRepository, ProtocolError> + Send>;
#[derive(Clone, Default)]
pub(crate) struct UsageCache(Arc<(Mutex<State>, Condvar)>);
#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    pending: VecDeque<(String, Loader)>,
    running: bool,
}
struct Entry {
    repository_id: String,
    report: UsageRepository,
    completed: Option<Instant>,
    touched: Instant,
    refreshing: bool,
    refresh_failed: bool,
    updated_at_ms: Option<u64>,
    bytes: usize,
}
impl UsageCache {
    pub(crate) fn get(
        &self,
        key: String,
        mut empty: UsageRepository,
        load: impl FnOnce() -> Result<UsageRepository, ProtocolError> + Send + 'static,
    ) -> UsageRepository {
        let (state, changed) = &*self.0;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.entries.contains_key(&key) && state.entries.len() >= MAX_ENTRIES {
            let oldest = state
                .entries
                .iter()
                .filter(|(_, e)| !e.refreshing)
                .min_by_key(|(_, e)| e.touched)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                state.entries.remove(&oldest);
            } else {
                empty.coverage.snapshot = Some(UsageSnapshot {
                    updated_at_ms: None,
                    refreshing: false,
                    refresh_failed: true,
                });
                return empty;
            }
        }
        let room = state.pending.len() < MAX_PENDING;
        let entry = state.entries.entry(key.clone()).or_insert_with(|| Entry {
            repository_id: empty.repository_id.clone(),
            report: empty,
            completed: None,
            touched: Instant::now(),
            refreshing: false,
            refresh_failed: false,
            updated_at_ms: None,
            bytes: 0,
        });
        entry.touched = Instant::now();
        let start = !entry.refreshing && entry.completed.is_none_or(|at| at.elapsed() >= FRESH_FOR);
        if start {
            entry.refreshing = room;
            entry.refresh_failed = !room;
        }
        let mut report = entry.report.clone();
        let source = report.coverage.snapshot.take();
        report.coverage.snapshot = Some(UsageSnapshot {
            updated_at_ms: entry.updated_at_ms,
            refreshing: entry.refreshing || source.as_ref().is_some_and(|s| s.refreshing),
            refresh_failed: entry.refresh_failed
                || source.as_ref().is_some_and(|s| s.refresh_failed),
        });
        if start && room {
            state.pending.push_back((key, Box::new(load)));
        }
        let spawn = !state.running && !state.pending.is_empty();
        if spawn {
            state.running = true;
        }
        drop(state);
        if spawn {
            let cache = self.clone();
            if std::thread::Builder::new()
                .name("usage-refresh".into())
                .spawn(move || cache.run())
                .is_err()
            {
                let mut state = self
                    .0
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.running = false;
                while let Some((key, _)) = state.pending.pop_front() {
                    if let Some(entry) = state.entries.get_mut(&key) {
                        entry.refreshing = false;
                        entry.refresh_failed = true;
                    }
                }
                if let Some(snapshot) = report.coverage.snapshot.as_mut() {
                    snapshot.refreshing = false;
                    snapshot.refresh_failed = true;
                }
                changed.notify_all();
            }
        }
        report
    }
    fn run(&self) {
        loop {
            let next = {
                let mut state = self
                    .0
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match state.pending.pop_front() {
                    Some(job) => Some(job),
                    None => {
                        state.running = false;
                        None
                    }
                }
            };
            let Some((key, load)) = next else {
                return;
            };
            let result = catch_unwind(AssertUnwindSafe(load)).unwrap_or_else(|_| {
                Err(ProtocolError::new(
                    ErrorCode::InternalError,
                    "usage refresh failed",
                ))
            });
            self.finish(&key, result);
        }
    }
    fn finish(&self, key: &str, result: Result<UsageRepository, ProtocolError>) {
        let (state, changed) = &*self.0;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = result.as_ref().ok().and_then(report_bytes);
        if let Some(bytes) = bytes {
            while state
                .entries
                .iter()
                .filter(|(k, _)| k.as_str() != key)
                .map(|(_, e)| e.bytes)
                .sum::<usize>()
                .saturating_add(bytes)
                > MAX_CACHE_BYTES
            {
                let oldest = state
                    .entries
                    .iter()
                    .filter(|(k, e)| k.as_str() != key && !e.refreshing)
                    .min_by_key(|(_, e)| e.touched)
                    .map(|(k, _)| k.clone());
                if let Some(oldest) = oldest {
                    state.entries.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        let room = bytes.is_some_and(|bytes| {
            state
                .entries
                .iter()
                .filter(|(k, _)| k.as_str() != key)
                .map(|(_, e)| e.bytes)
                .sum::<usize>()
                .saturating_add(bytes)
                <= MAX_CACHE_BYTES
        });
        if let Some(entry) = state.entries.get_mut(key) {
            let usable = room
                && result.as_ref().is_ok_and(|report| {
                    !report
                        .coverage
                        .unavailable_reasons
                        .contains_key("source_unavailable")
                        && report.coverage.available_collectors
                            >= entry.report.coverage.available_collectors
                        && (report.coverage.available_collectors > 0
                            || report.coverage.configured_collectors == 0
                            || (!report.coverage.unavailable_reasons.is_empty()
                                && report
                                    .coverage
                                    .unavailable_reasons
                                    .keys()
                                    .all(|r| r == "mapping_unavailable")))
                });
            if let Ok(report) = result {
                if usable || (room && entry.updated_at_ms.is_none()) {
                    if usable {
                        entry.updated_at_ms = report
                            .coverage
                            .snapshot
                            .as_ref()
                            .and_then(|s| s.updated_at_ms)
                            .or(Some(report.generated_at_ms));
                    }
                    entry.bytes = bytes.unwrap_or(0);
                    entry.report = report;
                }
            }
            entry.refresh_failed = !usable;
            entry.refreshing = false;
            entry.completed = Some(Instant::now());
        }
        changed.notify_all();
    }
    pub(crate) fn wait(&self, repository_id: Option<&str>) {
        let state = self
            .0
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _waited =
            self.0
                .1
                .wait_timeout_while(state, MAX_WAIT, |state| {
                    state.entries.values().any(|e| {
                        e.refreshing && repository_id.is_none_or(|id| e.repository_id == id)
                    })
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}
// Bound serialization without allocating a second copy. The multiplier covers
// numeric-heavy structs, vectors and owned strings in display projections.
fn report_bytes(report: &UsageRepository) -> Option<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(value.len());
            if self.0 > MAX_ENTRY_BYTES / 8 {
                return Err(std::io::Error::other("snapshot too large"));
            }
            Ok(value.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Counter(0);
    serde_json::to_writer(&mut count, report).ok()?;
    Some(count.0.saturating_mul(8))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{RepositoryRecord, combine};
    use devcoordinator2_api::params::UsageRange;
    use std::collections::BTreeMap;
    use std::sync::mpsc;

    fn report(tokens: Option<u64>) -> UsageRepository {
        let mut report = combine(
            &RepositoryRecord {
                repository_id: "fixture".into(),
                display_name: "Fixture".into(),
                root_path: "/fixture".into(),
            },
            UsageRange::Hours24,
            100,
            0,
            100,
            1,
            &[],
            BTreeMap::new(),
            0,
        );
        report.totals.total_tokens = tokens;
        report.series[0].total_tokens = tokens;
        report
    }

    fn expire(cache: &UsageCache, key: &str) {
        cache
            .0
            .0
            .lock()
            .unwrap()
            .entries
            .get_mut(key)
            .unwrap()
            .completed = Some(Instant::now() - FRESH_FOR);
    }

    #[test]
    fn cold_reads_share_one_refresh_and_wait_for_completion() {
        let cache = UsageCache::default();
        let (release, blocked) = mpsc::channel();
        let started = Instant::now();
        let first = cache.get("key".into(), report(None), move || {
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(report(Some(100)))
        });
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(first.totals.total_tokens, None);
        assert!(first.coverage.snapshot.unwrap().refreshing);
        for _ in 0..10 {
            let duplicate = cache.get("key".into(), report(None), || panic!("duplicate loader"));
            assert!(duplicate.coverage.snapshot.unwrap().refreshing);
        }
        release.send(()).unwrap();
        cache.wait(Some("fixture"));
        let ready = cache.get("key".into(), report(None), || panic!("fresh loader"));
        assert_eq!(ready.totals.total_tokens, Some(100));
        assert_eq!(
            ready.coverage.snapshot,
            Some(UsageSnapshot {
                updated_at_ms: Some(100),
                refreshing: false,
                refresh_failed: false,
            })
        );
    }

    #[test]
    fn failed_refresh_keeps_saved_data_then_recovers_without_inventing_zero() {
        let cache = UsageCache::default();
        cache.get("key".into(), report(None), || Ok(report(Some(100))));
        cache.wait(None);
        expire(&cache, "key");
        let saved = cache.get("key".into(), report(None), || {
            let mut failed = report(None);
            failed
                .coverage
                .unavailable_reasons
                .insert("source_unavailable".into(), 1);
            Ok(failed)
        });
        assert_eq!(saved.totals.total_tokens, Some(100));
        cache.wait(None);
        let failed = cache.get("key".into(), report(None), || panic!("failure cooldown"));
        assert_eq!(failed.totals.total_tokens, Some(100));
        assert!(failed.coverage.snapshot.unwrap().refresh_failed);
        expire(&cache, "key");
        cache.get("key".into(), report(None), || Ok(report(Some(0))));
        cache.wait(None);
        let zero = cache.get("key".into(), report(None), || panic!("fresh loader"));
        assert_eq!(zero.totals.total_tokens, Some(0));
        assert!(!zero.coverage.snapshot.unwrap().refresh_failed);
        expire(&cache, "key");
        cache.get("key".into(), report(None), || Ok(report(None)));
        cache.wait(None);
        assert_eq!(
            cache
                .get("key".into(), report(None), || panic!())
                .totals
                .total_tokens,
            None
        );
    }

    #[test]
    fn missing_repository_history_is_not_a_refresh_failure() {
        let cache = UsageCache::default();
        cache.get("key".into(), report(None), || {
            let mut missing = report(None);
            missing.coverage.configured_collectors = 1;
            missing.coverage.available_collectors = 0;
            missing
                .coverage
                .unavailable_reasons
                .insert("mapping_unavailable".into(), 1);
            Ok(missing)
        });
        cache.wait(None);
        let missing = cache.get("key".into(), report(None), || panic!("fresh loader"));
        assert_eq!(missing.totals.total_tokens, None);
        let snapshot = missing.coverage.snapshot.unwrap();
        assert_eq!(snapshot.updated_at_ms, Some(100));
        assert!(!snapshot.refresh_failed);
    }

    #[test]
    fn independent_keys_do_not_share_data_and_restart_is_empty() {
        let cache = UsageCache::default();
        cache.get("repo-one-window-one".into(), report(None), || {
            Ok(report(Some(100)))
        });
        cache.wait(None);
        let (release, blocked) = mpsc::channel();
        let other = cache.get("repo-one-window-two".into(), report(None), move || {
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(report(Some(200)))
        });
        assert_eq!(other.totals.total_tokens, None);
        release.send(()).unwrap();
        cache.wait(None);
        assert_eq!(
            cache
                .get("repo-one-window-one".into(), report(None), || panic!())
                .totals
                .total_tokens,
            Some(100)
        );
        assert!(UsageCache::default().0.0.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn refresh_admission_queues_second_scan_without_losing_its_refresh() {
        let cache = UsageCache::default();
        let (release, release_rx) = mpsc::channel();
        let (started_tx, started) = mpsc::channel();
        cache.get("first".into(), report(None), move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(report(Some(1)))
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let deferred = cache.get("second".into(), report(None), || Ok(report(Some(2))));
        let snapshot = deferred.coverage.snapshot.unwrap();
        assert!(snapshot.refreshing);
        assert!(!snapshot.refresh_failed);
        release.send(()).unwrap();
        cache.wait(Some("fixture"));
    }

    #[test]
    fn cache_bounds_evict_idle_entries_and_never_evict_active_refreshes() {
        let cache = UsageCache::default();
        for index in 0..=MAX_ENTRIES {
            cache.get(index.to_string(), report(None), || Ok(report(Some(1))));
            cache.wait(None);
        }
        {
            let mut state = cache.0.0.lock().unwrap();
            let entries = &mut state.entries;
            assert_eq!(entries.len(), MAX_ENTRIES);
            assert!(!entries.contains_key("0"));
            for entry in entries.values_mut() {
                entry.refreshing = true;
            }
        }
        let full = cache.get("overflow".into(), report(None), || panic!("over capacity"));
        assert_eq!(full.totals.total_tokens, None);
        assert!(full.coverage.snapshot.unwrap().refresh_failed);
        assert_eq!(cache.0.0.lock().unwrap().entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn panic_completes_wait_and_exposes_no_loader_error() {
        let cache = UsageCache::default();
        cache.get("key".into(), report(None), || panic!("fixture panic"));
        cache.wait(None);
        let failed = cache.get("key".into(), report(None), || panic!());
        let json = serde_json::to_string(&failed).unwrap();
        assert!(!json.contains("fixture panic"));
        assert_eq!(
            failed.coverage.snapshot,
            Some(UsageSnapshot {
                updated_at_ms: None,
                refreshing: false,
                refresh_failed: true,
            })
        );
    }
    #[test]
    fn oversized_refresh_preserves_last_good_snapshot_and_byte_budget() {
        let cache = UsageCache::default();
        cache.get("key".into(), report(None), || Ok(report(Some(25))));
        cache.wait(None);
        expire(&cache, "key");
        cache.get("key".into(), report(None), || {
            let mut too_large = report(Some(99));
            too_large.display_name = "x".repeat(MAX_ENTRY_BYTES);
            Ok(too_large)
        });
        cache.wait(None);
        let kept = cache.get("key".into(), report(None), || panic!("failure cooldown"));
        assert_eq!(kept.totals.total_tokens, Some(25));
        assert!(kept.coverage.snapshot.unwrap().refresh_failed);
        let state = cache.0.0.lock().unwrap();
        assert!(state.entries.values().map(|e| e.bytes).sum::<usize>() <= MAX_CACHE_BYTES);
    }

    #[test]
    fn pending_reads_are_bounded_and_all_admitted_reads_finish() {
        let cache = UsageCache::default();
        let (release, blocked) = mpsc::channel();
        let (started_tx, started) = mpsc::channel();
        cache.get("active".into(), report(None), move || {
            started_tx.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(report(Some(1)))
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        for i in 0..MAX_PENDING {
            let row = cache.get(format!("queued-{i}"), report(None), || Ok(report(Some(2))));
            assert!(row.coverage.snapshot.unwrap().refreshing);
        }
        let refused = cache.get("overflow".into(), report(None), || panic!("queue overflow"));
        assert!(refused.coverage.snapshot.unwrap().refresh_failed);
        assert_eq!(cache.0.0.lock().unwrap().pending.len(), MAX_PENDING);
        release.send(()).unwrap();
        cache.wait(None);
        for i in 0..MAX_PENDING {
            assert_eq!(
                cache
                    .get(format!("queued-{i}"), report(None), || panic!("duplicate"))
                    .totals
                    .total_tokens,
                Some(2)
            );
        }
    }
}
