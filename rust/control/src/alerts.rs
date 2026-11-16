//! Sustained health alerts backed by current alert rows.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use devcoordinator2_api::results::Alert;
use devcoordinator2_api::{ErrorCode, ProtocolError};
use time::{format_description::FormatItem, macros::format_description};

use crate::database::{Database, DatabaseError};
use crate::platform::{Clock, HostClock, HostMonotonicClock, MonotonicClock};

const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

#[derive(Clone, Debug)]
pub struct Condition {
    pub key: String,
    pub kind: String,
    pub subject_kind: String,
    pub subject_id: String,
    pub severity: String,
    pub message: String,
    pub active: bool,
    pub sustain_seconds: f64,
}

#[derive(Clone, Debug)]
pub struct AlertEvent {
    pub kind: &'static str,
    pub alert_key: String,
    pub alert_kind: String,
    pub subject_kind: String,
    pub subject_id: String,
    pub severity: Option<String>,
    pub message: String,
}

pub trait AlertEventSink: Send + Sync + 'static {
    fn publish(&self, event: AlertEvent);
}

impl<F> AlertEventSink for F
where
    F: Fn(AlertEvent) + Send + Sync + 'static,
{
    fn publish(&self, event: AlertEvent) {
        self(event);
    }
}

#[derive(Clone)]
pub struct AlertEngine {
    inner: Arc<Inner>,
}

struct Inner {
    database: Database,
    clock: Arc<dyn Clock>,
    monotonic: Arc<dyn MonotonicClock>,
    state: Mutex<State>,
    events: Mutex<Option<Arc<dyn AlertEventSink>>>,
}

#[derive(Default)]
struct State {
    since: HashMap<String, f64>,
    open: HashSet<String>,
}

impl AlertEngine {
    pub fn new(database: Database) -> Self {
        Self::with_clocks(
            database,
            Arc::new(HostClock),
            Arc::new(HostMonotonicClock::default()),
        )
    }

    pub fn with_clocks(
        database: Database,
        clock: Arc<dyn Clock>,
        monotonic: Arc<dyn MonotonicClock>,
    ) -> Self {
        let open = database
            .read(|connection| {
                let mut statement = connection.prepare("SELECT alert_key FROM alerts")?;
                Ok(statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<HashSet<_>, _>>()?)
            })
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                database,
                clock,
                monotonic,
                state: Mutex::new(State {
                    since: HashMap::new(),
                    open,
                }),
                events: Mutex::new(None),
            }),
        }
    }

    pub fn set_event_sink(&self, sink: Arc<dyn AlertEventSink>) {
        *self
            .inner
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    pub fn evaluate(&self, conditions: &[Condition]) -> Result<(), ProtocolError> {
        let now_mono = self.inner.monotonic.seconds();
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut seen = HashSet::new();
        for condition in conditions {
            validate_condition(condition)?;
            seen.insert(condition.key.clone());
            if condition.active {
                let since = *state.since.entry(condition.key.clone()).or_insert(now_mono);
                if !state.open.contains(&condition.key)
                    && now_mono - since >= condition.sustain_seconds
                {
                    self.open(condition)?;
                    state.open.insert(condition.key.clone());
                } else if state.open.contains(&condition.key) {
                    self.refresh(condition)?;
                }
            } else {
                state.since.remove(&condition.key);
                if state.open.remove(&condition.key) {
                    self.resolve(&condition.key, Some(condition))?;
                }
            }
        }
        let vanished = state
            .open
            .iter()
            .filter(|key| !seen.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in vanished {
            state.open.remove(&key);
            state.since.remove(&key);
            self.resolve(&key, None)?;
        }
        Ok(())
    }

    pub fn current(&self) -> Result<Vec<Alert>, ProtocolError> {
        self.inner
            .database
            .read(|connection| {
                let mut statement = connection.prepare(
                    "SELECT alert_key,kind,subject_kind,subject_id,severity,message,opened_at,last_seen_at FROM alerts ORDER BY severity,opened_at",
                )?;
                Ok(statement
                    .query_map([], |row| {
                        Ok(Alert {
                            alert_key: row.get(0)?,
                            kind: row.get(1)?,
                            subject_kind: row.get(2)?,
                            subject_id: row.get(3)?,
                            severity: row.get(4)?,
                            message: row.get(5)?,
                            opened_at: row.get(6)?,
                            last_seen_at: row.get(7)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)
    }

    fn open(&self, condition: &Condition) -> Result<(), ProtocolError> {
        let now = self.timestamp()?;
        let event = AlertEvent {
            kind: "alert.opened",
            alert_key: condition.key.clone(),
            alert_kind: condition.kind.clone(),
            subject_kind: condition.subject_kind.clone(),
            subject_id: condition.subject_id.clone(),
            severity: Some(condition.severity.clone()),
            message: condition.message.clone(),
        };
        let condition = condition.clone();
        self.inner
            .database
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO alerts(alert_key,kind,subject_kind,subject_id,severity,message,opened_at,last_seen_at) VALUES($1,$2,$3,$4,$5,$6,$7,$7) ON CONFLICT(alert_key) DO UPDATE SET kind=excluded.kind,subject_kind=excluded.subject_kind,subject_id=excluded.subject_id,severity=excluded.severity,message=excluded.message,opened_at=excluded.opened_at,last_seen_at=excluded.last_seen_at",
                    crate::database::params![condition.key,condition.kind,condition.subject_kind,condition.subject_id,condition.severity,condition.message,now],
                )?;
                Ok(())
            })
            .map_err(database_error)?;
        self.publish(event);
        Ok(())
    }

    fn refresh(&self, condition: &Condition) -> Result<(), ProtocolError> {
        let now = self.timestamp()?;
        let key = condition.key.clone();
        let message = condition.message.clone();
        self.inner
            .database
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE alerts SET last_seen_at=$1,message=$2 WHERE alert_key=$3",
                    crate::database::params![now, message, key],
                )?;
                Ok(())
            })
            .map_err(database_error)
    }

    fn resolve(&self, key: &str, fallback: Option<&Condition>) -> Result<(), ProtocolError> {
        let key_owned = key.to_owned();
        let row = self
            .inner
            .database
            .read(move |connection| {
                connection
                    .query_row(
                        "SELECT kind,subject_kind,subject_id,severity,message FROM alerts WHERE alert_key=$1",
                        [&key_owned],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        let key_owned = key.to_owned();
        self.inner
            .database
            .transaction(move |transaction| {
                transaction.execute("DELETE FROM alerts WHERE alert_key=$1", [&key_owned])?;
                Ok(())
            })
            .map_err(database_error)?;
        let (kind, subject_kind, subject_id, severity, message) = row.unwrap_or_else(|| {
            fallback.map_or_else(
                || {
                    (
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                    )
                },
                |condition| {
                    (
                        condition.kind.clone(),
                        condition.subject_kind.clone(),
                        condition.subject_id.clone(),
                        condition.severity.clone(),
                        condition.message.clone(),
                    )
                },
            )
        });
        self.publish(AlertEvent {
            kind: "alert.recovered",
            alert_key: key.into(),
            alert_kind: kind,
            subject_kind,
            subject_id,
            severity: Some(severity),
            message: format!("recovered: {message}"),
        });
        Ok(())
    }

    fn timestamp(&self) -> Result<String, ProtocolError> {
        self.inner
            .clock
            .now_utc()
            .format(TIMESTAMP_FORMAT)
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InternalError, "cannot format alert timestamp")
                    .with_detail(error.to_string())
            })
    }

    fn publish(&self, event: AlertEvent) {
        if let Some(sink) = self
            .inner
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            sink.publish(event);
        }
    }
}

fn validate_condition(condition: &Condition) -> Result<(), ProtocolError> {
    if condition.key.is_empty()
        || condition.key.len() > 256
        || condition.kind.is_empty()
        || condition.subject_kind.is_empty()
        || condition.subject_id.is_empty()
        || !matches!(condition.severity.as_str(), "warning" | "critical")
        || condition.message.is_empty()
        || condition.message.len() > 512
        || !condition.sustain_seconds.is_finite()
        || condition.sustain_seconds < 0.0
    {
        return Err(ProtocolError::new(
            ErrorCode::InternalError,
            "health alert condition is invalid",
        ));
    }
    Ok(())
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "health alert storage failed")
            .with_detail(other.to_string()),
    }
}

use crate::database::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::FixedClock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::tempdir;
    use time::macros::datetime;

    struct StepClock(AtomicU64);
    impl MonotonicClock for StepClock {
        fn seconds(&self) -> f64 {
            self.0.load(Ordering::SeqCst) as f64
        }
    }

    #[test]
    fn sustained_alerts_open_refresh_resolve_and_publish() {
        let temporary = tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let monotonic = Arc::new(StepClock(AtomicU64::new(0)));
        let engine = AlertEngine::with_clocks(
            database,
            Arc::new(FixedClock(datetime!(2026-09-04 00:00 UTC))),
            monotonic.clone(),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        engine.set_event_sink(Arc::new(move |event| captured.lock().unwrap().push(event)));
        let mut condition = Condition {
            key: "host/disk".into(),
            kind: "host_disk".into(),
            subject_kind: "host".into(),
            subject_id: "host".into(),
            severity: "critical".into(),
            message: "disk low".into(),
            active: true,
            sustain_seconds: 10.0,
        };
        engine.evaluate(&[condition.clone()]).unwrap();
        assert!(engine.current().unwrap().is_empty());
        monotonic.0.store(10, Ordering::SeqCst);
        engine.evaluate(&[condition.clone()]).unwrap();
        assert_eq!(engine.current().unwrap().len(), 1);
        condition.active = false;
        engine.evaluate(&[condition]).unwrap();
        assert!(engine.current().unwrap().is_empty());
        assert_eq!(events.lock().unwrap().len(), 2);
    }
}
