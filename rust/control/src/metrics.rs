//! Schema-15 minute aggregates and bounded trend queries.

use std::collections::BTreeMap;
use std::sync::Arc;

use devcoordinator2_api::results::MetricPoint;
use devcoordinator2_api::{ErrorCode, ProtocolError};
use time::{Duration, format_description::FormatItem, macros::format_description};

use crate::database::{Database, DatabaseError};
use crate::platform::{Clock, HostClock};

pub const RETENTION_DAYS: i64 = 30;
/// High-cardinality runtime subjects are disposable observations. Keep their
/// minute-level history for one dashboard week while preserving the full
/// 30-day window for host and repository trend series.
pub const HIGH_CARDINALITY_RETENTION_DAYS: i64 = 7;
const MINUTE_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]Z");

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aggregate {
    pub min: f64,
    pub sum: f64,
    pub max: f64,
    pub samples: u32,
}

#[derive(Clone)]
pub struct MetricsStore {
    database: Database,
    clock: Arc<dyn Clock>,
}

impl MetricsStore {
    pub fn new(database: Database) -> Self {
        Self::with_clock(database, Arc::new(HostClock))
    }

    pub fn with_clock(database: Database, clock: Arc<dyn Clock>) -> Self {
        Self { database, clock }
    }

    pub fn minute(&self) -> Result<String, ProtocolError> {
        self.clock.now_utc().format(MINUTE_FORMAT).map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "cannot format metric minute")
                .with_detail(error.to_string())
        })
    }

    pub fn flush(
        &self,
        minute: &str,
        aggregates: &BTreeMap<(String, String, String), Aggregate>,
    ) -> Result<(), ProtocolError> {
        let minute = minute.to_owned();
        let rows = aggregates
            .iter()
            .filter(|(_, aggregate)| aggregate.samples > 0)
            .map(|((kind, id, metric), aggregate)| {
                (
                    kind.clone(),
                    id.clone(),
                    metric.clone(),
                    aggregate.min,
                    aggregate.sum / f64::from(aggregate.samples),
                    aggregate.max,
                    aggregate.samples,
                )
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return Ok(());
        }
        self.database
            .transaction(move |transaction| {
                for (kind, id, metric, min, average, max, samples) in rows {
                    transaction.execute(
                        "INSERT OR REPLACE INTO metric_minutes(subject_kind,subject_id,metric,minute_utc,min_value,avg_value,max_value,samples) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                        rusqlite::params![kind,id,metric,minute,min,average,max,samples],
                    )?;
                }
                Ok(())
            })
            .map_err(database_error)
    }

    pub fn expire(&self) -> Result<u64, ProtocolError> {
        let now = self.clock.now_utc();
        let cutoff = (now - Duration::days(RETENTION_DAYS))
            .format(MINUTE_FORMAT)
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InternalError, "cannot format metric cutoff")
                    .with_detail(error.to_string())
            })?;
        let high_cardinality_cutoff = (now - Duration::days(HIGH_CARDINALITY_RETENTION_DAYS))
        .format(MINUTE_FORMAT)
        .map_err(|error| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "cannot format high-cardinality metric cutoff",
            )
            .with_detail(error.to_string())
        })?;
        self.database
            .transaction(move |transaction| {
                Ok(u64::try_from(
                    transaction
                        .execute(
                            "DELETE FROM metric_minutes WHERE minute_utc < ?1 OR (subject_kind IN ('component','container','test') AND minute_utc < ?2)",
                            rusqlite::params![cutoff, high_cardinality_cutoff],
                        )?,
                )
                .unwrap_or(u64::MAX))
            })
            .map_err(database_error)
    }

    pub fn series(
        &self,
        subject_kind: &str,
        subject_id: &str,
        metric: &str,
        minutes: u32,
    ) -> Result<Vec<MetricPoint>, ProtocolError> {
        let minutes = minutes.clamp(1, 60 * 24 * RETENTION_DAYS as u32);
        let since = (self.clock.now_utc() - Duration::minutes(i64::from(minutes)))
            .format(MINUTE_FORMAT)
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InternalError, "cannot format metric range")
                    .with_detail(error.to_string())
            })?;
        let kind = subject_kind.to_owned();
        let id = subject_id.to_owned();
        let metric = metric.to_owned();
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT minute_utc,min_value,avg_value,max_value,samples FROM metric_minutes WHERE subject_kind=?1 AND subject_id=?2 AND metric=?3 AND minute_utc>=?4 ORDER BY minute_utc",
                )?;
                Ok(statement
                    .query_map(rusqlite::params![kind,id,metric,since], |row| {
                        Ok(MetricPoint {
                            minute: row.get(0)?,
                            min: row.get(1)?,
                            avg: row.get(2)?,
                            max: row.get(3)?,
                            samples: row.get(4)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)
    }

    pub fn trend(
        &self,
        subject_kind: &str,
        subject_id: &str,
        metric: &str,
        minutes: u32,
        points: usize,
    ) -> Result<Vec<f64>, ProtocolError> {
        let data = self.series(subject_kind, subject_id, metric, minutes)?;
        if data.is_empty() || points == 0 {
            return Ok(Vec::new());
        }
        let bucket = (data.len() / points).max(1);
        let mut result = data
            .chunks(bucket)
            .map(|chunk| {
                round3(chunk.iter().map(|point| point.avg).sum::<f64>() / chunk.len() as f64)
            })
            .collect::<Vec<_>>();
        if result.len() > points {
            result.drain(..result.len() - points);
        }
        Ok(result)
    }

    pub fn table_size(&self) -> Result<u64, ProtocolError> {
        self.database
            .call(|connection| {
                connection
                    .query_row("SELECT count(*) FROM metric_minutes", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .map(|value| u64::try_from(value).unwrap_or(0))
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)
    }
}

pub fn downsample(points: &[MetricPoint], target: usize) -> Vec<MetricPoint> {
    if points.len() <= target || target < 2 {
        return points.to_vec();
    }
    let bucket = points.len().div_ceil(target);
    points
        .chunks(bucket)
        .map(|chunk| {
            let samples = chunk.iter().map(|point| point.samples).sum::<u32>();
            let denominator = if samples == 0 {
                u32::try_from(chunk.len()).unwrap_or(u32::MAX)
            } else {
                samples
            };
            let weighted = chunk
                .iter()
                .map(|point| point.avg * f64::from(point.samples.max(1)))
                .sum::<f64>();
            MetricPoint {
                minute: chunk.last().expect("nonempty chunk").minute.clone(),
                min: chunk
                    .iter()
                    .map(|point| point.min)
                    .fold(f64::INFINITY, f64::min),
                avg: round3(weighted / f64::from(denominator)),
                max: chunk
                    .iter()
                    .map(|point| point.max)
                    .fold(f64::NEG_INFINITY, f64::max),
                samples: denominator,
            }
        })
        .collect()
}

fn round3(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "metric storage failed")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::FixedClock;
    use tempfile::tempdir;
    use time::macros::datetime;

    #[test]
    fn aggregates_series_downsampling_trends_and_expiry_are_exact() {
        let temporary = tempdir().unwrap();
        let database_path = temporary.path().join("authority.sqlite3");
        let database = Database::open(&database_path).unwrap();
        let clock = Arc::new(FixedClock(datetime!(2026-09-04 12:05 UTC)));
        let store = MetricsStore::with_clock(database, clock);
        store
            .flush(
                "2026-09-04T12:04Z",
                &BTreeMap::from([(
                    ("repository".into(), "r1".into(), "cpu_percent".into()),
                    Aggregate {
                        min: 1.0,
                        sum: 6.0,
                        max: 5.0,
                        samples: 2,
                    },
                )]),
            )
            .unwrap();
        let series = store.series("repository", "r1", "cpu_percent", 60).unwrap();
        assert_eq!(series[0].avg, 3.0);
        assert_eq!(
            store
                .trend("repository", "r1", "cpu_percent", 60, 12)
                .unwrap(),
            [3.0]
        );
        assert_eq!(store.table_size().unwrap(), 1);
        assert_eq!(store.expire().unwrap(), 0);
        store
            .flush(
                "2026-08-27T12:00Z",
                &BTreeMap::from([
                    (
                        ("component".into(), "c-old".into(), "cpu_percent".into()),
                        Aggregate {
                            min: 1.0,
                            sum: 1.0,
                            max: 1.0,
                            samples: 1,
                        },
                    ),
                    (
                        (
                            "repository".into(),
                            "r-retained".into(),
                            "cpu_percent".into(),
                        ),
                        Aggregate {
                            min: 1.0,
                            sum: 1.0,
                            max: 1.0,
                            samples: 1,
                        },
                    ),
                ]),
            )
            .unwrap();
        store
            .flush(
                "2026-08-01T12:00Z",
                &BTreeMap::from([(
                    ("repository".into(), "r-old".into(), "cpu_percent".into()),
                    Aggregate {
                        min: 1.0,
                        sum: 1.0,
                        max: 1.0,
                        samples: 1,
                    },
                )]),
            )
            .unwrap();
        store
            .flush(
                "2026-08-30T12:00Z",
                &BTreeMap::from([(
                    (
                        "container".into(),
                        "container-retained".into(),
                        "cpu_percent".into(),
                    ),
                    Aggregate {
                        min: 1.0,
                        sum: 1.0,
                        max: 1.0,
                        samples: 1,
                    },
                )]),
            )
            .unwrap();
        assert_eq!(store.expire().unwrap(), 2);
        assert_eq!(store.table_size().unwrap(), 3);
        drop(store);
        let reopened = MetricsStore::with_clock(
            Database::open(database_path).unwrap(),
            Arc::new(FixedClock(datetime!(2026-09-04 12:05 UTC))),
        );
        assert_eq!(reopened.table_size().unwrap(), 3);
        assert_eq!(
            reopened
                .series("repository", "r1", "cpu_percent", 60)
                .unwrap()[0]
                .avg,
            3.0
        );

        let many = (0..10)
            .map(|index| MetricPoint {
                minute: format!("m{index}"),
                min: f64::from(index),
                avg: f64::from(index),
                max: f64::from(index),
                samples: 1,
            })
            .collect::<Vec<_>>();
        let reduced = downsample(&many, 3);
        assert_eq!(reduced.len(), 3);
        assert_eq!(
            (reduced[0].min, reduced[0].max, reduced[0].samples),
            (0.0, 3.0, 4)
        );
    }
}
