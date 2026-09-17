use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

fn git(root: &Path, bin: &Path, args: &[&str]) -> Output {
    let mut paths = vec![bin.to_owned()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("GIT_AUTHOR_NAME", "Guard fixture")
        .env("GIT_COMMITTER_NAME", "Guard fixture")
        .env("GIT_AUTHOR_EMAIL", "guard@example.invalid")
        .env("GIT_COMMITTER_EMAIL", "guard@example.invalid")
        .output()
        .unwrap()
}

#[test]
fn repository_hook_rejects_staged_defect_and_accepts_corrected_index() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repo");
    let bin = temporary.path().join("bin");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&bin).unwrap();
    symlink(
        env!("CARGO_BIN_EXE_devcoordinator2-tooling"),
        bin.join("devcoordinator2-tooling"),
    )
    .unwrap();
    assert!(git(&root, &bin, &["init", "-q"]).status.success());
    fs::create_dir(root.join(".githooks")).unwrap();
    let hook = root.join(".githooks/pre-commit");
    fs::write(&hook, include_str!("../../../.githooks/pre-commit")).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        git(&root, &bin, &["config", "core.hooksPath", ".githooks"])
            .status
            .success()
    );
    let synthetic = format!("ghp_{}", "A".repeat(36));
    fs::write(
        root.join("config.txt"),
        format!("Authorization: Bearer {synthetic}\n"),
    )
    .unwrap();
    assert!(git(&root, &bin, &["add", "."]).status.success());
    fs::write(root.join("config.txt"), "corrected but not yet staged\n").unwrap();
    let rejected = git(&root, &bin, &["commit", "-qm", "must reject staged defect"]);
    assert!(!rejected.status.success());
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(diagnostic.contains("text-secret"));
    assert!(!diagnostic.contains(&synthetic));
    assert!(
        !git(&root, &bin, &["rev-parse", "--verify", "HEAD"])
            .status
            .success()
    );
    assert!(git(&root, &bin, &["add", "config.txt"]).status.success());
    let corrected = git(
        &root,
        &bin,
        &["commit", "-qm", "accept corrected staged source"],
    );
    assert!(
        corrected.status.success(),
        "{}",
        String::from_utf8_lossy(&corrected.stderr)
    );
    assert!(
        git(&root, &bin, &["status", "--porcelain"])
            .stdout
            .is_empty()
    );
}
