use std::fs::{self, File, FileTimes};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use devcoordinator2_executor_core::source_digest;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Repository(PathBuf);

impl Repository {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "dc2-source-content-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        git(&path, &["init", "-q"]);
        fs::write(path.join("source.txt"), b"unchanged fixture\n").unwrap();
        fs::write(path.join("data.bin"), [0, 1, 0, 255]).unwrap();
        git(&path, &["add", "source.txt", "data.bin"]);
        Self(path)
    }

    fn digest(&self) -> String {
        source_digest(&self.0).unwrap()
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "fixture git failed");
}

fn change_time(path: &Path) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(42)))
        .unwrap();
}

#[test]
fn timestamp_and_index_refresh_do_not_change_source_identity() {
    let repo = Repository::new();
    let before = repo.digest();
    change_time(&repo.0.join("source.txt"));
    change_time(&repo.0.join("data.bin"));
    assert_eq!(
        before,
        repo.digest(),
        "stat-only changes are not source edits"
    );
    git(&repo.0, &["update-index", "--refresh"]);
    assert_eq!(before, repo.digest());
    fs::write(repo.0.join("data.bin"), [255, 0, 1, 0]).unwrap();
    let dirty = repo.digest();
    assert_ne!(before, dirty);
    change_time(&repo.0.join("source.txt"));
    assert_eq!(
        dirty,
        repo.digest(),
        "a real dirty file must not make unrelated timestamps significant"
    );
}

#[test]
fn real_bytes_modes_deletion_staging_and_untracked_files_are_detected() {
    let repo = Repository::new();
    let before = repo.digest();
    fs::write(repo.0.join("source.txt"), b"actual edit\n").unwrap();
    assert_ne!(before, repo.digest());
    git(&repo.0, &["add", "source.txt"]);
    let staged = repo.digest();
    assert_ne!(before, staged);
    change_time(&repo.0.join("source.txt"));
    assert_eq!(staged, repo.digest());
    fs::set_permissions(repo.0.join("source.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(staged, repo.digest());
    fs::set_permissions(repo.0.join("source.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(staged, repo.digest());
    fs::write(repo.0.join("untracked"), b"included\n").unwrap();
    let extra = repo.digest();
    assert_ne!(staged, extra);
    fs::write(repo.0.join("untracked"), b"changed\n").unwrap();
    assert_ne!(extra, repo.digest());
    fs::remove_file(repo.0.join("untracked")).unwrap();
    fs::remove_file(repo.0.join("source.txt")).unwrap();
    assert_ne!(staged, repo.digest());
}

#[test]
fn raw_bytes_are_not_hidden_by_git_line_ending_filters() {
    let repo = Repository::new();
    fs::write(repo.0.join(".gitattributes"), b"*.txt text eol=lf\n").unwrap();
    git(&repo.0, &["add", ".gitattributes", "source.txt"]);
    let before = repo.digest();
    fs::write(repo.0.join("source.txt"), b"unchanged fixture\r\n").unwrap();
    assert_ne!(
        before,
        repo.digest(),
        "raw working bytes matter even if Git normalizes line endings"
    );
}

#[test]
fn unchanged_symlink_target_is_stable_and_retargeting_is_detected() {
    let repo = Repository::new();
    symlink("source.txt", repo.0.join("link")).unwrap();
    git(&repo.0, &["add", "link"]);
    let before = repo.digest();
    fs::remove_file(repo.0.join("link")).unwrap();
    symlink("source.txt", repo.0.join("link")).unwrap();
    assert_eq!(before, repo.digest());
    fs::remove_file(repo.0.join("link")).unwrap();
    symlink("data.bin", repo.0.join("link")).unwrap();
    assert_ne!(before, repo.digest());
}
