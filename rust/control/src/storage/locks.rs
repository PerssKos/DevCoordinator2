use super::{
    StorageService, db_error, fs,
    model::{Locator, Record},
};
use crate::database::{Database, DatabaseError};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use std::collections::BTreeMap;

pub(super) struct ResourceGuard {
    database: Database,
    job: String,
}
impl Drop for ResourceGuard {
    fn drop(&mut self) {
        let job = self.job.clone();
        let _ = self.database.call(move |c| {
            c.execute("DELETE FROM storage_resource_locks WHERE job_id=?1", [job])?;
            Ok(())
        });
    }
}

impl StorageService {
    pub(super) fn lock_resources(
        &self,
        job: &str,
        records: &[Record],
    ) -> Result<ResourceGuard, ProtocolError> {
        let mut claims = BTreeMap::<String, bool>::new();
        for record in records {
            claims.insert(record.resource_key.clone(), true);
            if let Some(lineage) = &record.recovery_lineage {
                claims.insert(format!("backup-lineage:{lineage}"), true);
            }
            if matches!(record.locator, Locator::Mount { .. }) {
                claims.insert("host-mount-configuration".into(), true);
            }
            // Shared ancestor claims allow sibling cleanup while preventing a
            // parent tree and one of its descendants from being removed together
            // by independent jobs. Inode keys also coordinate bind aliases.
            for alias in &record.private_aliases {
                let mut path = alias.parent();
                while let Some(parent) = path {
                    if let Ok((device, inode)) = fs::identity(parent) {
                        claims
                            .entry(format!("fs:{device}:{inode}"))
                            .or_insert(false);
                    }
                    path = parent.parent();
                }
            }
        }
        let id = job.to_owned();
        self.database.transaction(move|c|{
            for (key,exclusive) in &claims {
                let conflict=c.query_row("SELECT EXISTS(SELECT 1 FROM storage_resource_locks WHERE resource_key=?1 AND job_id<>?2 AND (exclusive=1 OR ?3=1))",rusqlite::params![key,id,*exclusive],|r|r.get::<_,bool>(0))?;
                if conflict{return Err(DatabaseError::Domain(ProtocolError::new(ErrorCode::Busy,"artifact_cleanup_in_progress")));}
            }
            for (key,exclusive) in claims {c.execute("INSERT INTO storage_resource_locks VALUES(?1,?2,?3) ON CONFLICT(resource_key,job_id) DO UPDATE SET exclusive=excluded.exclusive",rusqlite::params![key,id,exclusive])?;}
            Ok(())
        }).map_err(db_error)?;
        Ok(ResourceGuard {
            database: self.database.clone(),
            job: job.into(),
        })
    }
}

pub(super) fn ensure_editable(
    c: &rusqlite::Connection,
    resource: &str,
    job: Option<&str>,
) -> Result<(), DatabaseError> {
    let busy=c.query_row("SELECT EXISTS(SELECT 1 FROM storage_resource_locks WHERE resource_key=?1 AND (?2 IS NULL OR job_id<>?2))",rusqlite::params![resource,job],|r|r.get::<_,bool>(0))?;
    if busy {
        return Err(DatabaseError::Domain(ProtocolError::new(
            ErrorCode::Busy,
            "artifact_cleanup_in_progress",
        )));
    }
    Ok(())
}
