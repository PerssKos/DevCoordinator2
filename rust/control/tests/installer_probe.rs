use std::process::Command;

#[test]
fn installer_accepts_deployment_only_but_rejects_invalid_declared_capabilities() {
    let temporary = tempfile::tempdir().unwrap();
    let deployment = r#"schema = 2
[deployment.site]
source = ["worktree"]
domain = "site"
components = ["web"]
public = false
[deployment.site.component.web]
type = "process"
command = ["python3", "serve.py"]
cwd = "."
port = true
route = true
health = { path = "/healthz", timeout_seconds = 30 }
"#;
    let probe = |config: &str| {
        std::fs::write(temporary.path().join(".devcoordinator.toml"), config).unwrap();
        Command::new(env!("CARGO_BIN_EXE_devcoordinator2"))
            .arg("--validate-repository-config")
            .arg(temporary.path())
            .output()
            .unwrap()
    };
    let valid = probe(deployment);
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&valid.stdout).unwrap();
    assert_eq!(value["tests"], serde_json::json!([]));
    assert_eq!(value["deployments"], serde_json::json!(["site"]));
    let mixed = probe(&format!(
        "{deployment}\n[test.unit]\n[[test.unit.check]]\nname = 'main'\ntier = 'release'\ncommand = ['true']\n"
    ));
    assert!(mixed.status.success());
    let value: serde_json::Value = serde_json::from_slice(&mixed.stdout).unwrap();
    assert_eq!(value["tests"], serde_json::json!(["unit"]));
    assert_eq!(value["deployments"], serde_json::json!(["site"]));
    for invalid in [
        deployment.replace("schema = 2", "schema = 1"),
        deployment.replace("schema = 2", "schema = 2\ntest = false"),
        deployment.replace("schema = 2", "schema = 2\nunknown = true"),
        deployment.replace("type = \"process\"", "type = \"unknown\""),
        format!("{deployment}\n[test.invalid]\n"),
        format!("{deployment}\n[test]\n"),
        "schema = 2\n".to_owned(),
    ] {
        assert!(
            !probe(&invalid).status.success(),
            "invalid configuration accepted"
        );
    }
}

#[test]
fn installer_probes_report_embedded_commit_and_strict_repository_configuration() {
    let binary = env!("CARGO_BIN_EXE_devcoordinator2");
    let commit = Command::new(binary)
        .arg("--source-commit")
        .output()
        .expect("source commit probe");
    assert!(commit.status.success());
    assert!(!String::from_utf8(commit.stdout).unwrap().trim().is_empty());

    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(
        temporary.path().join(".devcoordinator.toml"),
        r#"schema = 2
[test]
default = "unit"
[test.unit]
[[test.unit.check]]
name = "main"
tier = "release"
command = ["true"]
"#,
    )
    .unwrap();
    let validated = Command::new(binary)
        .arg("--validate-repository-config")
        .arg(temporary.path())
        .output()
        .expect("configuration probe");
    assert!(
        validated.status.success(),
        "{}",
        String::from_utf8_lossy(&validated.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&validated.stdout).unwrap();
    assert_eq!(value["schema"], 2);
    assert_eq!(value["tests"], serde_json::json!(["unit"]));
    assert_eq!(value["deployments"], serde_json::json!([]));

    std::fs::write(
        temporary.path().join(".devcoordinator.toml"),
        "schema = 1\n[test.unit]\ncommand = ['true']\n",
    )
    .unwrap();
    let rejected = Command::new(binary)
        .arg("--validate-repository-config")
        .arg(temporary.path())
        .output()
        .expect("invalid configuration probe");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("not ready for schema 2"));
}
