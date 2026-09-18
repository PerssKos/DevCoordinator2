use super::*;
use devcoordinator2_api::recovery::Conflict;

pub(super) struct Overlap {
    pub existing: BTreeMap<String, BTreeSet<String>>,
    pub conflict_count: u64,
    pub conflicts: Vec<Conflict>,
}

pub(super) fn classify(source: &Tables, live: &Tables) -> Result<Overlap, DatabaseError> {
    let mut result = Overlap {
        existing: BTreeMap::new(),
        conflict_count: 0,
        conflicts: vec![],
    };
    for &(table, key) in TABLES {
        let existing = result.existing.entry(table.to_owned()).or_default();
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
