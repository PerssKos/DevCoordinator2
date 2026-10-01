use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use devcoordinator2_api::results::{UsageRepository, UsageSnapshot};
use devcoordinator2_api::{ErrorCode, ProtocolError};

const FRESH_FOR: Duration = Duration::from_secs(30);
const MAX_ENTRIES: usize = 128;
// A repository collection request can touch millions of token observations.
// Bound concurrent source scans so the Usage page cannot fan out one large
// SQLite read per repository and exhaust daemon memory.
const MAX_ACTIVE_REFRESHES: usize = 1;
// The edge gives ordinary daemon requests a ten-second response budget. Keep
// an explicit refresh wait below that ceiling so a slow collector returns an
// honest refreshing/partial snapshot instead of becoming daemon_unavailable.
const MAX_WAIT: Duration = Duration::from_secs(7);

type SharedEntries = (Mutex<HashMap<String, Entry>>, Condvar, AtomicUsize);
#[derive(Clone, Default)]
pub(crate) struct UsageCache(Arc<SharedEntries>);

struct Entry {
    repository_id: String,
    report: UsageRepository,
    completed: Option<Instant>,
    touched: Instant,
    refreshing: bool,
    refresh_failed: bool,
    updated_at_ms: Option<u64>,
}

impl UsageCache {
    pub(crate) fn get(
        &self,
        key: String,
        empty: UsageRepository,
        load: impl FnOnce() -> Result<UsageRepository, ProtocolError> + Send + 'static,
    ) -> UsageRepository {
        let (entries, changed, active) = &*self.0;
        let mut entries = entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !entries.contains_key(&key) && entries.len() >= MAX_ENTRIES {
            let oldest = entries
                .iter()
                .filter(|(_, entry)| !entry.refreshing)
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            } else {
                let mut empty = empty;
                empty.coverage.snapshot = Some(UsageSnapshot {
                    updated_at_ms: None,
                    refreshing: false,
                    refresh_failed: true,
                });
                return empty;
            }
        }
        let entry = entries.entry(key.clone()).or_insert_with(|| Entry {
            repository_id: empty.repository_id.clone(),
            report: empty,
            completed: None,
            touched: Instant::now(),
            refreshing: false,
            refresh_failed: false,
            updated_at_ms: None,
        });
        entry.touched = Instant::now();
        let start = !entry.refreshing && entry.completed.is_none_or(|at| at.elapsed() >= FRESH_FOR);
        let admitted = start
            && active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count < MAX_ACTIVE_REFRESHES).then_some(count + 1)
                })
                .is_ok();
        entry.refreshing |= admitted;
        if start && !admitted {
            entry.refresh_failed = true;
        }
        let mut report = entry.report.clone();
        report.coverage.snapshot = Some(UsageSnapshot {
            updated_at_ms: entry.updated_at_ms,
            refreshing: entry.refreshing,
            refresh_failed: entry.refresh_failed,
        });
        drop(entries);
        if admitted {
            let cache = self.clone();
            let worker_key = key.clone();
            let spawned = std::thread::Builder::new()
                .name("usage-refresh".into())
                .spawn(move || {
                    let result = catch_unwind(AssertUnwindSafe(load)).unwrap_or_else(|_| {
                        Err(ProtocolError::new(
                            ErrorCode::InternalError,
                            "usage refresh failed",
                        ))
                    });
                    cache.finish(&worker_key, result);
                });
            if spawned.is_err() {
                self.finish(
                    &key,
                    Err(ProtocolError::new(
                        ErrorCode::InternalError,
                        "usage refresh unavailable",
                    )),
                );
                report
                    .coverage
                    .snapshot
                    .as_mut()
                    .expect("snapshot set")
                    .refreshing = false;
                report
                    .coverage
                    .snapshot
                    .as_mut()
                    .expect("snapshot set")
                    .refresh_failed = true;
            }
            changed.notify_all();
        }
        report
    }

    fn finish(&self, key: &str, result: Result<UsageRepository, ProtocolError>) {
        let (entries, changed, active) = &*self.0;
        active.fetch_sub(1, Ordering::AcqRel);
        let mut entries = entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = entries.get_mut(key) {
            let usable = result.as_ref().is_ok_and(|report| {
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
                                .all(|reason| reason == "mapping_unavailable")))
            });
            if let Ok(report) = result {
                if usable {
                    entry.updated_at_ms = Some(report.generated_at_ms);
                    entry.report = report;
                } else if entry.updated_at_ms.is_none() {
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
        let (entries, changed, _) = &*self.0;
        let entries = entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _waited = changed
            .wait_timeout_while(entries, MAX_WAIT, |entries| {
                entries.values().any(|entry| {
                    entry.refreshing && repository_id.is_none_or(|id| entry.repository_id == id)
                })
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
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
        cache.0.0.lock().unwrap().get_mut(key).unwrap().completed =
            Some(Instant::now() - FRESH_FOR);
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
        assert!(UsageCache::default().0.0.lock().unwrap().is_empty());
    }

    #[test]
    fn refresh_admission_defers_second_source_scan_without_claiming_refreshing() {
        let cache = UsageCache::default();
        let (release, release_rx) = mpsc::channel();
        let (started_tx, started) = mpsc::channel();
        cache.get("first".into(), report(None), move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(report(Some(1)))
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let deferred = cache.get("second".into(), report(None), || panic!("admission capped"));
        let snapshot = deferred.coverage.snapshot.unwrap();
        assert!(!snapshot.refreshing);
        assert!(snapshot.refresh_failed);
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
            let mut entries = cache.0.0.lock().unwrap();
            assert_eq!(entries.len(), MAX_ENTRIES);
            assert!(!entries.contains_key("0"));
            for entry in entries.values_mut() {
                entry.refreshing = true;
            }
        }
        let full = cache.get("overflow".into(), report(None), || panic!("over capacity"));
        assert_eq!(full.totals.total_tokens, None);
        assert!(full.coverage.snapshot.unwrap().refresh_failed);
        assert_eq!(cache.0.0.lock().unwrap().len(), MAX_ENTRIES);
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
}
