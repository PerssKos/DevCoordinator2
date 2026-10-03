//! Directory-fd traversal shared by measurement, revalidation and removal.
use super::{blocked, unavailable};
use devcoordinator2_api::ProtocolError;
use rustix::fs::{self as unix, AtFlags, Dir, Mode, OFlags};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

const MAX_ENTRIES: usize = 1_000_000;
const MAX_DEPTH: usize = 128;

#[derive(Clone, Debug)]
pub struct Measurement {
    pub device: u64,
    pub inode: u64,
    pub bytes: u64,
    pub newest_modified_ns: i128,
    pub entries: usize,
    pub nested_git: bool,
    pub multiply_linked: bool,
}

pub fn open_directory(path: &Path) -> Result<File, ProtocolError> {
    if !path.is_absolute() {
        return Err(blocked("path_not_absolute"));
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
                fd = File::from(
                    unix::openat(
                        &fd,
                        name,
                        OFlags::RDONLY
                            | OFlags::DIRECTORY
                            | OFlags::NOFOLLOW
                            | OFlags::CLOEXEC
                            | OFlags::NONBLOCK,
                        Mode::empty(),
                    )
                    .map_err(|_| blocked("directory_identity_unavailable"))?,
                )
            }
            _ => return Err(blocked("path_traversal")),
        }
    }
    Ok(fd)
}

pub fn identity(path: &Path) -> Result<(u64, u64), ProtocolError> {
    let m = open_directory(path)?
        .metadata()
        .map_err(|_| unavailable("directory_metadata_unavailable"))?;
    Ok((m.dev(), m.ino()))
}

/// Only a successful lookup in a no-follow parent can prove absence. Permission
/// errors, an unavailable mount, and substituted symlinks never mean removed.
pub fn absent(path: &Path) -> Result<bool, ProtocolError> {
    let parent = path
        .parent()
        .ok_or_else(|| blocked("invalid_storage_path"))?;
    let name = path
        .file_name()
        .ok_or_else(|| blocked("invalid_storage_path"))?;
    let fd = open_directory(parent)?;
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
        multiply_linked: false,
    };
    walk_measure(&fd, 0, &mut value, &mut HashSet::new())?;
    Ok(value)
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
    remove_children(&child, expected.0, allow_git, 0)?;
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
    depth: usize,
) -> Result<(), ProtocolError> {
    if depth > MAX_DEPTH {
        return Err(blocked("directory_depth_limit"));
    }
    let entries = names(dir)?;
    if !allow_git && entries.iter().any(|n| n == ".git") {
        return Err(blocked("nested_repository"));
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
            remove_children(&child, device, allow_git, depth + 1)?;
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
