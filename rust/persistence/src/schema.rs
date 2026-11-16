//! Ordered, hash-bound PostgreSQL migrations, shared by daemon and cutover tool.

use crate::{Error, Persistence, Result};
use sha2::{Digest, Sha256};

pub const LEGACY_SCHEMA_VERSION: u32 = 34;
pub const DATABASE_SCHEMA_VERSION: u32 = 35;
pub const MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "authority",
        include_str!("../migrations/0001_authority.sql"),
    ),
    (
        2,
        "invariants_search",
        include_str!("../migrations/0002_invariants_search.sql"),
    ),
    (
        3,
        "defaults",
        include_str!("../migrations/0003_defaults.sql"),
    ),
    (
        4,
        "maintenance",
        include_str!("../migrations/0004_maintenance.sql"),
    ),
    (
        5,
        "review_reminder_projection",
        include_str!("../migrations/0005_review_reminder_projection.sql"),
    ),
    (
        6,
        "log_retention_projection",
        include_str!("../migrations/0006_log_retention_projection.sql"),
    ),
    (
        7,
        "evidence_protection_projections",
        include_str!("../migrations/0007_evidence_protection_projections.sql"),
    ),
    (
        8,
        "storage_query_projections",
        include_str!("../migrations/0008_storage_query_projections.sql"),
    ),
    (
        9,
        "retention_publication",
        include_str!("../migrations/0009_retention_publication.sql"),
    ),
    (
        10,
        "storage_repository_owner",
        include_str!("../migrations/0010_storage_repository_owner.sql"),
    ),
    (
        11,
        "protection_metadata",
        include_str!("../migrations/0011_protection_metadata.sql"),
    ),
    (
        12,
        "compact_json_bounds",
        include_str!("../migrations/0012_compact_json_bounds.sql"),
    ),
    (
        13,
        "compact_json_restore_path",
        include_str!("../migrations/0013_compact_json_restore_path.sql"),
    ),
    (
        14,
        "storage_gin_pending",
        include_str!("../migrations/0014_storage_gin_pending.sql"),
    ),
    (
        15,
        "generation_source_origins",
        include_str!("../migrations/0015_generation_source_origins.sql"),
    ),
    (
        16,
        "usage_operation_contexts",
        include_str!("../migrations/0016_usage_operation_contexts.sql"),
    ),
];

pub fn migrate(database: &Persistence) -> Result<()> {
    database.maintenance_transaction(|tx| {
        tx.query_row("SELECT pg_advisory_xact_lock(29162844963183701)",[],|_|Ok(()))?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS dc2_schema_migrations(version BIGINT PRIMARY KEY,name TEXT NOT NULL,sha256 TEXT NOT NULL,applied_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp())")?;
        let highest:Option<i64>=tx.query_row("SELECT max(version) FROM dc2_schema_migrations",[],|r|r.get(0))?;
        if highest.is_some_and(|v|v>MIGRATIONS.last().unwrap().0) {return Err(Error::Migration("target schema is newer than this executable".into()));}
        for (version,name,sql) in MIGRATIONS {
            let digest=crate::hex(&Sha256::digest(sql.as_bytes()));
            let recorded=tx.query_row("SELECT sha256 FROM dc2_schema_migrations WHERE version=$1",[*version],|r|r.get::<_,String>(0));
            match recorded {
                Ok(recorded) if recorded==digest=>continue,
                Ok(_)=>return Err(Error::Migration(format!("migration {version} differs from its applied source"))),
                Err(Error::QueryReturnedNoRows)=>{},
                Err(e)=>return Err(e),
            }
            tx.execute_batch(sql)?;
            tx.execute("INSERT INTO dc2_schema_migrations(version,name,sha256) VALUES($1,$2,$3)",crate::params![version,name,digest])?;
        }
        Ok(())
    })
}
