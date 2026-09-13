use super::super::{CheckFinding, CheckReport, CheckStatus, audit_agent_neutrality};
use super::*;
use serde_json::json;
use tempfile::TempDir;

fn fixture(documents: &[(&str, &[&str], &str)]) -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("reference/universal");
    fs::create_dir_all(root.join("modules")).unwrap();
    fs::write(
        root.join("AGENTS.md"),
        format!("{MARKER}\n{CORE_TITLE}\n\nCore requirement\n"),
    )
    .unwrap();
    let modules = documents
        .iter()
        .map(|(id, applicability, text)| {
            let relative = format!("modules/{id}.md");
            fs::write(root.join(&relative), text).unwrap();
            json!({"id": id, "relativePath": relative, "applicability": applicability})
        })
        .collect::<Vec<_>>();
    fs::write(
        root.join("modules.json"),
        json!({"version":1,"modules":modules}).to_string(),
    )
    .unwrap();
    directory
}

#[test]
fn legacy_documents_remain_byte_identical_and_ignore_sidecars() {
    let directory = tempfile::tempdir().unwrap();
    let policy = directory.path().join("AGENTS.md");
    fs::write(
        directory.path().join("modules.json"),
        "invalid unrelated sidecar",
    )
    .unwrap();
    for text in [String::new(), "Legacy full document\r\n".repeat(2000)] {
        fs::write(&policy, &text).unwrap();
        assert_eq!(
            read_policy_bundle(&policy).unwrap(),
            PolicyBundle {
                contract_text: text.clone(),
                sources: vec![PolicySource {
                    path: policy.clone(),
                    scan_text: text
                }],
            }
        );
    }
}

#[test]
fn all_modules_load_in_manifest_order_regardless_of_applicability() {
    let directory = fixture(&[
        (
            "second",
            &["implementation"],
            "## Second\nImplementation requirement\n",
        ),
        (
            "first",
            &["unknown-future-purpose"],
            "## First\nFuture requirement\n",
        ),
        ("third", &["ui"], "## Third\nInterface requirement\n"),
    ]);
    let root = directory.path().join("reference/universal");
    assert_eq!(
        read_policy_bundle(&root.join("AGENTS.md")).unwrap(),
        PolicyBundle {
            contract_text: format!(
                "{AUDIT_TITLE}\n\nCore requirement\n\n\n## Second\nImplementation requirement\n\n\n## First\nFuture requirement\n\n\n## Third\nInterface requirement\n"
            ),
            sources: vec![
                PolicySource {
                    path: root.join("AGENTS.md"),
                    scan_text: format!("\n{CORE_TITLE}\n\nCore requirement\n")
                },
                PolicySource {
                    path: root.join("modules/second.md"),
                    scan_text: "## Second\nImplementation requirement\n".into()
                },
                PolicySource {
                    path: root.join("modules/first.md"),
                    scan_text: "## First\nFuture requirement\n".into()
                },
                PolicySource {
                    path: root.join("modules/third.md"),
                    scan_text: "## Third\nInterface requirement\n".into()
                },
            ],
        }
    );
}

#[cfg(unix)]
#[test]
fn installed_core_symlink_loads_modules_from_its_canonical_source() {
    let directory = fixture(&[("rules", &["always"], "## Rules\nRequired behavior\n")]);
    let source = directory.path().join("reference/universal/AGENTS.md");
    let installation = tempfile::tempdir().unwrap();
    let link = installation.path().join("AGENTS.md");
    std::os::unix::fs::symlink(&source, &link).unwrap();
    let mut expected = read_policy_bundle(&source).unwrap();
    expected.sources[0].path = link.clone();
    assert_eq!(read_policy_bundle(&link).unwrap(), expected);
}

#[test]
fn neutrality_uses_module_coordinates_and_excludes_only_the_verified_marker() {
    let directory = fixture(&[("rules", &["always"], "## Rules\nClaude requirement\n")]);
    let root = directory.path().join("reference/universal");
    fs::write(
        root.join("AGENTS.md"),
        format!("{MARKER}\n{CORE_TITLE}\n\nCodex requirement\n{MARKER}\n"),
    )
    .unwrap();
    assert_eq!(
        audit_agent_neutrality(directory.path()).unwrap(),
        CheckReport {
            check: "agent_neutrality",
            status: CheckStatus::Findings,
            total_findings: 3,
            findings_truncated: false,
            findings: vec![
                CheckFinding {
                    rule: "runtime-name".into(),
                    path: "reference/universal/AGENTS.md".into(),
                    line: Some(4),
                    detail: "shared contract is runtime-specific".into()
                },
                CheckFinding {
                    rule: "runtime-name".into(),
                    path: "reference/universal/AGENTS.md".into(),
                    line: Some(5),
                    detail: "shared contract is runtime-specific".into()
                },
                CheckFinding {
                    rule: "runtime-name".into(),
                    path: "reference/universal/modules/rules.md".into(),
                    line: Some(2),
                    detail: "shared contract is runtime-specific".into()
                },
            ],
        }
    );
}

#[test]
fn crlf_transport_headers_preserve_source_line_coordinates() {
    let directory = fixture(&[("rules", &["always"], "Module requirement")]);
    let root = directory.path().join("reference/universal");
    fs::write(
        root.join("AGENTS.md"),
        format!("{MARKER}\r\n{CORE_TITLE}\r\n\r\nCore requirement\r\n"),
    )
    .unwrap();
    let bundle = read_policy_bundle(&root.join("AGENTS.md")).unwrap();
    assert_eq!(
        bundle.sources[0].scan_text,
        format!("\n{CORE_TITLE}\r\n\r\nCore requirement\r\n")
    );
    assert_eq!(
        bundle.contract_text,
        format!("{AUDIT_TITLE}\n\r\nCore requirement\r\n\n\nModule requirement")
    );
}

#[test]
fn unsupported_versions_and_invalid_titles_are_not_laundered() {
    let directory = fixture(&[("rules", &["always"], "Module requirement")]);
    let policy = directory.path().join("reference/universal/AGENTS.md");
    for text in [
        format!("<!-- codex:focused-policy:v2 -->\n{CORE_TITLE}\n"),
        format!("{MARKER}\n# Wrong title\n"),
        MARKER.to_string(),
    ] {
        fs::write(&policy, text).unwrap();
        assert_eq!(
            read_policy_bundle(&policy).unwrap_err().kind,
            CheckErrorKind::InvalidInput
        );
    }
}

#[test]
fn invalid_manifest_shapes_and_duplicate_sources_are_rejected() {
    let directory = fixture(&[("rules", &["always"], "Module requirement")]);
    let root = directory.path().join("reference/universal");
    let entry = json!({"id":"rules","relativePath":"modules/rules.md","applicability":["always"]});
    for manifest in [
        json!({"version":2,"modules":[entry.clone()]}),
        json!({"version":1,"modules":[]}),
        json!({"version":1,"modules":vec![entry.clone();33]}),
        json!({"version":1,"modules":[entry.clone(),entry.clone()]}),
        json!({"version":1,"modules":[entry.clone(),{"id":"other","relativePath":"modules/rules.md","applicability":["always"]}]}),
        json!({"version":1,"modules":[{"id":"rules","relativePath":"modules/rules.md","applicability":[]}]}),
        json!({"version":1,"modules":[{"id":"rules","relativePath":"modules/rules.md","applicability":vec!["always";17]}]}),
        json!({"version":1,"modules":[entry],"remote":"not allowed"}),
    ] {
        fs::write(root.join("modules.json"), manifest.to_string()).unwrap();
        assert_eq!(
            read_policy_bundle(&root.join("AGENTS.md"))
                .unwrap_err()
                .kind,
            CheckErrorKind::InvalidInput
        );
    }
}

#[test]
fn unsafe_relative_paths_are_rejected_before_reading() {
    let directory = fixture(&[("rules", &["always"], "Module requirement")]);
    let root = directory.path().join("reference/universal");
    for path in [
        "../outside.md",
        "/tmp/outside.md",
        "C:/outside.md",
        "https://example.test/policy",
        "modules/../outside.md",
        "modules/./rules.md",
        "modules//rules.md",
        "modules\\rules.md",
        "modules/rules\0.md",
    ] {
        fs::write(root.join("modules.json"), json!({"version":1,"modules":[{"id":"rules","relativePath":path,"applicability":["always"]}]}).to_string()).unwrap();
        assert_eq!(
            read_policy_bundle(&root.join("AGENTS.md"))
                .unwrap_err()
                .kind,
            CheckErrorKind::InvalidInput
        );
    }
}

#[test]
fn missing_empty_non_regular_or_invalid_utf8_modules_never_return_partial_policy() {
    let directory = fixture(&[
        ("first", &["always"], "First requirement"),
        ("last", &["ui"], "Last requirement"),
    ]);
    let root = directory.path().join("reference/universal");
    let last = root.join("modules/last.md");
    for bytes in [Vec::new(), vec![0xff]] {
        fs::write(&last, bytes).unwrap();
        assert_eq!(
            read_policy_bundle(&root.join("AGENTS.md"))
                .unwrap_err()
                .kind,
            CheckErrorKind::InvalidInput
        );
    }
    fs::remove_file(&last).unwrap();
    assert_eq!(
        read_policy_bundle(&root.join("AGENTS.md"))
            .unwrap_err()
            .kind,
        CheckErrorKind::InputUnavailable
    );
    fs::create_dir(&last).unwrap();
    assert_eq!(
        read_policy_bundle(&root.join("AGENTS.md"))
            .unwrap_err()
            .kind,
        CheckErrorKind::InvalidInput
    );
}

#[test]
fn manifest_document_and_aggregate_limits_fail_without_truncation() {
    let directory = fixture(&[
        ("one", &["always"], "One"),
        ("two", &["always"], "Two"),
        ("three", &["always"], "Three"),
        ("four", &["always"], "Four"),
    ]);
    let root = directory.path().join("reference/universal");
    fs::write(
        root.join("modules/one.md"),
        "x".repeat(MAX_DOCUMENT_BYTES as usize + 1),
    )
    .unwrap();
    assert_eq!(
        read_policy_bundle(&root.join("AGENTS.md"))
            .unwrap_err()
            .kind,
        CheckErrorKind::InvalidInput
    );
    for name in ["one", "two", "three", "four"] {
        fs::write(
            root.join(format!("modules/{name}.md")),
            "x".repeat(MAX_DOCUMENT_BYTES as usize),
        )
        .unwrap();
    }
    assert_eq!(
        read_policy_bundle(&root.join("AGENTS.md"))
            .unwrap_err()
            .kind,
        CheckErrorKind::InvalidInput
    );
    fs::write(
        root.join("modules.json"),
        " ".repeat(MAX_MANIFEST_BYTES as usize + 1),
    )
    .unwrap();
    assert_eq!(
        read_policy_bundle(&root.join("AGENTS.md"))
            .unwrap_err()
            .kind,
        CheckErrorKind::InvalidInput
    );
    fs::write(
        root.join("AGENTS.md"),
        format!(
            "{MARKER}\n{CORE_TITLE}\n{}",
            "x".repeat(MAX_DOCUMENT_BYTES as usize)
        ),
    )
    .unwrap();
    assert_eq!(
        read_policy_bundle(&root.join("AGENTS.md"))
            .unwrap_err()
            .kind,
        CheckErrorKind::InvalidInput
    );
}

#[cfg(unix)]
#[test]
fn symlinked_manifest_modules_and_parent_directories_are_rejected() {
    use std::os::unix::fs::symlink;
    for relative in ["modules.json", "modules/rules.md", "modules"] {
        let directory = fixture(&[("rules", &["always"], "Module requirement")]);
        let root = directory.path().join("reference/universal");
        let original = root.join(relative);
        let moved = directory.path().join("outside");
        fs::rename(&original, &moved).unwrap();
        symlink(&moved, &original).unwrap();
        assert_eq!(
            read_policy_bundle(&root.join("AGENTS.md"))
                .unwrap_err()
                .kind,
            CheckErrorKind::InvalidInput
        );
    }
}
