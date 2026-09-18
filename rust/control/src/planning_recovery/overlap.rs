use super::*;
use devcoordinator2_api::recovery::Conflict;

pub(super) struct Overlap {
    pub existing: BTreeMap<String, BTreeSet<String>>,
    pub conflicting: BTreeMap<String, BTreeSet<String>>,
    pub conflict_count: u64,
    pub conflicts: Vec<Conflict>,
}

pub(super) fn classify(source: &Tables, live: &Tables) -> Result<Overlap, DatabaseError> {
    let mut result = Overlap {
        existing: BTreeMap::new(),
        conflicting: BTreeMap::new(),
        conflict_count: 0,
        conflicts: vec![],
    };
    for &(table, key) in TABLES {
        let existing = result.existing.entry(table.to_owned()).or_default();
        let conflicting = result.conflicting.entry(table.to_owned()).or_default();
        if table == "decision_summaries" {
            continue;
        }
        if matches!(table, "plan_events" | "visual_feedback_events") {
            let mut occurrences = BTreeMap::<String, usize>::new();
            for row in &live[table] {
                let mut event = row.clone();
                event.remove(key);
                let encoded = serde_json::to_string(&event)
                    .map_err(|_| invalid("cannot compare saved event"))?;
                *occurrences.entry(encoded).or_default() += 1;
            }
            for row in &source[table] {
                let mut event = row.clone();
                event.remove(key);
                let encoded = serde_json::to_string(&event)
                    .map_err(|_| invalid("cannot compare saved event"))?;
                if let Some(count) = occurrences.get_mut(&encoded)
                    && *count > 0
                {
                    *count -= 1;
                    existing.insert(number(row, key)?.to_string());
                }
            }
            continue;
        }
        let current: BTreeMap<&str, &Row> = live[table]
            .iter()
            .map(|row| text(row, key).map(|id| (id, row)))
            .collect::<Result<_, _>>()?;
        for row in &source[table] {
            let id = text(row, key)?;
            let Some(present) = current.get(id) else {
                continue;
            };
            let fields: Vec<String> = row
                .iter()
                .filter(|(field, value)| present.get(*field) != Some(*value))
                .map(|(field, _)| field.clone())
                .collect();
            if fields.is_empty() {
                existing.insert(id.to_owned());
                continue;
            }
            result.conflict_count += 1;
            conflicting.insert(id.to_owned());
            if result.conflicts.len() < 64 {
                result.conflicts.push(Conflict {
                    kind: table.to_owned(),
                    id: id.to_owned(),
                    fields,
                    saved_sha256: digest(
                        serde_json::to_vec(row)
                            .map_err(|_| invalid("cannot fingerprint saved record"))?,
                    ),
                    live_sha256: digest(
                        serde_json::to_vec(present)
                            .map_err(|_| invalid("cannot fingerprint live record"))?,
                    ),
                });
            }
        }
    }
    Ok(result)
}

pub(super) fn preserve_live_tasks(
    request: &Request,
    overlap: &Overlap,
    live_sha256: &str,
) -> Result<BTreeSet<String>, DatabaseError> {
    if request.preserve_live_tasks.len() > 64 {
        return Err(invalid(
            "review at most 64 task conflicts per recovery plan",
        ));
    }
    if !request.preserve_live_tasks.is_empty()
        && request.expected_live_sha256.as_deref() != Some(live_sha256)
    {
        return Err(invalid(
            "task conflict review requires the current dry-run fingerprint",
        ));
    }
    let mut reviewed = BTreeSet::new();
    for choice in &request.preserve_live_tasks {
        let conflict = overlap
            .conflicts
            .iter()
            .find(|conflict| conflict.kind == "tasks" && conflict.id == choice.task_id)
            .ok_or_else(|| invalid("reviewed task is not in the current bounded conflict plan"))?;
        if !reviewed.insert(choice.task_id.clone())
            || choice.saved_sha256 != conflict.saved_sha256
            || choice.live_sha256 != conflict.live_sha256
        {
            return Err(invalid(
                "task conflict review is duplicated or its record hashes changed",
            ));
        }
        if conflict
            .fields
            .iter()
            .any(|field| !matches!(field.as_str(), "status" | "updated_at" | "position"))
        {
            return Err(invalid(
                "only task status, update time and position conflicts support preserving live state",
            ));
        }
    }
    Ok(reviewed)
}
