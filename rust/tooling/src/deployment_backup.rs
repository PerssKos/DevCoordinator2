//! Bounded deployment metadata from a verified, immutable activation backup.
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

pub fn inspect(
    transaction_dir: PathBuf,
    repository_id: String,
    deployment_id: String,
) -> Result<Value, String> {
    if deployment_id.len() != 17
        || !deployment_id.starts_with('d')
        || !deployment_id.as_bytes()[1..]
            .iter()
            .all(u8::is_ascii_hexdigit)
    {
        return Err("use one valid deployment identity".into());
    }
    let request = crate::planning_backup::InspectRequest {
        transaction_dir,
        repository_id: repository_id.clone(),
        task_ids: vec![],
        include_identities: false,
    };
    let (backup, deployment) = crate::planning_backup::inspect_with(&request, |connection| {
        metadata(connection, &repository_id, &deployment_id)
    })?;
    Ok(
        json!({"backup_sha256":backup.backup_sha256,"provenance":backup.provenance,
        "database_schema":backup.database_schema,"repository_id":repository_id,
        "deployment_id":deployment_id,"deployment":deployment,"recovery_performed":false}),
    )
}

fn metadata(
    connection: &Connection,
    repository: &str,
    deployment: &str,
) -> Result<Option<Value>, String> {
    let selected = connection.query_row(
        "SELECT current_generation,previous_generation,source,public,domain,domain_override FROM deployments WHERE repository_id=?1 AND deployment_id=?2",
        [repository, deployment], |row| Ok(json!({
            "current_generation":row.get::<_,Option<i64>>(0)?,
            "previous_generation":row.get::<_,Option<i64>>(1)?,
            "source":row.get::<_,String>(2)?,"public":row.get::<_,bool>(3)?,
            "domain":row.get::<_,Option<String>>(4)?,"domain_override":row.get::<_,Option<String>>(5)?,
        }))
    ).optional().map_err(|_| "cannot inspect saved deployment metadata")?;
    let Some(mut selected) = selected else {
        return Ok(None);
    };
    let mut components = connection.prepare("SELECT name,type,generation,binding_kind,binding_identity FROM components WHERE deployment_id=?1 ORDER BY name LIMIT 65")
        .map_err(|_| "cannot inspect saved component metadata")?;
    let rows = components.query_map([deployment], |row| Ok(json!({
        "name":row.get::<_,String>(0)?,"type":row.get::<_,String>(1)?,
        "generation":row.get::<_,Option<i64>>(2)?,"binding_kind":row.get::<_,Option<String>>(3)?,
        "binding_identity":row.get::<_,Option<String>>(4)?,
    }))).map_err(|_| "cannot inspect saved component metadata")?
        .collect::<Result<Vec<_>,_>>().map_err(|_| "cannot inspect saved component metadata")?;
    if rows.len() > 64 {
        return Err("saved deployment has too many components".into());
    }
    let mut ports = connection.prepare("SELECT component,generation,port FROM port_assignments WHERE deployment_id=?1 ORDER BY component,generation LIMIT 257")
        .map_err(|_| "cannot inspect saved port assignments")?;
    let ports = ports.query_map([deployment], |row| Ok(json!({
        "component":row.get::<_,String>(0)?,"generation":row.get::<_,i64>(1)?,"port":row.get::<_,u16>(2)?,
    }))).map_err(|_| "cannot inspect saved port assignments")?
        .collect::<Result<Vec<_>,_>>().map_err(|_| "cannot inspect saved port assignments")?;
    if ports.len() > 256 {
        return Err("saved deployment has too many port assignments".into());
    }
    selected["components"] = json!(rows);
    selected["ports"] = json!(ports);
    if selected.to_string().len() > 32768 {
        return Err("saved deployment metadata exceeds its response limit".into());
    }
    Ok(Some(selected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_exactly_scoped_and_omits_private_payloads() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE deployments(repository_id TEXT,deployment_id TEXT,current_generation INTEGER,previous_generation INTEGER,source TEXT,public INTEGER,domain TEXT,domain_override TEXT,spec_json TEXT);
            CREATE TABLE components(deployment_id TEXT,name TEXT,type TEXT,generation INTEGER,binding_kind TEXT,binding_identity TEXT,last_error TEXT);
            CREATE TABLE port_assignments(deployment_id TEXT,component TEXT,generation INTEGER,port INTEGER);
            INSERT INTO deployments VALUES('r1','d1',149,148,'worktree',0,'skydive',NULL,'PRIVATE_SPEC');
            INSERT INTO components VALUES('d1','db','postgres',149,'container','container-identity','PRIVATE_ERROR');
            INSERT INTO port_assignments VALUES('d1','db',0,20001);").unwrap();
        assert!(metadata(&db, "other", "d1").unwrap().is_none());
        assert!(metadata(&db, "r1", "other").unwrap().is_none());
        let result = metadata(&db, "r1", "d1").unwrap().unwrap();
        assert_eq!(result["current_generation"], 149);
        assert_eq!(result["components"][0]["name"], "db");
        assert_eq!(result["ports"][0]["port"], 20001);
        assert!(!result.to_string().contains("PRIVATE_"));
        for _ in 0..65 {
            db.execute(
                "INSERT INTO components VALUES('d1','extra','process',149,NULL,NULL,NULL)",
                [],
            )
            .unwrap();
        }
        assert!(metadata(&db, "r1", "d1").is_err());
    }
}
