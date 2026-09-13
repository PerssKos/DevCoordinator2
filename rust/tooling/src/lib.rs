use std::path::Path;

pub mod audit_common;
pub mod audit_evidence;
pub mod audit_findings;
pub mod audit_ledger;
pub mod audit_queue;
pub mod audit_targets;
pub mod audit_verify;
pub mod cutover;
pub mod decision_import;
pub mod formal_review;
pub mod formal_selftest;
pub mod install;
pub mod instance;
pub mod journey_docs;
pub mod legacy_export;
pub mod legacy_import;
pub mod marker_free;
pub mod public_artifacts;
pub mod python_guard;
pub mod repository_checks;
pub mod skill_links;
pub mod skill_selftest;
pub mod skill_validation;
pub mod test_assurance;
pub mod test_catalog;
pub mod test_coverage_audit;
pub mod ui_audit;
pub mod ui_audit_verify;
pub mod ui_gate;

pub fn export_contract(path: &Path, check: bool) -> Result<(), String> {
    let document = devcoordinator2_api::contract_document();
    let mut rendered =
        serde_json::to_string_pretty(&document).map_err(|error| error.to_string())?;
    rendered.push('\n');
    if check {
        let current = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if current != rendered {
            return Err(format!(
                "{} does not match the Rust contract",
                path.display()
            ));
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    std::fs::write(path, rendered)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}
