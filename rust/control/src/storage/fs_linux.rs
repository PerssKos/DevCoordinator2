//! Directory-fd traversal shared by measurement, revalidation and removal.
use super::Measurement;
use crate::storage::{blocked, unavailable};
use devcoordinator2_api::ProtocolError;
use rustix::fs::{self as unix, AtFlags, Dir, Mode, OFlags};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

const MAX_ENTRIES: usize = 1_000_000;
const MAX_DEPTH: usize = 128;

pub fn open_directory(path: &Path) -> Result<File, ProtocolError> {
    open_directory_if_present(path)?.ok_or_else(|| blocked("directory_identity_unavailable"))
}

fn open_directory_if_present(path: &Path) -> Result<Option<File>, ProtocolError> {
    if !path.is_absolute() {
        return Err(blocked("path_not_absolute"));
    }
    if path
        .components()
        .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(blocked("path_traversal"));
    }
    let mut fd = File::from(
        unix::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| unavailable("root_directory_unavailable"))?,
    );
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                let opened = unix::openat(
                    &fd,
                    name,
                    OFlags::RDONLY
                        | OFlags::DIRECTORY
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC
                        | OFlags::NONBLOCK,
                    Mode::empty(),
                );
                fd = match opened {
                    Ok(fd) => File::from(fd),
                    Err(rustix::io::Errno::NOENT) => return Ok(None),
                    Err(_) => return Err(blocked("directory_identity_unavailable")),
                };
            }
            _ => return Err(blocked("path_traversal")),
        }
    }
    Ok(Some(fd))
}

pub fn identity(path: &Path) -> Result<(u64, u64), ProtocolError> {
    let m = open_directory(path)?
        .metadata()
        .map_err(|_| unavailable("directory_metadata_unavailable"))?;
    Ok((m.dev(), m.ino()))
}

/// A missing entry or ancestor in a no-follow walk proves absence. Permission
/// errors and substituted symlinks never mean removed.
pub fn absent(path: &Path) -> Result<bool, ProtocolError> {
    let parent = path
        .parent()
        .ok_or_else(|| blocked("invalid_storage_path"))?;
    let name = path
        .file_name()
        .ok_or_else(|| blocked("invalid_storage_path"))?;
    let Some(fd) = open_directory_if_present(parent)? else {
        return Ok(true);
    };
    match unix::statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(false),
        Err(rustix::io::Errno::NOENT) => Ok(true),
        Err(_) => Err(unavailable("removal_observation_unavailable")),
    }
}

fn names(dir: &File) -> Result<Vec<OsString>, ProtocolError> {
    let mut result = Vec::new();
    let mut entries = Dir::read_from(dir).map_err(|_| unavailable("directory_read_failed"))?;
    for entry in &mut entries {
        let entry = entry.map_err(|_| unavailable("directory_read_failed"))?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if result.len() == MAX_ENTRIES {
            return Err(blocked("directory_entry_limit"));
        }
        result.push(std::os::unix::ffi::OsStringExt::from_vec(bytes.to_vec()));
    }
    result.sort();
    Ok(result)
}

pub fn measure(path: &Path) -> Result<Measurement, ProtocolError> {
    let fd = open_directory(path)?;
    let meta = fd
        .metadata()
        .map_err(|_| unavailable("directory_metadata_unavailable"))?;
    let mut value = Measurement {
        device: meta.dev(),
        inode: meta.ino(),
        bytes: 0,
        newest_modified_ns: 0,
        entries: 0,
        nested_git: false,
        protected_metadata: false,
        multiply_linked: false,
    };
    walk_measure(&fd, 0, &mut value, &mut HashSet::new())?;
    Ok(value)
}

/// A scan-owned, fd-relative traversal. A scheduler may retain this cursor
/// between turns without rescanning earlier directory entries.
pub struct MeasurementCursor {
    stack: Vec<(File, Dir, usize)>,
    seen: HashSet<(u64, u64)>,
    value: Measurement,
}

impl MeasurementCursor {
    pub fn new(path: &Path) -> Result<Self, ProtocolError> {
        let directory = open_directory(path)?;
        let own = directory
            .metadata()
            .map_err(|_| unavailable("directory_metadata_unavailable"))?;
        let entries =
            Dir::read_from(&directory).map_err(|_| unavailable("directory_read_failed"))?;
        let mut seen = HashSet::new();
        seen.insert((own.dev(), own.ino()));
        Ok(Self {
            stack: vec![(directory, entries, 0)],
            seen,
            value: Measurement {
                device: own.dev(),
                inode: own.ino(),
                bytes: own.blocks().saturating_mul(512),
                newest_modified_ns: 0,
                entries: 0,
                nested_git: false,
                protected_metadata: false,
                multiply_linked: false,
            },
        })
    }

    pub fn step(
        &mut self,
        max_entries: usize,
        budget: std::time::Duration,
    ) -> Result<Option<Measurement>, ProtocolError> {
        use std::os::unix::ffi::OsStringExt;
        let started = std::time::Instant::now();
        let mut visited = 0;
        while visited < max_entries.max(1) && (visited == 0 || started.elapsed() < budget) {
            let Some((directory, entries, depth)) = self.stack.last_mut() else {
                return Ok(Some(self.value.clone()));
            };
            let Some(entry) = entries.next() else {
                self.stack.pop();
                continue;
            };
            let entry = entry.map_err(|_| unavailable("directory_read_failed"))?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            visited += 1;
            self.value.entries += 1;
            if self.value.entries > MAX_ENTRIES {
                return Err(blocked("directory_entry_limit"));
            }
            let name = OsString::from_vec(bytes.to_vec());
            self.value.nested_git |= name == ".git";
            self.value.protected_metadata |= super::protected_metadata_name(&name);
            let stat = unix::statat(&*directory, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| blocked("directory_changed"))?;
            if stat.st_dev != self.value.device {
                return Err(blocked("unexpected_filesystem"));
            }
            self.value.newest_modified_ns = self
                .value
                .newest_modified_ns
                .max(i128::from(stat.st_mtime) * 1_000_000_000 + i128::from(stat.st_mtime_nsec));
            let kind = unix::FileType::from_raw_mode(stat.st_mode);
            match kind {
                unix::FileType::Directory => {
                    let child_depth = *depth + 1;
                    if child_depth > MAX_DEPTH {
                        return Err(blocked("directory_depth_limit"));
                    }
                    let child = File::from(
                        unix::openat2(
                            &*directory,
                            &name,
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                            unix::ResolveFlags::BENEATH
                                | unix::ResolveFlags::NO_SYMLINKS
                                | unix::ResolveFlags::NO_XDEV,
                        )
                        .map_err(|_| blocked("directory_changed"))?,
                    );
                    let own = child
                        .metadata()
                        .map_err(|_| unavailable("directory_metadata_unavailable"))?;
                    if self.seen.insert((own.dev(), own.ino())) {
                        self.value.bytes = self
                            .value
                            .bytes
                            .saturating_add(own.blocks().saturating_mul(512));
                    }
                    let entries =
                        Dir::read_from(&child).map_err(|_| unavailable("directory_read_failed"))?;
                    self.stack.push((child, entries, child_depth));
                }
                unix::FileType::RegularFile | unix::FileType::Symlink => {
                    self.value.multiply_linked |=
                        stat.st_nlink > 1 && kind == unix::FileType::RegularFile;
                    if self.seen.insert((stat.st_dev, stat.st_ino)) {
                        self.value.bytes = self.value.bytes.saturating_add(
                            u64::try_from(stat.st_blocks)
                                .map_err(|_| unavailable("invalid_allocated_size"))?
                                .saturating_mul(512),
                        );
                    }
                }
                _ => return Err(blocked("special_file")),
            }
        }
        Ok(if self.stack.is_empty() {
            Some(self.value.clone())
        } else {
            None
        })
    }
}

fn walk_measure(
    dir: &File,
    depth: usize,
    out: &mut Measurement,
    seen: &mut HashSet<(u64, u64)>,
) -> Result<(), ProtocolError> {
    if depth > MAX_DEPTH {
        return Err(blocked("directory_depth_limit"));
    }
    let own = dir
        .metadata()
        .map_err(|_| unavailable("directory_metadata_unavailable"))?;
    if own.dev() != out.device {
        return Err(blocked("unexpected_filesystem"));
    }
    if seen.insert((own.dev(), own.ino())) {
        out.bytes = out.bytes.saturating_add(own.blocks().saturating_mul(512));
    }
    for name in names(dir)? {
        out.entries += 1;
        if out.entries > MAX_ENTRIES {
            return Err(blocked("directory_entry_limit"));
        }
        if name == ".git" {
            out.nested_git = true;
        }
        out.protected_metadata |= super::protected_metadata_name(&name);
        let st = unix::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| blocked("directory_changed"))?;
        if st.st_dev != out.device {
            return Err(blocked("unexpected_filesystem"));
        }
        out.newest_modified_ns = out
            .newest_modified_ns
            .max(i128::from(st.st_mtime) * 1_000_000_000 + i128::from(st.st_mtime_nsec));
        match unix::FileType::from_raw_mode(st.st_mode) {
            unix::FileType::Directory => {
                let child = File::from(
                    unix::openat2(
                        dir,
                        &name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                        unix::ResolveFlags::BENEATH
                            | unix::ResolveFlags::NO_SYMLINKS
                            | unix::ResolveFlags::NO_XDEV,
                    )
                    .map_err(|_| blocked("directory_changed"))?,
                );
                walk_measure(&child, depth + 1, out, seen)?;
            }
            unix::FileType::RegularFile | unix::FileType::Symlink => {
                out.multiply_linked |= st.st_nlink > 1
                    && unix::FileType::from_raw_mode(st.st_mode) == unix::FileType::RegularFile;
                if seen.insert((st.st_dev, st.st_ino)) {
                    out.bytes = out.bytes.saturating_add(
                        u64::try_from(st.st_blocks)
                            .map_err(|_| unavailable("invalid_allocated_size"))?
                            .saturating_mul(512),
                    );
                }
            }
            _ => return Err(blocked("special_file")),
        }
    }
    Ok(())
}

/// Mount IDs, not device numbers, detect nested bind mounts on the same disk.
pub fn mount_targets() -> Result<Vec<PathBuf>, ProtocolError> {
    let text = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|_| unavailable("mount_inventory_unavailable"))?;
    Ok(text
        .lines()
        .filter_map(|l| l.split_whitespace().nth(4))
        .map(|s| PathBuf::from(unescape_mount(s)))
        .collect())
}

pub fn unescape_mount(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

pub fn remove_tree(
    path: &Path,
    expected: (u64, u64),
    allow_git: bool,
    preserve_metadata: bool,
) -> Result<(), ProtocolError> {
    remove_tree_checked(path, expected, allow_git, preserve_metadata, None)
}

pub fn validate_native_snapshot(path: &Path) -> Result<(), ProtocolError> {
    native_snapshot_entries(&open_directory(path)?)?;
    Ok(())
}

fn native_snapshot_entries(dir: &File) -> Result<Vec<OsString>, ProtocolError> {
    let entries = names(dir)?;
    let postgres = entries.iter().any(|name| name == "authority-before.pgdump");
    if !entries
        .iter()
        .any(|name| name == "installation-snapshot.json")
        || entries
            .iter()
            .any(|name| name == "authority-before.sqlite3")
            == postgres
    {
        return Err(blocked("backup_unverified"));
    }
    let header = File::from(
        unix::openat(
            dir,
            "installation-snapshot.json",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|_| blocked("backup_unverified"))?,
    );
    if !header
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= 64 * 1024)
    {
        return Err(blocked("backup_unverified"));
    }
    let mut encoded = Vec::new();
    header
        .take(64 * 1024 + 1)
        .read_to_end(&mut encoded)
        .map_err(|_| blocked("backup_unverified"))?;
    let header: serde_json::Value =
        serde_json::from_slice(&encoded).map_err(|_| blocked("backup_unverified"))?;
    if !matches!(
        (
            postgres,
            header.get("schema").and_then(serde_json::Value::as_u64)
        ),
        (true, Some(3)) | (false, Some(1 | 2))
    ) {
        return Err(blocked("backup_unverified"));
    }
    let device = dir
        .metadata()
        .map_err(|_| unavailable("directory_metadata_unavailable"))?
        .dev();
    for name in &entries {
        if !matches!(
            name.to_str(),
            Some(
                "installation-snapshot.json"
                    | "authority-before.sqlite3"
                    | "authority-before.sqlite3-wal"
                    | "authority-before.sqlite3-shm"
                    | "authority-before.pgdump"
            )
        ) {
            return Err(blocked("native_backup_contents_changed"));
        }
        if postgres
            && name
                .to_str()
                .is_some_and(|name| name.starts_with("authority-before.sqlite3"))
        {
            return Err(blocked("native_backup_contents_changed"));
        }
        let st = unix::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| blocked("directory_changed"))?;
        if unix::FileType::from_raw_mode(st.st_mode) != unix::FileType::RegularFile
            || st.st_dev != device
            || st.st_nlink != 1
        {
            return Err(blocked("native_backup_contents_changed"));
        }
    }
    Ok(entries)
}

pub fn remove_native_snapshot(
    path: &Path,
    expected: (u64, u64),
    expected_parent: (u64, u64),
) -> Result<(), ProtocolError> {
    remove_tree_checked(path, expected, false, true, Some(expected_parent))
}

fn remove_tree_checked(
    path: &Path,
    expected: (u64, u64),
    allow_git: bool,
    preserve_metadata: bool,
    native_parent: Option<(u64, u64)>,
) -> Result<(), ProtocolError> {
    if path.parent().is_none() || path == Path::new("/") {
        return Err(blocked("protected_root"));
    }
    if mount_targets()?
        .iter()
        .any(|p| p == path || p.starts_with(path))
    {
        return Err(blocked("mounted_path"));
    }
    let parent = open_directory(path.parent().ok_or_else(|| blocked("protected_root"))?)?;
    if let Some(expected_parent) = native_parent {
        let parent_metadata = parent
            .metadata()
            .map_err(|_| unavailable("directory_metadata_unavailable"))?;
        if (parent_metadata.dev(), parent_metadata.ino()) != expected_parent {
            return Err(blocked("configured_root_identity_changed"));
        }
    }
    let name = path.file_name().ok_or_else(|| blocked("protected_root"))?;
    let child = File::from(
        unix::openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| blocked("directory_changed"))?,
    );
    let st = child
        .metadata()
        .map_err(|_| unavailable("directory_metadata_unavailable"))?;
    if (st.dev(), st.ino()) != expected {
        return Err(blocked("identity_changed"));
    }
    if native_parent.is_some() {
        // Delete only this fixed native file set. Newly added entries are never
        // traversed or removed; they make the final directory removal fail.
        for entry in native_snapshot_entries(&child)? {
            let st = unix::statat(&child, &entry, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| blocked("directory_changed"))?;
            if unix::FileType::from_raw_mode(st.st_mode) != unix::FileType::RegularFile
                || st.st_dev != expected.0
                || st.st_nlink != 1
            {
                return Err(blocked("native_backup_contents_changed"));
            }
            unix::unlinkat(&child, &entry, AtFlags::empty())
                .map_err(|_| unavailable("directory_remove_failed"))?;
        }
    } else {
        remove_children(&child, expected.0, allow_git, preserve_metadata, 0)?;
    }
    let current = unix::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| blocked("directory_changed"))?;
    if (current.st_dev, current.st_ino) != expected {
        return Err(blocked("identity_changed"));
    }
    unix::unlinkat(&parent, name, AtFlags::REMOVEDIR)
        .map_err(|_| unavailable("directory_remove_failed"))?;
    parent
        .sync_all()
        .map_err(|_| unavailable("directory_sync_failed"))
}

fn remove_children(
    dir: &File,
    device: u64,
    allow_git: bool,
    preserve_metadata: bool,
    depth: usize,
) -> Result<(), ProtocolError> {
    if depth > MAX_DEPTH {
        return Err(blocked("directory_depth_limit"));
    }
    let entries = names(dir)?;
    if !allow_git && entries.iter().any(|n| n == ".git") {
        return Err(blocked("nested_repository"));
    }
    if preserve_metadata && entries.iter().any(|n| super::protected_metadata_name(n)) {
        return Err(blocked("protected_source_or_credentials"));
    }
    for name in entries {
        let st = unix::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| blocked("directory_changed"))?;
        if st.st_dev != device {
            return Err(blocked("unexpected_filesystem"));
        }
        let flags = if unix::FileType::from_raw_mode(st.st_mode) == unix::FileType::Directory {
            let child = File::from(
                unix::openat2(
                    dir,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                    unix::ResolveFlags::BENEATH
                        | unix::ResolveFlags::NO_SYMLINKS
                        | unix::ResolveFlags::NO_XDEV,
                )
                .map_err(|_| blocked("directory_changed"))?,
            );
            let m = child
                .metadata()
                .map_err(|_| unavailable("directory_metadata_unavailable"))?;
            if (m.dev(), m.ino()) != (st.st_dev, st.st_ino) {
                return Err(blocked("identity_changed"));
            }
            remove_children(&child, device, allow_git, preserve_metadata, depth + 1)?;
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        let fresh = unix::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| blocked("directory_changed"))?;
        if (fresh.st_dev, fresh.st_ino) != (st.st_dev, st.st_ino) {
            return Err(blocked("identity_changed"));
        }
        unix::unlinkat(dir, &name, flags).map_err(|_| unavailable("directory_remove_failed"))?;
    }
    Ok(())
}

pub fn filesystem(
    path: &Path,
    now_ms: u64,
    label: &str,
) -> Result<devcoordinator2_api::storage::Filesystem, ProtocolError> {
    let dir = open_directory(path)?;
    let meta = dir
        .metadata()
        .map_err(|_| unavailable("filesystem_metadata_unavailable"))?;
    let st = unix::fstatvfs(&dir).map_err(|_| unavailable("filesystem_size_unavailable"))?;
    Ok(devcoordinator2_api::storage::Filesystem {
        filesystem_id: format!("fs-{}", meta.dev()),
        label: label.into(),
        capacity_bytes: st.f_blocks.saturating_mul(st.f_frsize),
        available_bytes: st.f_bavail.saturating_mul(st.f_frsize),
        measured_at_ms: now_ms,
    })
}

#[cfg(test)]
mod cursor_tests {
    use super::*;
    #[test]
    fn bounded_measurement_matches_full_safety_observation_without_following_links() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("tree");
        std::fs::create_dir_all(root.join("nested/.git")).unwrap();
        for index in 0..37 {
            std::fs::write(
                root.join("nested").join(format!("file-{index}")),
                b"fixture",
            )
            .unwrap();
        }
        std::fs::write(root.join("nested/auth.json"), b"protected fixture").unwrap();
        std::fs::hard_link(root.join("nested/file-0"), root.join("hard-link")).unwrap();
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("unvisited"), vec![0u8; 16384]).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("alias")).unwrap();
        let complete = measure(&root).unwrap();
        let mut cursor = MeasurementCursor::new(&root).unwrap();
        let mut turns = 0;
        let bounded = loop {
            let before = cursor.value.entries;
            let result = cursor.step(3, std::time::Duration::from_secs(1)).unwrap();
            assert!(cursor.value.entries - before <= 3);
            turns += 1;
            if let Some(result) = result {
                break result;
            }
            assert!(turns < 100);
        };
        assert!(turns > 10);
        assert_eq!(
            (bounded.bytes, bounded.entries, bounded.newest_modified_ns),
            (
                complete.bytes,
                complete.entries,
                complete.newest_modified_ns
            )
        );
        assert!(bounded.nested_git && bounded.protected_metadata && bounded.multiply_linked);
        assert!(MeasurementCursor::new(&root.join("alias")).is_err());
    }
}
