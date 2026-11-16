//! Bounded metric retention on PostgreSQL's independent maintenance lane.

use crate::{Connection, Error, OptionalExtension, Result, params};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, format_description::FormatItem, macros::format_description};

const MINUTE: &[FormatItem<'static>] = format_description!("[year]-[month]-[day]T[hour]:[minute]Z");
const DAY: &[FormatItem<'static>] = format_description!("[year][month][day]");
const MAX_BYTES: i64 = 256 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct PartitionReport {
    pub created: u32,
    pub skipped_nonempty_default: u32,
    pub complete: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct RetentionReport {
    /// Exact rows removed by the bounded DELETE. Whole-partition removals are
    /// reported separately; no unbounded COUNT is performed for a drop.
    pub rows_deleted: u64,
    pub partitions_dropped: u32,
    pub bytes_selected: u64,
    pub complete: bool,
}

pub fn ensure_metric_partitions(
    connection: &Connection,
    now: OffsetDateTime,
) -> Result<PartitionReport> {
    connection
        .execute_batch("SET LOCAL lock_timeout='25ms'; SET LOCAL statement_timeout='100ms'")?;
    connection.query_row(
        "SELECT pg_advisory_xact_lock(29162844963183702)",
        [],
        |_| Ok(()),
    )?;
    let mut report = PartitionReport::default();
    // Provision ahead of traffic, preserving any historical rows already in
    // the fallback partition. Migration provisions source days before COPY.
    for days in -30..=7 {
        ensure_metric_day(connection, now + Duration::days(days), &mut report)?;
        if report.created > 0 {
            return Ok(report);
        }
    }
    report.complete = true;
    Ok(report)
}
pub(crate) fn ensure_metric_day(
    connection: &Connection,
    day: OffsetDateTime,
    report: &mut PartitionReport,
) -> Result<()> {
    let start = day
        .date()
        .with_hms(0, 0, 0)
        .map_err(|e| Error::Migration(e.to_string()))?
        .assume_utc();
    let end = start + Duration::days(1);
    let start_text = start
        .format(MINUTE)
        .map_err(|e| Error::Migration(e.to_string()))?;
    let end_text = end
        .format(MINUTE)
        .map_err(|e| Error::Migration(e.to_string()))?;
    let table = format!(
        "metric_minutes_d{}",
        start
            .format(DAY)
            .map_err(|e| Error::Migration(e.to_string()))?
    );
    if connection
        .query_row(
            "SELECT 1 FROM dc2_metric_partitions WHERE table_name=$1",
            [&table],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        return Ok(());
    }
    let fallback:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM ONLY metric_minutes_default WHERE minute_utc>=$1 AND minute_utc<$2)",params![start_text,end_text],|r|r.get(0))?;
    if fallback {
        report.skipped_nonempty_default += 1;
        return Ok(());
    }
    // Identifiers and bounds are generated solely from a typed UTC date.
    // Build away from the partitioned parent, then attach with PostgreSQL's
    // weaker parent lock. The fallback-partition lock/validation is bounded by
    // the short maintenance DDL deadline set by the caller above.
    connection.execute_batch(&format!("CREATE TABLE {table} (LIKE metric_minutes INCLUDING DEFAULTS INCLUDING CONSTRAINTS INCLUDING GENERATED INCLUDING INDEXES); ALTER TABLE metric_minutes ATTACH PARTITION {table} FOR VALUES FROM ('{start_text}') TO ('{end_text}')"))?;
    connection.execute(
        "INSERT INTO dc2_metric_partitions(table_name,range_start,range_end) VALUES($1,$2,$3)",
        params![table, start_text, end_text],
    )?;
    report.created += 1;
    Ok(())
}

/// One bounded transaction. The scheduler persists progress by committing this
/// receipt, then resubmits only when `complete` is false. The caller supplies
/// the maintenance-lane transaction; request pools are never consumed here.
pub fn retain_metrics(
    connection: &Connection,
    now: OffsetDateTime,
    batch_rows: u32,
) -> Result<RetentionReport> {
    let limit = i64::from(batch_rows.clamp(1, 256));
    let cutoff = (now - Duration::days(30))
        .format(MINUTE)
        .map_err(|e| Error::Migration(e.to_string()))?;
    let high = (now - Duration::days(7))
        .format(MINUTE)
        .map_err(|e| Error::Migration(e.to_string()))?;
    let mut report = RetentionReport::default();
    connection.query_row(
        "SELECT pg_advisory_xact_lock(29162844963183702)",
        [],
        |_| Ok(()),
    )?;
    let expired=connection.prepare("SELECT table_name FROM dc2_metric_partitions WHERE range_end<=$1 ORDER BY range_end LIMIT 1")?.query_map([&cutoff],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>>>()?;
    let settings: (String, String) = connection.query_row(
        "SELECT current_setting('statement_timeout'),current_setting('lock_timeout')",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    for table in expired {
        if !valid_partition_name(&table) {
            return Err(Error::Migration(
                "invalid owned metric partition metadata".into(),
            ));
        }
        connection
            .execute_batch("SET LOCAL lock_timeout='25ms'; SET LOCAL statement_timeout='100ms'")?;
        connection.execute_batch(&format!("DROP TABLE {table}"))?;
        connection.execute(
            "DELETE FROM dc2_metric_partitions WHERE table_name=$1",
            [table],
        )?;
        report.partitions_dropped += 1;
    }
    connection.query_row(
        "SELECT set_config('statement_timeout',$1,true),set_config('lock_timeout',$2,true)",
        params![settings.0, settings.1],
        |_| Ok(()),
    )?;
    let (rows,bytes):(i64,i64)=connection.query_row(
        "WITH high AS MATERIALIZED (
           SELECT tableoid AS relation,ctid AS tuple,minute_utc,rowid,
             (octet_length(subject_kind)+octet_length(subject_id)+octet_length(metric)+octet_length(minute_utc)+32)::bigint AS bytes
           FROM metric_minutes
           WHERE minute_utc<$2 AND subject_kind IN ('component','container','test')
           ORDER BY minute_utc,rowid LIMIT $3
         ), low AS MATERIALIZED (
           SELECT tableoid AS relation,ctid AS tuple,minute_utc,rowid,
             (octet_length(subject_kind)+octet_length(subject_id)+octet_length(metric)+octet_length(minute_utc)+32)::bigint AS bytes
           FROM metric_minutes
           WHERE minute_utc<$1 AND subject_kind NOT IN ('component','container','test')
           ORDER BY minute_utc,rowid LIMIT $3
         ), candidates AS MATERIALIZED (
           SELECT * FROM (SELECT * FROM high UNION ALL SELECT * FROM low) both_kinds
           ORDER BY minute_utc,rowid LIMIT $3
         ), budget AS MATERIALIZED (
           SELECT relation,tuple,bytes,row_number() OVER(ORDER BY minute_utc,rowid) AS ordinal,
             sum(bytes) OVER(ORDER BY minute_utc,rowid ROWS UNBOUNDED PRECEDING) AS used
           FROM candidates
         ), selected AS MATERIALIZED (
           SELECT relation,tuple,bytes FROM budget WHERE used<=$4 OR ordinal=1
         ), removed AS (
           DELETE FROM metric_minutes m USING selected s WHERE m.tableoid=s.relation AND m.ctid=s.tuple RETURNING m.tableoid
         ) SELECT (SELECT count(*) FROM removed),(SELECT coalesce(sum(bytes),0)::bigint FROM selected)",
        params![cutoff,high,limit,MAX_BYTES],|r|Ok((r.get(0)?,r.get(1)?)))?;
    report.rows_deleted = u64::try_from(rows).map_err(|_| Error::InvalidQuery)?;
    report.bytes_selected = u64::try_from(bytes).map_err(|_| Error::InvalidQuery)?;
    let remains:bool=connection.query_row("WITH high AS MATERIALIZED(SELECT 1 FROM metric_minutes WHERE minute_utc<$2 AND subject_kind IN ('component','container','test') ORDER BY minute_utc,rowid LIMIT 1), low AS MATERIALIZED(SELECT 1 FROM metric_minutes WHERE minute_utc<$1 AND subject_kind NOT IN ('component','container','test') ORDER BY minute_utc,rowid LIMIT 1) SELECT EXISTS(SELECT 1 FROM high) OR EXISTS(SELECT 1 FROM low) OR EXISTS(SELECT 1 FROM dc2_metric_partitions WHERE range_end<=$1)",params![cutoff,high],|r|r.get(0))?;
    report.complete = !remains;
    Ok(report)
}
fn valid_partition_name(name: &str) -> bool {
    name.strip_prefix("metric_minutes_d")
        .is_some_and(|suffix| suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_digit()))
}
