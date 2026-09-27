//! Validation of hash-bound capability inventories.
//!
//! This is deliberately a small gate. It does not discover requirements or
//! infer product behavior; it checks the inventory supplied by the project
//! against the Coordinator's task ledger and the source identity of the run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use devcoordinator2_api::completion::{self, Capability, Claim, Scope, State};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::OptionalExtension;

use crate::database::{Database, DatabaseError};

const MANIFEST_SCHEMA_VERSION: u8 = 1;
const MAX_CAPABILITIES: usize = 512;
const MAX_EVIDENCE_REFS: usize = 32;
const MAX_REF_BYTES: usize = 512;

#[derive(Clone)]
pub struct CompletionService {
    database: Database,
}

impl CompletionService {
    pub(crate) fn new(database: Database) -> Self {
        Self { database }
    }

    pub(crate) fn check_path(
        &self,
        repository_id: &str,
        path: &Path,
        manifest: completion::Manifest,
    ) -> Result<completion::CheckResult, ProtocolError> {
        let source_sha256 = devcoordinator2_executor_core::source_digest(path).map_err(|_| {
            ProtocolError::new(
                ErrorCode::RepositoryNotFound,
                "the registered source identity is unavailable",
            )
        })?;
        self.check_source(repository_id, &source_sha256, None, manifest)
    }

    pub(crate) fn check_source(
        &self,
        repository_id: &str,
        source_sha256: &str,
        config_sha256: Option<&str>,
        manifest: completion::Manifest,
    ) -> Result<completion::CheckResult, ProtocolError> {
        validate_manifest(&manifest)?;
        if manifest.source_sha256 != source_sha256 {
            return Err(invalid(
                "capability inventory source identity does not match the governed source",
            ));
        }
        if let (Some(expected), Some(actual)) = (manifest.config_sha256.as_deref(), config_sha256)
            && expected != actual
        {
            return Err(invalid(
                "capability inventory configuration identity does not match the governed configuration",
            ));
        }

        let task_ids = manifest
            .capabilities
            .iter()
            .filter_map(|capability| capability.task_id.as_deref())
            .collect::<BTreeSet<_>>();
        let tasks = self.task_states(&task_ids)?;
        evaluate(repository_id, source_sha256, manifest, &tasks)
    }

    fn task_states(
        &self,
        task_ids: &BTreeSet<&str>,
    ) -> Result<BTreeMap<String, Option<TaskState>>, ProtocolError> {
        if task_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let ids = task_ids
            .iter()
            .map(|id| (*id).to_owned())
            .collect::<Vec<_>>();
        self.database
            .call(move |connection| {
                let mut states = BTreeMap::new();
                for task_id in ids {
                    let row = connection
                        .query_row(
                            "SELECT repository_id,status FROM tasks WHERE task_id=?1",
                            [&task_id],
                            |row| {
                                Ok(TaskState {
                                    repository_id: row.get(0)?,
                                    status: row.get(1)?,
                                })
                            },
                        )
                        .optional()?;
                    states.insert(task_id, row);
                }
                Ok(states)
            })
            .map_err(database_error)
    }
}

#[derive(Clone, Debug)]
struct TaskState {
    repository_id: String,
    status: String,
}

fn evaluate(
    repository_id: &str,
    source_sha256: &str,
    manifest: completion::Manifest,
    tasks: &BTreeMap<String, Option<TaskState>>,
) -> Result<completion::CheckResult, ProtocolError> {
    let claim = manifest.claim.clone();
    let capability_count =
        u32::try_from(manifest.capabilities.len()).expect("bounded capabilities");
    let real_e2e_count = u32::try_from(
        manifest
            .capabilities
            .iter()
            .filter(|capability| capability.state == State::RealE2e)
            .count(),
    )
    .expect("bounded capabilities");
    let incomplete_count = u32::try_from(
        manifest
            .capabilities
            .iter()
            .filter(|capability| {
                capability.scope == Scope::Product && capability.state != State::RealE2e
            })
            .count(),
    )
    .expect("bounded capabilities");
    let mut findings = Vec::new();

    for capability in &manifest.capabilities {
        let task = capability
            .task_id
            .as_deref()
            .and_then(|task_id| tasks.get(task_id).and_then(Option::as_ref));
        if let Some(task_id) = capability.task_id.as_deref() {
            match tasks.get(task_id).and_then(Option::as_ref) {
                None => findings.push(finding(
                    capability,
                    "task_missing",
                    "the referenced Coordinator outcome does not exist",
                )),
                Some(task) if task.repository_id != repository_id => findings.push(finding(
                    capability,
                    "task_wrong_repository",
                    "the referenced Coordinator outcome belongs to another repository",
                )),
                Some(task) if task.status == "dropped" => findings.push(finding(
                    capability,
                    "task_dropped",
                    "the referenced Coordinator outcome was dropped",
                )),
                _ => {}
            }
        }

        if capability.scope == Scope::Product && capability.state != State::RealE2e {
            match (capability.task_id.as_deref(), task) {
                (None, _) => findings.push(finding(
                    capability,
                    "unfinished_outcome_missing",
                    "every incomplete product capability needs an open Coordinator outcome",
                )),
                (Some(_), Some(task)) if task.status == "done" => findings.push(finding(
                    capability,
                    "unfinished_outcome_closed",
                    "an incomplete product capability cannot reference a completed outcome",
                )),
                (Some(_), Some(task))
                    if task.status != "planned" && task.status != "in_progress" =>
                {
                    findings.push(finding(
                        capability,
                        "unfinished_outcome_not_open",
                        "the referenced outcome is not open",
                    ))
                }
                _ => {}
            }
        }

        if capability.scope == Scope::Product
            && capability.state == State::RealE2e
            && !has_runtime_evidence(&capability.evidence_refs)
        {
            findings.push(finding(
                capability,
                "runtime_evidence_missing",
                "real end-to-end capabilities need a source-bound run or journey evidence reference",
            ));
        }

        if capability.enabled_control {
            if capability.scope != Scope::Product || capability.state != State::RealE2e {
                findings.push(finding(
                    capability,
                    "enabled_control_incomplete",
                    "an enabled product control must be backed by real end-to-end behavior",
                ));
            } else if !has_runtime_evidence(&capability.rendered_evidence_refs) {
                findings.push(finding(
                    capability,
                    "enabled_control_evidence_missing",
                    "an enabled control needs rendered interaction evidence with its downstream result",
                ));
            }
        }

        if claim == Claim::Complete {
            if capability.scope == Scope::Product && capability.state != State::RealE2e {
                findings.push(finding(
                    capability,
                    "complete_claim_incomplete",
                    "a complete claim cannot contain an incomplete product capability",
                ));
            }
            if capability.scope == Scope::Product
                && capability.state == State::RealE2e
                && task.is_some_and(|task| task.status == "planned" || task.status == "in_progress")
            {
                findings.push(finding(
                    capability,
                    "complete_claim_open_outcome",
                    "a complete claim cannot leave the referenced outcome open",
                ));
            }
        }
    }

    Ok(completion::CheckResult {
        valid: findings.is_empty(),
        claim,
        source_sha256: source_sha256.to_owned(),
        capability_count,
        real_e2e_count,
        incomplete_count,
        findings,
    })
}

fn validate_manifest(manifest: &completion::Manifest) -> Result<(), ProtocolError> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(invalid("unsupported capability inventory schema"));
    }
    validate_digest(&manifest.source_sha256)?;
    if let Some(config_sha256) = &manifest.config_sha256 {
        validate_digest(config_sha256)?;
    }
    if manifest.capabilities.len() > MAX_CAPABILITIES {
        return Err(invalid(
            "capability inventory contains too many capabilities",
        ));
    }
    let mut ids = BTreeSet::new();
    for capability in &manifest.capabilities {
        if capability.id.trim().is_empty()
            || capability.id.len() > 128
            || !ids.insert(capability.id.as_str())
        {
            return Err(invalid("capability IDs must be unique and bounded"));
        }
        let expected = capability.expected_result.trim();
        if expected.len() < 3 || expected.len() > 2_000 {
            return Err(invalid(
                "capability expected results must be between 3 and 2000 bytes",
            ));
        }
        validate_refs(&capability.evidence_refs, "evidence_refs")?;
        validate_refs(&capability.rendered_evidence_refs, "rendered_evidence_refs")?;
        if capability.scope == Scope::Product
            && capability.state == State::RealE2e
            && capability.evidence_refs.is_empty()
        {
            return Err(invalid("real product capabilities need evidence_refs"));
        }
    }
    Ok(())
}

fn validate_refs(refs: &[String], label: &str) -> Result<(), ProtocolError> {
    if refs.len() > MAX_EVIDENCE_REFS {
        return Err(invalid(format!("{label} contains too many references")));
    }
    if refs.iter().any(|reference| {
        reference.trim().is_empty()
            || reference.len() > MAX_REF_BYTES
            || reference.contains(['\n', '\r'])
    }) {
        return Err(invalid(format!("{label} contains an invalid reference")));
    }
    Ok(())
}

fn has_runtime_evidence(refs: &[String]) -> bool {
    refs.iter()
        .any(|reference| reference.starts_with("run/") || reference.starts_with("journey/"))
}

fn validate_digest(value: &str) -> Result<(), ProtocolError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "source_sha256 must be a 64-character hexadecimal digest",
        ));
    }
    Ok(())
}

fn finding(capability: &Capability, code: &str, detail: &str) -> completion::Finding {
    completion::Finding {
        capability_id: capability.id.clone(),
        code: code.to_owned(),
        detail: detail.to_owned(),
        task_id: capability.task_id.clone(),
    }
}

fn invalid(detail: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, "capability inventory is invalid")
        .with_detail(detail.into())
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "cannot read completion outcomes")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(state: State, scope: Scope) -> completion::Manifest {
        completion::Manifest {
            schema_version: 1,
            claim: Claim::Preliminary,
            source_sha256: "a".repeat(64),
            config_sha256: None,
            capabilities: vec![Capability {
                id: "profile-save".into(),
                scope,
                state,
                expected_result: "saved values survive reload".into(),
                task_id: None,
                evidence_refs: vec!["run/worktree/run-1".into()],
                enabled_control: false,
                rendered_evidence_refs: vec![],
            }],
        }
    }

    #[test]
    fn test_only_fixture_can_be_complete_without_an_outcome() {
        let mut value = manifest(State::FixtureOnly, Scope::TestOnly);
        value.claim = Claim::Complete;
        let result = evaluate("repo", &"a".repeat(64), value, &BTreeMap::new()).unwrap();
        assert!(result.valid);
        assert_eq!(result.incomplete_count, 0);
    }

    #[test]
    fn product_fixture_requires_an_open_outcome() {
        let value = manifest(State::FixtureOnly, Scope::Product);
        let result = evaluate("repo", &"a".repeat(64), value, &BTreeMap::new()).unwrap();
        assert!(!result.valid);
        assert!(
            result
                .findings
                .iter()
                .any(|finding| finding.code == "unfinished_outcome_missing")
        );
    }

    #[test]
    fn complete_claim_rejects_open_outcome_and_missing_control_evidence() {
        let mut value = manifest(State::RealE2e, Scope::Product);
        value.claim = Claim::Complete;
        value.capabilities[0].task_id = Some("p-open".into());
        value.capabilities[0].enabled_control = true;
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "p-open".into(),
            Some(TaskState {
                repository_id: "repo".into(),
                status: "in_progress".into(),
            }),
        );
        let result = evaluate("repo", &"a".repeat(64), value, &tasks).unwrap();
        assert!(!result.valid);
        assert!(
            result
                .findings
                .iter()
                .any(|finding| finding.code == "complete_claim_open_outcome")
        );
        assert!(
            result
                .findings
                .iter()
                .any(|finding| finding.code == "enabled_control_evidence_missing")
        );
    }

    #[test]
    fn configuration_identity_mismatch_is_rejected() {
        let mut value = manifest(State::FixtureOnly, Scope::TestOnly);
        value.config_sha256 = Some("b".repeat(64));
        let temporary = tempfile::tempdir().unwrap();
        let service = CompletionService {
            database: Database::open(temporary.path().join("authority.sqlite3")).unwrap(),
        };
        let error = service
            .check_source("repo", &"a".repeat(64), Some(&"c".repeat(64)), value)
            .unwrap_err();
        assert!(error.detail.contains("configuration identity"));
    }

    #[test]
    fn preliminary_product_gap_is_valid_only_with_an_open_matching_outcome() {
        let mut value = manifest(State::Deferred, Scope::Product);
        value.capabilities[0].task_id = Some("p-open".into());
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "p-open".into(),
            Some(TaskState {
                repository_id: "repo".into(),
                status: "in_progress".into(),
            }),
        );
        let result = evaluate("repo", &"a".repeat(64), value, &tasks).unwrap();
        assert!(result.valid);
        assert_eq!(result.incomplete_count, 1);
    }

    #[test]
    fn wrong_repository_and_dropped_outcomes_are_rejected() {
        let mut value = manifest(State::ExternalBlocked, Scope::Product);
        value.capabilities[0].task_id = Some("p-other".into());
        value.capabilities.push(Capability {
            id: "another-gap".into(),
            scope: Scope::Product,
            state: State::Deferred,
            expected_result: "the deferred behavior is delivered".into(),
            task_id: Some("p-dropped".into()),
            evidence_refs: vec![],
            enabled_control: false,
            rendered_evidence_refs: vec![],
        });
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "p-other".into(),
            Some(TaskState {
                repository_id: "other-repo".into(),
                status: "dropped".into(),
            }),
        );
        tasks.insert(
            "p-dropped".into(),
            Some(TaskState {
                repository_id: "repo".into(),
                status: "dropped".into(),
            }),
        );
        let result = evaluate("repo", &"a".repeat(64), value, &tasks).unwrap();
        assert!(!result.valid);
        assert!(
            result
                .findings
                .iter()
                .any(|finding| finding.code == "task_wrong_repository")
        );
        assert!(
            result
                .findings
                .iter()
                .any(|finding| finding.code == "task_dropped")
        );
    }
}
