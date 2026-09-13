use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn cli_separates_valid_structural_review_from_full_assurance_and_rejects_fake_tests() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let out = dir.path().join("audit");
    write(
        &repo.join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n#[test]\nfn works() { assert_eq!(value(), 1); }\n",
    );
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "-A"])
            .status()
            .unwrap()
            .success()
    );
    let binary = env!("CARGO_BIN_EXE_devcoordinator2-tooling");
    let built = Command::new(binary)
        .args(["audit", "test-coverage", "build", "--repo"])
        .arg(&repo)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["assurance_version"], 2);
    assert!(out.join("assurance-input.example.json").is_file());
    assert!(!out.join("effort_ledger.json").exists());
    let mut ledger: Value =
        serde_json::from_slice(&std::fs::read(out.join("review_ledger.json")).unwrap()).unwrap();
    ledger["lead_review"] = json!({"status":"completed","agent_id":"fixture","runtime_provenance":"CLI contract fixture"});
    for row in ledger["batch_workers"].as_array_mut().unwrap() {
        row["status"] = json!("completed");
        row["agent_id"] = json!("fixture");
        row["runtime_provenance"] = json!("CLI contract fixture");
    }
    write(
        &out.join("review_ledger.json"),
        &serde_json::to_string(&ledger).unwrap(),
    );
    for batch in manifest["batches"].as_array().unwrap() {
        let units = manifest["coverage_units"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|unit| {
                batch["coverage_units"]
                    .as_array()
                    .unwrap()
                    .contains(&unit["unit_id"])
            })
            .collect::<Vec<_>>();
        let coverage = units
            .iter()
            .map(|unit| {
                format!(
                    "| {} | CHECKED | {} | Actual Rust source with an inline test |",
                    unit["unit_id"].as_str().unwrap(),
                    unit["sha256"].as_str().unwrap()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let inventory=manifest["test_coverage_audit"]["target_inventory"].as_array().unwrap().iter().filter(|target|units.iter().any(|unit|unit["unit_id"]==target["unit_id"])).map(|target|format!("| {} | {} | {} | {} | {} | TESTED | STRUCTURAL | src/lib.rs#works | The test declares an output assertion; execution is unproven | Obtain a source-bound run |",target["target_id"].as_str().unwrap(),target["unit_id"].as_str().unwrap(),target["rel_path"].as_str().unwrap(),target["symbol"].as_str().unwrap(),target["kind"].as_str().unwrap())).collect::<Vec<_>>().join("\n");
        write(
            &out.join(format!("reports/{}.md", batch["id"].as_str().unwrap())),
            &format!(
                "## Run ID\n{}\n\n## Batch ID\n{}\n\n## Batch Summary\nStructural CLI fixture.\n\n## File Coverage\n| Unit | Status | SHA-256 | Purpose |\n| --- | --- | --- | --- |\n{coverage}\n\n## Test Target Inventory\n| Target ID | Unit | File | Target | Kind | Disposition | Evidence Level | Existing Test Evidence | Scenario Assessment | Recommendation |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n{inventory}\n\n## Coverage Findings\nNo findings.\n\n## No Gap Notes\nThe source reference exists; empirical assurance remains unproven.\n\n## Open Questions\nMissing run evidence.\n",
                manifest["run_id"].as_str().unwrap(),
                batch["id"].as_str().unwrap()
            ),
        );
    }
    let verify = |strict: bool| {
        let mut cmd = Command::new(binary);
        cmd.args(["audit", "test-coverage", "verify", "--manifest"])
            .arg(out.join("manifest.json"))
            .arg("--reports")
            .arg(out.join("reports"))
            .arg("--json");
        if strict {
            cmd.arg("--require-assurance");
        }
        cmd.output().unwrap()
    };
    let ordinary = verify(false);
    assert!(
        ordinary.status.success(),
        "{}",
        String::from_utf8_lossy(&ordinary.stdout)
    );
    let value: Value = serde_json::from_slice(&ordinary.stdout).unwrap();
    assert_eq!(value["assurance_met"], false);
    assert_eq!(value["assurance"]["coverage"], "unproven");
    assert_eq!(verify(true).status.code(), Some(3));
    let report = out.join("reports/batch_001.md");
    let original = std::fs::read_to_string(&report).unwrap();
    write(
        &report,
        &original.replace("src/lib.rs#works", "src/lib.rs#value"),
    );
    assert_eq!(verify(true).status.code(), Some(1));
    write(
        &report,
        &original.replace("| STRUCTURAL |", "| EMPIRICAL |"),
    );
    assert_eq!(verify(true).status.code(), Some(1));
}
