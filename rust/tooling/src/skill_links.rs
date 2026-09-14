//! Transactional installation of canonical skills and the universal policy.
//!
//! The library deliberately has no home-directory discovery and never invokes
//! a shell. Every repository, runtime root, policy target, and transaction
//! directory is supplied explicitly. POSIX mutations are protected by stable
//! directory identities and advisory directory locks; command renderings are
//! data for a human or calling installer, never executable strings.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

pub const SKILL_LINK_JOURNAL_VERSION: u64 = 3;
pub const SKILL_LINK_JOURNAL_NAME: &str = "journal.json";
pub const POLICY_SCHEMA_VERSION: u64 = 1;
pub const POLICY_MANAGER_VERSION: &str = "1.0.0";
pub const POLICY_MANAGER_NAME: &str = "devcoordinator2-universal-policy";
pub const POLICY_PLAN_NAME: &str = "plan.json";
pub const POLICY_JOURNAL_NAME: &str = "journal.json";
pub const POLICY_DIRECT_LINK_MODE: &str = "direct-absolute-symlink";
pub const POLICY_WINDOWS_WRAPPER_MODE: &str = "claude-absolute-import-wrapper-v1";
pub const CANONICAL_POLICY_RELATIVE: &str = "reference/universal/AGENTS.md";

const POLICY_BACKUP_MARKER: &str = "devcoordinator2-universal-policy-backup";
const POLICY_TEMP_MARKER: &str = "devcoordinator2-universal-policy-new";
const MAX_RESULT_ENTRIES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkErrorKind {
    InvalidInput,
    UnsafePath,
    UnsupportedPlatform,
    Drift,
    Conflict,
    InvalidTransaction,
    Io,
}

impl LinkErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::UnsafePath => "unsafe_path",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::Drift => "drift",
            Self::Conflict => "conflict",
            Self::InvalidTransaction => "invalid_transaction",
            Self::Io => "io_error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkError {
    pub kind: LinkErrorKind,
    pub message: String,
}

impl LinkError {
    fn new(kind: LinkErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn io(context: impl fmt::Display, error: io::Error) -> Self {
        Self::new(LinkErrorKind::Io, format!("{context}: {error}"))
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LinkError {}

pub type LinkResult<T> = Result<T, LinkError>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Identity {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
    size: u64,
    mtime_ns: i128,
}

impl Identity {
    fn compact_json(&self) -> Value {
        json!({
            "device": self.device,
            "inode": self.inode,
            "mode": self.mode,
        })
    }

    fn full_json(&self) -> Value {
        json!({
            "device": self.device,
            "inode": self.inode,
            "mode": self.mode,
            "uid": self.uid,
            "gid": self.gid,
            "nlink": self.nlink,
            "size": self.size,
            "mtime_ns": self.mtime_ns,
        })
    }

    fn from_full_json(value: &Value, label: &str) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                format!("invalid metadata in {label} snapshot"),
            )
        })?;
        let unsigned = |name: &str| -> LinkResult<u64> {
            object.get(name).and_then(Value::as_u64).ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    format!("invalid metadata in {label} snapshot"),
                )
            })
        };
        let mtime_ns = object
            .get("mtime_ns")
            .and_then(Value::as_i64)
            .map(i128::from)
            .or_else(|| {
                object
                    .get("mtime_ns")
                    .and_then(Value::as_u64)
                    .map(i128::from)
            })
            .ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    format!("invalid metadata in {label} snapshot"),
                )
            })?;
        Ok(Self {
            device: unsigned("device")?,
            inode: unsigned("inode")?,
            mode: u32::try_from(unsigned("mode")?).map_err(|_| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "metadata mode is too large",
                )
            })?,
            uid: u32::try_from(unsigned("uid")?).map_err(|_| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "metadata uid is too large",
                )
            })?,
            gid: u32::try_from(unsigned("gid")?).map_err(|_| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "metadata gid is too large",
                )
            })?,
            nlink: unsigned("nlink")?,
            size: unsigned("size")?,
            mtime_ns,
        })
    }
}

#[cfg(unix)]
fn identity(metadata: &fs::Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt;

    Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode() & 0o7777,
        uid: metadata.uid(),
        gid: metadata.gid(),
        nlink: metadata.nlink(),
        size: metadata.size(),
        mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
    }
}

#[cfg(not(unix))]
fn identity(metadata: &fs::Metadata) -> Identity {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| i128::try_from(value.as_nanos()).unwrap_or(i128::MAX))
        .unwrap_or_default();
    Identity {
        device: 0,
        inode: 0,
        mode: if metadata.permissions().readonly() {
            0o444
        } else {
            0o666
        },
        uid: 0,
        gid: 0,
        nlink: 1,
        size: metadata.len(),
        mtime_ns: modified,
    }
}

fn symlink_metadata(path: &Path, label: &str) -> LinkResult<fs::Metadata> {
    fs::symlink_metadata(path)
        .map_err(|error| LinkError::io(format!("cannot inspect {label} {}", path.display()), error))
}

fn lexical_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn normalize_absolute(path: &Path, label: &str) -> LinkResult<PathBuf> {
    let text = path.to_string_lossy();
    if text.is_empty() || text.chars().any(|character| character.is_control()) {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            format!("{label} contains an empty or control-character path"),
        ));
    }
    if !path.is_absolute() {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            format!("{label} must be an explicit absolute path: {text:?}"),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(LinkError::new(
                        LinkErrorKind::InvalidInput,
                        format!("{label} escapes its filesystem root"),
                    ));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn real_directory(path: &Path, label: &str) -> LinkResult<PathBuf> {
    let path = normalize_absolute(path, label)?;
    let metadata = symlink_metadata(&path, label)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!(
                "{label} must be a real directory, not a symlink: {}",
                path.display()
            ),
        ));
    }
    let resolved = fs::canonicalize(&path)
        .map_err(|error| LinkError::io(format!("{label} cannot be resolved safely"), error))?;
    if resolved != path {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!(
                "{label} must not contain symlinked path components: {}",
                path.display()
            ),
        ));
    }
    Ok(path)
}

fn is_within(path: &Path, parent: &Path) -> bool {
    path.starts_with(parent)
}

fn valid_leaf(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && Path::new(name).file_name().and_then(|value| value.to_str()) == Some(name)
        && Path::new(name).components().count() == 1
}

fn set_mode(path: &Path, mode: u32) -> LinkResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
            LinkError::io(
                format!("cannot set permissions on {}", path.display()),
                error,
            )
        })?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

fn unique_hex() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{now}:{}:{counter}", std::process::id());
    let digest = sha256(seed.as_bytes());
    hex(&digest[..16])
}

fn atomic_write(path: &Path, payload: &[u8], mode: u32) -> LinkResult<()> {
    let parent = path.parent().ok_or_else(|| {
        LinkError::new(LinkErrorKind::UnsafePath, "atomic write path has no parent")
    })?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::UnsafePath,
                "atomic write path has an unsafe name",
            )
        })?;
    let temporary = parent.join(format!(".{name}.tmp-{}", unique_hex()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| LinkError::io(format!("cannot create {}", temporary.display()), error))?;
    let result = (|| {
        file.write_all(payload).map_err(|error| {
            LinkError::io(format!("cannot write {}", temporary.display()), error)
        })?;
        file.sync_all().map_err(|error| {
            LinkError::io(format!("cannot sync {}", temporary.display()), error)
        })?;
        drop(file);
        fs::rename(&temporary, path).map_err(|error| {
            LinkError::io(
                format!("cannot atomically publish {}", path.display()),
                error,
            )
        })?;
        set_mode(path, mode)?;
        sync_directory(parent);
        Ok(())
    })();
    if result.is_err() && lexical_exists(&temporary) {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn atomic_write_at(
    directory: &File,
    directory_path: &Path,
    name: &str,
    payload: &[u8],
    mode: u32,
) -> LinkResult<()> {
    if !valid_leaf(name) {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            "atomic write child has an unsafe name",
        ));
    }
    let temporary_name = format!(".{name}.tmp-{}", unique_hex());
    let descriptor = rustix::fs::openat(
        directory,
        temporary_name.as_str(),
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(mode as _),
    )
    .map_err(|error| {
        LinkError::io(
            format!(
                "cannot create transaction child in {}",
                directory_path.display()
            ),
            error.into(),
        )
    })?;
    let mut file = File::from(descriptor);
    let result = (|| {
        file.write_all(payload)
            .map_err(|error| LinkError::io("cannot write transaction child", error))?;
        file.sync_all()
            .map_err(|error| LinkError::io("cannot sync transaction child", error))?;
        drop(file);
        rustix::fs::renameat(directory, temporary_name.as_str(), directory, name)
            .map_err(|error| LinkError::io("cannot publish transaction child", error.into()))?;
        directory.sync_all().ok();
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(
            directory,
            temporary_name.as_str(),
            rustix::fs::AtFlags::empty(),
        );
    }
    result
}

fn canonical_json(value: &Value) -> LinkResult<Vec<u8>> {
    let mut rendered = serde_json::to_string_pretty(value)
        .map_err(|error| LinkError::new(LinkErrorKind::InvalidTransaction, error.to_string()))?;
    rendered.push('\n');
    Ok(rendered.into_bytes())
}

fn read_json(path: &Path, label: &str) -> LinkResult<(Value, Vec<u8>)> {
    let metadata = symlink_metadata(path, label)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(LinkError::new(
            LinkErrorKind::InvalidTransaction,
            format!("{label} must be a real regular file: {}", path.display()),
        ));
    }
    if identity(&metadata).nlink != 1 {
        return Err(LinkError::new(
            LinkErrorKind::InvalidTransaction,
            format!("{label} must not be hard-linked: {}", path.display()),
        ));
    }
    let payload =
        fs::read(path).map_err(|error| LinkError::io(format!("cannot read {label}"), error))?;
    let value = serde_json::from_slice(&payload).map_err(|error| {
        LinkError::new(
            LinkErrorKind::InvalidTransaction,
            format!("{label} is malformed: {error}"),
        )
    })?;
    Ok((value, payload))
}

#[cfg(unix)]
struct DirectoryLock {
    path: PathBuf,
    file: File,
    expected: Identity,
}

#[cfg(unix)]
impl DirectoryLock {
    fn acquire(path: &Path, expected: &Identity) -> LinkResult<Self> {
        let file = File::open(path).map_err(|error| {
            LinkError::io(
                format!("cannot safely open target directory {}", path.display()),
                error,
            )
        })?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).map_err(|error| {
            LinkError::io(
                format!("cannot lock target directory {}", path.display()),
                error.into(),
            )
        })?;
        let lock = Self {
            path: path.to_path_buf(),
            file,
            expected: expected.clone(),
        };
        lock.revalidate()?;
        Ok(lock)
    }

    fn revalidate(&self) -> LinkResult<()> {
        let opened = identity(&self.file.metadata().map_err(|error| {
            LinkError::io(
                format!("cannot inspect locked directory {}", self.path.display()),
                error,
            )
        })?);
        let current_metadata = symlink_metadata(&self.path, "locked target directory")?;
        if current_metadata.file_type().is_symlink()
            || !current_metadata.is_dir()
            || opened.device != self.expected.device
            || opened.inode != self.expected.inode
            || opened.mode != self.expected.mode
            || identity(&current_metadata).device != self.expected.device
            || identity(&current_metadata).inode != self.expected.inode
            || identity(&current_metadata).mode != self.expected.mode
        {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "target directory identity changed while locked: {}",
                    self.path.display()
                ),
            ));
        }
        Ok(())
    }

    fn sync(&self) {
        let _ = self.file.sync_all();
    }
}

#[cfg(unix)]
impl Drop for DirectoryLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

#[cfg(not(unix))]
struct DirectoryLock {
    path: PathBuf,
    file: File,
    expected: Identity,
}

#[cfg(not(unix))]
impl DirectoryLock {
    fn acquire(path: &Path, expected: &Identity) -> LinkResult<Self> {
        let lock_path = path.join(".devcoordinator2-rust-link-manager.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)
            .map_err(|error| LinkError::io("cannot open runtime policy lock", error))?;
        let lock = Self {
            path: path.to_path_buf(),
            file,
            expected: expected.clone(),
        };
        lock.revalidate()?;
        Ok(lock)
    }

    fn revalidate(&self) -> LinkResult<()> {
        let current = identity(&symlink_metadata(&self.path, "target directory")?);
        if current != self.expected {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                "target directory identity changed while locked",
            ));
        }
        Ok(())
    }

    fn sync(&self) {
        let _ = self.file.sync_all();
    }
}

fn acquire_directory_locks(
    specifications: &[(PathBuf, Identity)],
) -> LinkResult<Vec<DirectoryLock>> {
    let mut unique = BTreeMap::<String, (PathBuf, Identity)>::new();
    for (path, expected) in specifications {
        let key = path.to_string_lossy().to_string();
        if let Some((_, prior)) = unique.get(&key)
            && prior != expected
        {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!("conflicting target-directory snapshots: {}", path.display()),
            ));
        }
        unique.insert(key, (path.clone(), expected.clone()));
    }
    unique
        .into_values()
        .map(|(path, identity)| DirectoryLock::acquire(&path, &identity))
        .collect()
}

#[cfg(unix)]
fn open_real_directory_descriptor(path: &Path, label: &str) -> LinkResult<File> {
    let before = symlink_metadata(path, label)?;
    if before.file_type().is_symlink() || !before.is_dir() {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!("{label} must be a real directory: {}", path.display()),
        ));
    }
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| LinkError::io(format!("cannot safely open {label}"), error.into()))?;
    let file = File::from(descriptor);
    let opened = identity(
        &file
            .metadata()
            .map_err(|error| LinkError::io(format!("cannot inspect {label}"), error))?,
    );
    let expected = identity(&before);
    if opened.device != expected.device || opened.inode != expected.inode {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            format!("{label} changed while it was opened: {}", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn rename_between_noreplace(
    source_directory: &File,
    source_name: &str,
    destination_directory: &File,
    destination_name: &str,
    destination_path: &Path,
) -> LinkResult<()> {
    if !valid_leaf(source_name) || !valid_leaf(destination_name) {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            "unsafe adjacent transaction name",
        ));
    }
    rustix::fs::renameat_with(
        source_directory,
        source_name,
        destination_directory,
        destination_name,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| {
        if lexical_exists(destination_path) {
            LinkError::new(
                LinkErrorKind::Conflict,
                format!(
                    "atomic no-replace move refused a destination collision: {}",
                    destination_path.display()
                ),
            )
        } else {
            LinkError::io("atomic no-replace move failed", error.into())
        }
    })
}

#[cfg(unix)]
fn rename_noreplace(
    lock: &DirectoryLock,
    source_name: &str,
    destination_name: &str,
) -> LinkResult<()> {
    if !valid_leaf(source_name) || !valid_leaf(destination_name) {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            "unsafe adjacent transaction name",
        ));
    }
    lock.revalidate()?;
    if let Err(error) = rustix::fs::renameat_with(
        &lock.file,
        source_name,
        &lock.file,
        destination_name,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        if lexical_exists(&lock.path.join(destination_name)) {
            return Err(LinkError::new(
                LinkErrorKind::Conflict,
                format!(
                    "atomic no-replace move refused a destination collision: {}",
                    lock.path.join(destination_name).display()
                ),
            ));
        }
        return Err(LinkError::io("atomic no-replace move failed", error.into()));
    }
    lock.sync();
    lock.revalidate()?;
    Ok(())
}

#[cfg(not(unix))]
fn rename_noreplace(
    lock: &DirectoryLock,
    source_name: &str,
    destination_name: &str,
) -> LinkResult<()> {
    let source = lock.path.join(source_name);
    let destination = lock.path.join(destination_name);
    if lexical_exists(&destination) {
        return Err(LinkError::new(
            LinkErrorKind::Conflict,
            format!(
                "atomic no-replace move refused a destination collision: {}",
                destination.display()
            ),
        ));
    }
    fs::rename(source, destination)
        .map_err(|error| LinkError::io("no-replace move failed", error))?;
    lock.sync();
    lock.revalidate()
}

#[derive(Clone)]
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    length_bits: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0; 64],
            buffer_len: 0,
            length_bits: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length_bits = self.length_bits.wrapping_add(
            u64::try_from(data.len())
                .unwrap_or(u64::MAX)
                .wrapping_mul(8),
        );
        if self.buffer_len > 0 {
            let take = (64 - self.buffer_len).min(data.len());
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&data[..take]);
            self.buffer_len += take;
            data = &data[take..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffer_len = 0;
            }
        }
        while data.len() >= 64 {
            let block: &[u8; 64] = data[..64].try_into().expect("fixed block");
            self.compress(block);
            data = &data[64..];
        }
        self.buffer[..data.len()].copy_from_slice(data);
        self.buffer_len = data.len();
    }

    fn finalize(mut self) -> [u8; 32] {
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;
        if self.buffer_len > 56 {
            self.buffer[self.buffer_len..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer = [0; 64];
        } else {
            self.buffer[self.buffer_len..56].fill(0);
        }
        self.buffer[56..].copy_from_slice(&self.length_bits.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);
        let mut output = [0; 32];
        for (index, word) in self.state.iter().enumerate() {
            output[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        output
    }

    fn compress(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut words = [0u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(chunk.try_into().expect("word"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let big_s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temporary1 = h
                .wrapping_add(big_s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let big_s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temporary2 = big_s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temporary1);
            d = c;
            c = b;
            b = a;
            a = temporary1.wrapping_add(temporary2);
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(bytes);
    digest.finalize()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        value.to_string_lossy().as_bytes().to_vec()
    }
}

fn collect_tree(root: &Path, directory: &Path, paths: &mut Vec<PathBuf>) -> LinkResult<()> {
    let mut children = fs::read_dir(directory)
        .map_err(|error| LinkError::io(format!("cannot read {}", directory.display()), error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| LinkError::io(format!("cannot read {}", directory.display()), error))?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let path = child.path();
        let relative = path.strip_prefix(root).map_err(|_| {
            LinkError::new(LinkErrorKind::UnsafePath, "tree entry escaped its root")
        })?;
        if relative
            .components()
            .any(|component| component.as_os_str() == "__pycache__")
            || path.extension().is_some_and(|extension| extension == "pyc")
            || path.file_name().is_some_and(|name| name == ".DS_Store")
        {
            continue;
        }
        paths.push(path.clone());
        let metadata = symlink_metadata(&path, "tree entry")?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_tree(root, &path, paths)?;
        }
    }
    Ok(())
}

/// Hash names, object kinds, modes, link text, and bytes without following links.
pub fn tree_digest(root: &Path) -> LinkResult<String> {
    let root_metadata = symlink_metadata(root, "tree root")?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!("tree root must be a real directory: {}", root.display()),
        ));
    }
    let mut paths = vec![root.to_path_buf()];
    collect_tree(root, root, &mut paths)?;
    paths[1..].sort_by_key(|path| {
        path.strip_prefix(root)
            .unwrap_or(path)
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    });

    let mut digest = Sha256::new();
    for path in paths {
        let metadata = symlink_metadata(&path, "tree entry")?;
        let relative = if path == root {
            b".".to_vec()
        } else {
            let relative = path.strip_prefix(root).map_err(|_| {
                LinkError::new(LinkErrorKind::UnsafePath, "tree entry escaped its root")
            })?;
            relative
                .components()
                .enumerate()
                .fold(Vec::new(), |mut output, (index, component)| {
                    if index > 0 {
                        output.push(b'/');
                    }
                    output.extend(os_bytes(component.as_os_str()));
                    output
                })
        };
        let (kind, content) = if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).map_err(|error| {
                LinkError::io(format!("cannot read link {}", path.display()), error)
            })?;
            (b"link".as_slice(), os_bytes(target.as_os_str()))
        } else if metadata.is_dir() {
            (b"directory".as_slice(), Vec::new())
        } else if metadata.is_file() {
            let content = fs::read(&path)
                .map_err(|error| LinkError::io(format!("cannot read {}", path.display()), error))?;
            (b"file".as_slice(), content)
        } else {
            (b"other".as_slice(), Vec::new())
        };
        let mode = identity(&metadata).mode.to_string();
        for part in [
            relative.as_slice(),
            kind,
            mode.as_bytes(),
            content.as_slice(),
        ] {
            digest.update(part);
            digest.update(&[0]);
        }
    }
    Ok(hex(&digest.finalize()))
}

fn file_digest(path: &Path) -> LinkResult<String> {
    let before = symlink_metadata(path, "regular file")?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!("cannot safely hash non-regular file: {}", path.display()),
        ));
    }
    let before_identity = identity(&before);
    let mut file = File::open(path)
        .map_err(|error| LinkError::io(format!("cannot safely open {}", path.display()), error))?;
    let opened_identity = identity(
        &file
            .metadata()
            .map_err(|error| LinkError::io("cannot inspect opened regular file", error))?,
    );
    if opened_identity.device != before_identity.device
        || opened_identity.inode != before_identity.inode
        || !before.is_file()
    {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            format!(
                "regular file changed while it was opened: {}",
                path.display()
            ),
        ));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| LinkError::io(format!("cannot read {}", path.display()), error))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let after_identity = identity(
        &file
            .metadata()
            .map_err(|error| LinkError::io("cannot reinspect opened regular file", error))?,
    );
    if after_identity != opened_identity {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            format!(
                "regular file changed while it was hashed: {}",
                path.display()
            ),
        ));
    }
    Ok(hex(&hash.finalize()))
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SkillInstallationStatus {
    Missing,
    CopiedMatch,
    DirectLink,
    DivergentDirectory,
    ChainedLink,
    BrokenLink,
    NoncanonicalLink,
    UnexpectedFile,
    UnexpectedObject,
}

impl SkillInstallationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::CopiedMatch => "copied_match",
            Self::DirectLink => "direct_link",
            Self::DivergentDirectory => "divergent_directory",
            Self::ChainedLink => "chained_link",
            Self::BrokenLink => "broken_link",
            Self::NoncanonicalLink => "noncanonical_link",
            Self::UnexpectedFile => "unexpected_file",
            Self::UnexpectedObject => "unexpected_object",
        }
    }

    fn from_str(value: &str) -> LinkResult<Self> {
        match value {
            "missing" => Ok(Self::Missing),
            "copied_match" => Ok(Self::CopiedMatch),
            "direct_link" => Ok(Self::DirectLink),
            "divergent_directory" => Ok(Self::DivergentDirectory),
            "chained_link" => Ok(Self::ChainedLink),
            "broken_link" => Ok(Self::BrokenLink),
            "noncanonical_link" => Ok(Self::NoncanonicalLink),
            "unexpected_file" => Ok(Self::UnexpectedFile),
            "unexpected_object" => Ok(Self::UnexpectedObject),
            _ => Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                format!("invalid installation status: {value}"),
            )),
        }
    }

    fn safe_without_override(self) -> bool {
        matches!(self, Self::Missing | Self::CopiedMatch | Self::DirectLink)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillSourceSnapshot {
    pub repository_device: u64,
    pub repository_inode: u64,
    pub skills_device: u64,
    pub skills_inode: u64,
    pub source_device: u64,
    pub source_inode: u64,
    pub digest: String,
}

impl SkillSourceSnapshot {
    fn to_json(&self) -> Value {
        json!({
            "repository_device": self.repository_device,
            "repository_inode": self.repository_inode,
            "skills_device": self.skills_device,
            "skills_inode": self.skills_inode,
            "source_device": self.source_device,
            "source_inode": self.source_inode,
            "digest": self.digest,
        })
    }

    fn from_json(value: &Value) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal has an invalid canonical source snapshot",
            )
        })?;
        let get = |name: &str| {
            object.get(name).and_then(Value::as_u64).ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "transaction journal has an invalid canonical source snapshot",
                )
            })
        };
        let digest = object
            .get("digest")
            .and_then(Value::as_str)
            .filter(|value| is_hex(value, 64))
            .ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "transaction journal has an invalid canonical source snapshot",
                )
            })?;
        Ok(Self {
            repository_device: get("repository_device")?,
            repository_inode: get("repository_inode")?,
            skills_device: get("skills_device")?,
            skills_inode: get("skills_inode")?,
            source_device: get("source_device")?,
            source_inode: get("source_inode")?,
            digest: digest.to_owned(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillInstallationState {
    pub status: SkillInstallationStatus,
    pub link_target: Option<String>,
    pub resolved_link_target: Option<String>,
    pub digest: Option<String>,
    pub object_type: Option<u32>,
    pub object_mode: Option<u32>,
    pub object_device: Option<u64>,
}

impl SkillInstallationState {
    fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("status".into(), Value::String(self.status.as_str().into()));
        if let Some(value) = &self.link_target {
            object.insert("link_target".into(), Value::String(value.clone()));
        }
        if let Some(value) = &self.resolved_link_target {
            object.insert("resolved_link_target".into(), Value::String(value.clone()));
        }
        if let Some(value) = &self.digest {
            object.insert("digest".into(), Value::String(value.clone()));
        }
        if let Some(value) = self.object_type {
            object.insert("object_type".into(), Value::from(value));
        }
        if let Some(value) = self.object_mode {
            object.insert("object_mode".into(), Value::from(value));
        }
        if let Some(value) = self.object_device {
            object.insert("object_device".into(), Value::from(value));
        }
        Value::Object(object)
    }

    fn from_json(value: &Value) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(LinkErrorKind::InvalidTransaction, "invalid before snapshot")
        })?;
        let status = SkillInstallationStatus::from_str(
            object
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )?;
        Ok(Self {
            status,
            link_target: object
                .get("link_target")
                .and_then(Value::as_str)
                .map(str::to_owned),
            resolved_link_target: object
                .get("resolved_link_target")
                .and_then(Value::as_str)
                .map(str::to_owned),
            digest: object
                .get("digest")
                .and_then(Value::as_str)
                .map(str::to_owned),
            object_type: object
                .get("object_type")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            object_mode: object
                .get("object_mode")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            object_device: object.get("object_device").and_then(Value::as_u64),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillLinkEntry {
    pub root_index: usize,
    pub target_root: PathBuf,
    pub skill: String,
    pub destination: PathBuf,
    pub source: PathBuf,
    pub source_snapshot: SkillSourceSnapshot,
    pub installation: SkillInstallationState,
}

impl SkillLinkEntry {
    fn to_json(&self) -> Value {
        let mut object = self
            .installation
            .to_json()
            .as_object()
            .cloned()
            .unwrap_or_default();
        object.insert("root_index".into(), Value::from(self.root_index));
        object.insert(
            "target_root".into(),
            Value::String(self.target_root.to_string_lossy().into_owned()),
        );
        object.insert("skill".into(), Value::String(self.skill.clone()));
        object.insert(
            "destination".into(),
            Value::String(self.destination.to_string_lossy().into_owned()),
        );
        object.insert(
            "source".into(),
            Value::String(self.source.to_string_lossy().into_owned()),
        );
        object.insert("source_snapshot".into(), self.source_snapshot.to_json());
        Value::Object(object)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillLinkPlan {
    pub repository_root: PathBuf,
    pub target_roots: Vec<PathBuf>,
    pub skills: Vec<String>,
    pub entries: Vec<SkillLinkEntry>,
}

impl SkillLinkPlan {
    pub fn to_json(&self) -> Value {
        json!({
            "version": SKILL_LINK_JOURNAL_VERSION,
            "repository_root": self.repository_root.to_string_lossy(),
            "target_roots": self.target_roots.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),
            "skills": self.skills,
            "entries": self.entries.iter().map(SkillLinkEntry::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn is_verified(&self) -> bool {
        self.entries
            .iter()
            .all(|entry| entry.installation.status == SkillInstallationStatus::DirectLink)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillLinkVerification {
    pub plan: SkillLinkPlan,
    pub failures: Vec<PathBuf>,
}

impl SkillLinkVerification {
    pub fn is_verified(&self) -> bool {
        self.failures.is_empty()
    }
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_skill_repository(repository: &Path) -> LinkResult<PathBuf> {
    let repository = real_directory(repository, "repository root")?;
    let skills = repository.join("skills");
    let skills_metadata =
        symlink_metadata(&skills, "repository skills directory").map_err(|error| {
            if error.kind == LinkErrorKind::Io {
                LinkError::new(
                    LinkErrorKind::UnsafePath,
                    format!(
                        "repository has no skills directory: {}",
                        repository.display()
                    ),
                )
            } else {
                error
            }
        })?;
    if skills_metadata.file_type().is_symlink() || !skills_metadata.is_dir() {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!(
                "repository skills directory must be a real in-repository directory: {}",
                skills.display()
            ),
        ));
    }
    Ok(repository)
}

fn canonical_skill_roots(repository: &Path, roots: &[PathBuf]) -> LinkResult<Vec<PathBuf>> {
    if roots.is_empty() {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            "at least one explicit --target-root is required",
        ));
    }
    let mut result = Vec::with_capacity(roots.len());
    let mut seen = BTreeSet::new();
    for root in roots {
        let root = real_directory(root, "target root")?;
        if is_within(&root, repository) || is_within(repository, &root) {
            return Err(LinkError::new(
                LinkErrorKind::UnsafePath,
                format!(
                    "target root must be outside the canonical repository: {}",
                    root.display()
                ),
            ));
        }
        if !seen.insert(root.clone()) {
            return Err(LinkError::new(
                LinkErrorKind::InvalidInput,
                format!("duplicate target root: {}", root.display()),
            ));
        }
        result.push(root);
    }
    for (index, root) in result.iter().enumerate() {
        for other in result.iter().skip(index + 1) {
            if is_within(root, other) || is_within(other, root) {
                return Err(LinkError::new(
                    LinkErrorKind::UnsafePath,
                    format!(
                        "target roots must not be nested: {} and {}",
                        root.display(),
                        other.display()
                    ),
                ));
            }
        }
    }
    Ok(result)
}

fn managed_skills(repository: &Path, selected: Option<&[String]>) -> LinkResult<Vec<String>> {
    let skills_root = repository.join("skills");
    let mut paths = Vec::new();
    collect_tree(&skills_root, &skills_root, &mut paths)?;
    let mut symlinks = paths
        .iter()
        .filter(|path| {
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        })
        .map(|path| {
            path.strip_prefix(repository)
                .unwrap_or(path)
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/")
        })
        .collect::<Vec<_>>();
    symlinks.sort();
    if !symlinks.is_empty() {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!(
                "canonical skills tree must not contain symlinks: {}",
                symlinks.join(", ")
            ),
        ));
    }

    let mut available = BTreeSet::new();
    for entry in fs::read_dir(&skills_root)
        .map_err(|error| LinkError::io("cannot read canonical skills directory", error))?
    {
        let entry = entry.map_err(|error| LinkError::io("cannot read skill entry", error))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| LinkError::io("cannot inspect skill entry", error))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let skill_file = entry.path().join("SKILL.md");
        let Ok(skill_metadata) = fs::symlink_metadata(&skill_file) else {
            continue;
        };
        if skill_metadata.is_file()
            && !skill_metadata.file_type().is_symlink()
            && let Some(name) = entry.file_name().to_str()
        {
            available.insert(name.to_owned());
        }
    }
    if let Some(selected) = selected
        && !selected.is_empty()
    {
        let names = selected.iter().cloned().collect::<BTreeSet<_>>();
        let invalid = names
            .iter()
            .filter(|name| !valid_leaf(name))
            .cloned()
            .collect::<Vec<_>>();
        if !invalid.is_empty() {
            return Err(LinkError::new(
                LinkErrorKind::InvalidInput,
                format!("invalid skill names: {}", invalid.join(", ")),
            ));
        }
        let missing = names.difference(&available).cloned().collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(LinkError::new(
                LinkErrorKind::InvalidInput,
                format!(
                    "skills are not canonical repository skills: {}",
                    missing.join(", ")
                ),
            ));
        }
        return Ok(names.into_iter().collect());
    }
    if available.is_empty() {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            format!("no canonical skills found under {}", skills_root.display()),
        ));
    }
    Ok(available.into_iter().collect())
}

fn skill_source_snapshot(repository: &Path, skill: &str) -> LinkResult<SkillSourceSnapshot> {
    let checked = canonical_skill_repository(repository)?;
    if checked != repository {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            format!(
                "canonical repository identity changed: {}",
                repository.display()
            ),
        ));
    }
    if managed_skills(&checked, Some(&[skill.to_owned()]))? != [skill] {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            format!("canonical skill is unavailable: {skill}"),
        ));
    }
    let skills = repository.join("skills");
    let source = skills.join(skill);
    let repository_identity = identity(&symlink_metadata(repository, "repository root")?);
    let skills_identity = identity(&symlink_metadata(&skills, "skills directory")?);
    let source_identity = identity(&symlink_metadata(&source, "canonical skill")?);
    Ok(SkillSourceSnapshot {
        repository_device: repository_identity.device,
        repository_inode: repository_identity.inode,
        skills_device: skills_identity.device,
        skills_inode: skills_identity.inode,
        source_device: source_identity.device,
        source_inode: source_identity.inode,
        digest: tree_digest(&source)?,
    })
}

fn require_skill_source_snapshot(source: &Path, expected: &SkillSourceSnapshot) -> LinkResult<()> {
    let repository = source.parent().and_then(Path::parent).ok_or_else(|| {
        LinkError::new(
            LinkErrorKind::UnsafePath,
            "canonical skill path is malformed",
        )
    })?;
    let skill = source
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::UnsafePath,
                "canonical skill name is not Unicode",
            )
        })?;
    let actual = skill_source_snapshot(repository, skill).map_err(|error| {
        LinkError::new(
            LinkErrorKind::Drift,
            format!(
                "canonical source changed after planning: {}: {error}",
                source.display()
            ),
        )
    })?;
    if &actual != expected {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            format!(
                "canonical source identity or content changed after planning: {}",
                source.display()
            ),
        ));
    }
    Ok(())
}

fn absolute_link_matches(destination: &Path, source: &Path) -> bool {
    fs::symlink_metadata(destination).is_ok_and(|metadata| metadata.file_type().is_symlink())
        && fs::read_link(destination).is_ok_and(|target| target == source && target.is_absolute())
}

fn classify_skill_installation(
    destination: &Path,
    source: &Path,
    source_snapshot: &SkillSourceSnapshot,
) -> LinkResult<SkillInstallationState> {
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SkillInstallationState {
                status: SkillInstallationStatus::Missing,
                link_target: None,
                resolved_link_target: None,
                digest: None,
                object_type: None,
                object_mode: None,
                object_device: None,
            });
        }
        Err(error) => {
            return Err(LinkError::io(
                format!("cannot inspect {}", destination.display()),
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        let raw = fs::read_link(destination).map_err(|error| {
            LinkError::io(format!("cannot read link {}", destination.display()), error)
        })?;
        let target = if raw.is_absolute() {
            raw.clone()
        } else {
            destination.parent().unwrap_or(Path::new("/")).join(&raw)
        };
        let resolved = fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
        let status = if raw == source
            && target == source
            && absolute_link_matches(destination, source)
            && require_skill_source_snapshot(source, source_snapshot).is_ok()
        {
            SkillInstallationStatus::DirectLink
        } else if fs::symlink_metadata(&target)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            SkillInstallationStatus::ChainedLink
        } else if !target.exists() {
            SkillInstallationStatus::BrokenLink
        } else {
            SkillInstallationStatus::NoncanonicalLink
        };
        return Ok(SkillInstallationState {
            status,
            link_target: Some(raw.to_string_lossy().into_owned()),
            resolved_link_target: Some(resolved.to_string_lossy().into_owned()),
            digest: None,
            object_type: None,
            object_mode: None,
            object_device: None,
        });
    }
    if metadata.is_dir() {
        let digest = tree_digest(destination)?;
        return Ok(SkillInstallationState {
            status: if digest == source_snapshot.digest {
                SkillInstallationStatus::CopiedMatch
            } else {
                SkillInstallationStatus::DivergentDirectory
            },
            link_target: None,
            resolved_link_target: None,
            digest: Some(digest),
            object_type: None,
            object_mode: None,
            object_device: None,
        });
    }
    if metadata.is_file() {
        return Ok(SkillInstallationState {
            status: SkillInstallationStatus::UnexpectedFile,
            link_target: None,
            resolved_link_target: None,
            digest: Some(file_digest(destination)?),
            object_type: None,
            object_mode: None,
            object_device: None,
        });
    }
    let detail = identity(&metadata);
    Ok(SkillInstallationState {
        status: SkillInstallationStatus::UnexpectedObject,
        link_target: None,
        resolved_link_target: None,
        digest: None,
        object_type: Some(file_type_bits(&metadata)),
        object_mode: Some(detail.mode),
        object_device: Some(object_device(&metadata)),
    })
}

#[cfg(unix)]
fn file_type_bits(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode() & 0o170000
}

#[cfg(not(unix))]
fn file_type_bits(_: &fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn object_device(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.rdev()
}

#[cfg(not(unix))]
fn object_device(_: &fs::Metadata) -> u64 {
    0
}

/// Inspect only canonical skill names across every explicitly supplied runtime root.
pub fn build_skill_link_plan(
    repository: &Path,
    target_roots: &[PathBuf],
    selected_skills: Option<&[String]>,
) -> LinkResult<SkillLinkPlan> {
    let repository = canonical_skill_repository(repository)?;
    let target_roots = canonical_skill_roots(&repository, target_roots)?;
    let skills = managed_skills(&repository, selected_skills)?;
    let mut snapshots = BTreeMap::new();
    for skill in &skills {
        snapshots.insert(skill.clone(), skill_source_snapshot(&repository, skill)?);
    }
    let mut entries = Vec::new();
    for (root_index, target_root) in target_roots.iter().enumerate() {
        for skill in &skills {
            let source = repository.join("skills").join(skill);
            let source_snapshot = snapshots[skill].clone();
            let destination = target_root.join(skill);
            entries.push(SkillLinkEntry {
                root_index,
                target_root: target_root.clone(),
                skill: skill.clone(),
                installation: classify_skill_installation(&destination, &source, &source_snapshot)?,
                destination,
                source,
                source_snapshot,
            });
        }
    }
    if entries.len() > MAX_RESULT_ENTRIES {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            format!("skill-link plan exceeds the bounded {MAX_RESULT_ENTRIES}-entry limit"),
        ));
    }
    Ok(SkillLinkPlan {
        repository_root: repository,
        target_roots,
        skills,
        entries,
    })
}

/// Check mode: inspect the complete explicit runtime matrix without mutation.
pub fn verify_skill_links(
    repository: &Path,
    target_roots: &[PathBuf],
    selected_skills: Option<&[String]>,
) -> LinkResult<SkillLinkVerification> {
    let plan = build_skill_link_plan(repository, target_roots, selected_skills)?;
    let failures = plan
        .entries
        .iter()
        .filter(|entry| entry.installation.status != SkillInstallationStatus::DirectLink)
        .map(|entry| entry.destination.clone())
        .collect();
    Ok(SkillLinkVerification { plan, failures })
}

pub fn render_skill_link_plan(plan: &SkillLinkPlan) -> String {
    let mut lines = vec![format!(
        "canonical repository: {}",
        plan.repository_root.display()
    )];
    let mut counts = BTreeMap::<&str, usize>::new();
    for entry in &plan.entries {
        lines.push(format!(
            "{:<21} {}",
            entry.installation.status.as_str(),
            entry.destination.display()
        ));
        *counts
            .entry(entry.installation.status.as_str())
            .or_default() += 1;
    }
    lines.push(format!(
        "summary: {}",
        counts
            .into_iter()
            .map(|(status, count)| format!("{status}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    lines.join("\n")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RootIdentity {
    device: u64,
    inode: u64,
}

impl RootIdentity {
    fn from_identity(identity: &Identity) -> Self {
        Self {
            device: identity.device,
            inode: identity.inode,
        }
    }

    fn to_json(&self) -> Value {
        json!({"device": self.device, "inode": self.inode})
    }

    fn from_json(value: &Value) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal contains an invalid target-root identity",
            )
        })?;
        Ok(Self {
            device: object
                .get("device")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    LinkError::new(
                        LinkErrorKind::InvalidTransaction,
                        "transaction journal contains an invalid target-root identity",
                    )
                })?,
            inode: object.get("inode").and_then(Value::as_u64).ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "transaction journal contains an invalid target-root identity",
                )
            })?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SkillJournalEntry {
    root_index: usize,
    target_root: PathBuf,
    skill: String,
    destination: PathBuf,
    source: PathBuf,
    source_snapshot: Option<SkillSourceSnapshot>,
    backup: PathBuf,
    before: SkillInstallationState,
    stage: String,
}

impl SkillJournalEntry {
    fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("root_index".into(), Value::from(self.root_index));
        object.insert(
            "target_root".into(),
            Value::String(self.target_root.to_string_lossy().into_owned()),
        );
        object.insert("skill".into(), Value::String(self.skill.clone()));
        object.insert(
            "destination".into(),
            Value::String(self.destination.to_string_lossy().into_owned()),
        );
        object.insert(
            "source".into(),
            Value::String(self.source.to_string_lossy().into_owned()),
        );
        if let Some(snapshot) = &self.source_snapshot {
            object.insert("source_snapshot".into(), snapshot.to_json());
        }
        object.insert(
            "backup".into(),
            Value::String(self.backup.to_string_lossy().into_owned()),
        );
        object.insert("before".into(), self.before.to_json());
        object.insert("stage".into(), Value::String(self.stage.clone()));
        Value::Object(object)
    }

    fn from_json(
        value: &Value,
        version: u64,
        repository: &Path,
        roots: &[PathBuf],
        transaction: &Path,
    ) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal contains an invalid entry",
            )
        })?;
        let root_index = object
            .get("root_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value < roots.len())
            .ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "transaction journal contains an invalid skill or root index",
                )
            })?;
        let string = |name: &str| -> LinkResult<String> {
            object
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    LinkError::new(
                        LinkErrorKind::InvalidTransaction,
                        format!("transaction journal entry lacks {name}"),
                    )
                })
        };
        let skill = string("skill")?;
        if !valid_leaf(&skill) {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal contains an invalid skill or root index",
            ));
        }
        let target_root = PathBuf::from(string("target_root")?);
        let destination = PathBuf::from(string("destination")?);
        let source = PathBuf::from(string("source")?);
        let backup = PathBuf::from(string("backup")?);
        let expected_root = &roots[root_index];
        if &target_root != expected_root || destination != expected_root.join(&skill) {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal destination escaped its target root",
            ));
        }
        if source != repository.join("skills").join(&skill) {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal source escaped the canonical skills directory",
            ));
        }
        if backup
            != transaction
                .join("backups")
                .join(format!("root-{root_index}"))
                .join(&skill)
        {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal backup escaped its transaction directory",
            ));
        }
        let source_snapshot = if version >= 3 {
            Some(SkillSourceSnapshot::from_json(
                object.get("source_snapshot").unwrap_or(&Value::Null),
            )?)
        } else {
            None
        };
        Ok(Self {
            root_index,
            target_root,
            skill,
            destination,
            source,
            source_snapshot,
            backup,
            before: SkillInstallationState::from_json(
                object.get("before").unwrap_or(&Value::Null),
            )?,
            stage: string("stage")?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SkillJournalDocument {
    version: u64,
    status: String,
    repository_root: PathBuf,
    target_roots: Vec<PathBuf>,
    target_root_identities: Vec<RootIdentity>,
    skills: Vec<String>,
    entries: Vec<SkillJournalEntry>,
    error: Option<String>,
}

impl SkillJournalDocument {
    fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("version".into(), Value::from(self.version));
        object.insert("status".into(), Value::String(self.status.clone()));
        object.insert(
            "repository_root".into(),
            Value::String(self.repository_root.to_string_lossy().into_owned()),
        );
        object.insert(
            "target_roots".into(),
            Value::Array(
                self.target_roots
                    .iter()
                    .map(|path| Value::String(path.to_string_lossy().into_owned()))
                    .collect(),
            ),
        );
        object.insert(
            "target_root_identities".into(),
            Value::Array(
                self.target_root_identities
                    .iter()
                    .map(RootIdentity::to_json)
                    .collect(),
            ),
        );
        object.insert(
            "skills".into(),
            Value::Array(self.skills.iter().cloned().map(Value::String).collect()),
        );
        object.insert(
            "entries".into(),
            Value::Array(
                self.entries
                    .iter()
                    .map(SkillJournalEntry::to_json)
                    .collect(),
            ),
        );
        if let Some(error) = &self.error {
            object.insert("error".into(), Value::String(error.clone()));
        }
        Value::Object(object)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillLinkTransaction {
    pub status: String,
    pub transaction_dir: PathBuf,
    pub changed_entries: usize,
    pub document: Value,
}

impl SkillLinkTransaction {
    fn from_document(transaction_dir: PathBuf, document: &SkillJournalDocument) -> Self {
        Self {
            status: document.status.clone(),
            transaction_dir,
            changed_entries: document.entries.len(),
            document: document.to_json(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillLinkApplyOptions {
    pub selected_skills: Option<Vec<String>>,
    pub allow_noncanonical: bool,
    /// Test-only deterministic fault point retained from the Python oracle.
    pub failure_after_links: Option<usize>,
}

fn prepare_skill_transaction(
    transaction: &Path,
    repository: &Path,
    roots: &[PathBuf],
) -> LinkResult<PathBuf> {
    let transaction = normalize_absolute(transaction, "--transaction-dir")?;
    if lexical_exists(&transaction) {
        return Err(LinkError::new(
            LinkErrorKind::Conflict,
            format!(
                "transaction directory already exists; refusing partial/stale transaction: {}",
                transaction.display()
            ),
        ));
    }
    let parent = transaction.parent().ok_or_else(|| {
        LinkError::new(
            LinkErrorKind::UnsafePath,
            "transaction directory has no parent",
        )
    })?;
    let parent = real_directory(parent, "transaction parent")?;
    let transaction = parent.join(transaction.file_name().ok_or_else(|| {
        LinkError::new(
            LinkErrorKind::UnsafePath,
            "transaction directory has no safe leaf",
        )
    })?);
    if is_within(&transaction, repository) || roots.iter().any(|root| is_within(&transaction, root))
    {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            "transaction directory must be outside the repository and all target roots",
        ));
    }
    let parent_identity = identity(&symlink_metadata(&parent, "transaction parent")?);
    let different = roots
        .iter()
        .filter(|root| {
            identity(&fs::symlink_metadata(root).expect("validated root")).device
                != parent_identity.device
        })
        .collect::<Vec<_>>();
    if !different.is_empty() {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!(
                "transaction directory must share a filesystem with every target root so backups use atomic rename: {}",
                different
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    fs::create_dir(&transaction).map_err(|error| {
        LinkError::io(
            format!("cannot create transaction {}", transaction.display()),
            error,
        )
    })?;
    set_mode(&transaction, 0o700)?;
    fs::create_dir(transaction.join("backups"))
        .map_err(|error| LinkError::io("cannot create transaction backups", error))?;
    set_mode(&transaction.join("backups"), 0o700)?;
    sync_directory(&parent);
    Ok(transaction)
}

fn save_skill_journal(transaction: &Path, journal: &SkillJournalDocument) -> LinkResult<()> {
    atomic_write(
        &transaction.join(SKILL_LINK_JOURNAL_NAME),
        &canonical_json(&journal.to_json())?,
        0o600,
    )
}

fn load_skill_journal(transaction: &Path) -> LinkResult<(PathBuf, SkillJournalDocument)> {
    let transaction = real_directory(transaction, "transaction directory")?;
    let (value, _) = read_json(
        &transaction.join(SKILL_LINK_JOURNAL_NAME),
        "transaction journal",
    )?;
    let object = value.as_object().ok_or_else(|| {
        LinkError::new(
            LinkErrorKind::InvalidTransaction,
            "transaction journal must be a JSON object",
        )
    })?;
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if !matches!(version, 2 | 3) {
        return Err(LinkError::new(
            LinkErrorKind::InvalidTransaction,
            "unsupported or missing transaction journal version",
        ));
    }
    let repository_root = PathBuf::from(
        object
            .get("repository_root")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let target_roots = object
        .get("target_roots")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal has invalid repository or target roots",
            )
        })?
        .iter()
        .map(|value| {
            value.as_str().map(PathBuf::from).ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "transaction journal has invalid repository or target roots",
                )
            })
        })
        .collect::<LinkResult<Vec<_>>>()?;
    if !repository_root.is_absolute() || target_roots.is_empty() {
        return Err(LinkError::new(
            LinkErrorKind::InvalidTransaction,
            "transaction journal has invalid repository or target roots",
        ));
    }
    let target_root_identities = object
        .get("target_root_identities")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal has invalid target-root identities",
            )
        })?
        .iter()
        .map(RootIdentity::from_json)
        .collect::<LinkResult<Vec<_>>>()?;
    if target_root_identities.len() != target_roots.len() {
        return Err(LinkError::new(
            LinkErrorKind::InvalidTransaction,
            "transaction journal has invalid target-root identities",
        ));
    }
    let skills = object
        .get("skills")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let entries = object
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal entries are invalid",
            )
        })?
        .iter()
        .map(|entry| {
            SkillJournalEntry::from_json(
                entry,
                version,
                &repository_root,
                &target_roots,
                &transaction,
            )
        })
        .collect::<LinkResult<Vec<_>>>()?;
    Ok((
        transaction,
        SkillJournalDocument {
            version,
            status: object
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            repository_root,
            target_roots,
            target_root_identities,
            skills,
            entries,
            error: object
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
    ))
}

fn state_matches(path: &Path, before: &SkillInstallationState) -> bool {
    match before.status {
        SkillInstallationStatus::Missing => !lexical_exists(path),
        SkillInstallationStatus::CopiedMatch | SkillInstallationStatus::DivergentDirectory => {
            fs::symlink_metadata(path).is_ok_and(|metadata| {
                metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && tree_digest(path).ok().as_ref() == before.digest.as_ref()
            })
        }
        SkillInstallationStatus::DirectLink
        | SkillInstallationStatus::ChainedLink
        | SkillInstallationStatus::BrokenLink
        | SkillInstallationStatus::NoncanonicalLink => {
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
                && fs::read_link(path)
                    .ok()
                    .as_ref()
                    .map(|target| target.to_string_lossy().into_owned())
                    .as_ref()
                    == before.link_target.as_ref()
        }
        SkillInstallationStatus::UnexpectedFile => {
            fs::symlink_metadata(path).is_ok_and(|metadata| {
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && file_digest(path).ok().as_ref() == before.digest.as_ref()
            })
        }
        SkillInstallationStatus::UnexpectedObject => {
            fs::symlink_metadata(path).is_ok_and(|metadata| {
                file_type_bits(&metadata) == before.object_type.unwrap_or_default()
                    && identity(&metadata).mode == before.object_mode.unwrap_or_default()
                    && object_device(&metadata) == before.object_device.unwrap_or_default()
            })
        }
    }
}

#[cfg(not(unix))]
fn remove_one(path: &Path) -> LinkResult<()> {
    let metadata = symlink_metadata(path, "rollback target")?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
            .map_err(|error| LinkError::io(format!("cannot remove {}", path.display()), error))
    } else {
        fs::remove_file(path)
            .map_err(|error| LinkError::io(format!("cannot remove {}", path.display()), error))
    }
}

#[cfg(unix)]
fn remove_child_at(parent: &File, name: &std::ffi::CStr) -> LinkResult<()> {
    let metadata = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| LinkError::io("cannot inspect rollback target", error.into()))?;
    if rustix::fs::FileType::from_raw_mode(metadata.st_mode) == rustix::fs::FileType::Directory {
        let descriptor = rustix::fs::openat(
            parent,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| {
            LinkError::io(
                "cannot safely open rollback directory without following links",
                error.into(),
            )
        })?;
        let directory = File::from(descriptor);
        let entries = rustix::fs::Dir::read_from(&directory)
            .map_err(|error| LinkError::io("cannot enumerate rollback directory", error.into()))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| LinkError::io("cannot enumerate rollback entry", error.into()))?;
            let child = entry.file_name();
            if child.to_bytes() == b"." || child.to_bytes() == b".." {
                continue;
            }
            remove_child_at(&directory, child)?;
        }
        directory.sync_all().ok();
        rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::REMOVEDIR)
            .map_err(|error| LinkError::io("cannot remove rollback directory", error.into()))?;
    } else {
        rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty())
            .map_err(|error| LinkError::io("cannot remove rollback object", error.into()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn remove_one_locked(lock: &DirectoryLock, name: &str) -> LinkResult<()> {
    use std::os::unix::ffi::OsStrExt;

    if !valid_leaf(name) {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            "unsafe rollback target name",
        ));
    }
    lock.revalidate()?;
    let name = std::ffi::CString::new(Path::new(name).as_os_str().as_bytes()).map_err(|_| {
        LinkError::new(
            LinkErrorKind::UnsafePath,
            "rollback name contains a NUL byte",
        )
    })?;
    remove_child_at(&lock.file, &name)?;
    lock.sync();
    lock.revalidate()
}

#[cfg(not(unix))]
fn remove_one_locked(lock: &DirectoryLock, name: &str) -> LinkResult<()> {
    remove_one(&lock.path.join(name))
}

#[cfg(all(unix, test))]
fn create_file_symlink(source: &Path, destination: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, destination)
}

#[cfg(windows)]
fn create_file_symlink(source: &Path, destination: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(source, destination)
}

#[cfg(unix)]
fn create_file_symlink_at(
    lock: &DirectoryLock,
    source: &Path,
    destination_name: &str,
) -> LinkResult<()> {
    if !valid_leaf(destination_name) {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            "unsafe symlink destination name",
        ));
    }
    lock.revalidate()?;
    rustix::fs::symlinkat(source, &lock.file, destination_name).map_err(|error| {
        LinkError::io(
            format!("cannot create direct link in {}", lock.path.display()),
            error.into(),
        )
    })?;
    lock.sync();
    lock.revalidate()
}

#[cfg(not(unix))]
fn create_file_symlink_at(
    lock: &DirectoryLock,
    source: &Path,
    destination_name: &str,
) -> LinkResult<()> {
    create_file_symlink(source, &lock.path.join(destination_name))
        .map_err(|error| LinkError::io("cannot create direct link", error))
}

fn lock_for<'a>(locks: &'a [DirectoryLock], path: &Path) -> LinkResult<&'a DirectoryLock> {
    locks.iter().find(|lock| lock.path == path).ok_or_else(|| {
        LinkError::new(
            LinkErrorKind::InvalidTransaction,
            "target root is not locked",
        )
    })
}

fn rollback_skill_locked(
    transaction: &Path,
    journal: &mut SkillJournalDocument,
    locks: &[DirectoryLock],
    force: bool,
) -> LinkResult<()> {
    if journal.status == "rolled_back" {
        return Ok(());
    }
    for entry in journal.entries.iter().rev() {
        if entry.stage == "concurrent_change_preserved" {
            continue;
        }
        let lock = lock_for(locks, &entry.target_root)?;
        lock.revalidate()?;
        let backup_exists = lexical_exists(&entry.backup);
        let destination_exists = lexical_exists(&entry.destination);
        if backup_exists && !state_matches(&entry.backup, &entry.before) {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "transaction backup no longer matches its recorded pre-apply state: {}",
                    entry.backup.display()
                ),
            ));
        }
        if backup_exists
            && destination_exists
            && !force
            && !absolute_link_matches(&entry.destination, &entry.source)
        {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "refusing to overwrite a post-apply change during rollback: {}",
                    entry.destination.display()
                ),
            ));
        }
        if !backup_exists && entry.before.status == SkillInstallationStatus::Missing {
            if destination_exists
                && !force
                && !absolute_link_matches(&entry.destination, &entry.source)
            {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "refusing to remove a post-apply change during rollback: {}",
                        entry.destination.display()
                    ),
                ));
            }
        } else if !backup_exists
            && !matches!(entry.stage.as_str(), "pending" | "prepared" | "rolled_back")
            && !state_matches(&entry.destination, &entry.before)
        {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                format!(
                    "transaction backup is missing for {}; manual recovery required",
                    entry.destination.display()
                ),
            ));
        }
    }

    journal.status = "rolling_back".into();
    save_skill_journal(transaction, journal)?;
    for index in (0..journal.entries.len()).rev() {
        if journal.entries[index].stage == "concurrent_change_preserved" {
            continue;
        }
        let entry = journal.entries[index].clone();
        let lock = lock_for(locks, &entry.target_root)?;
        lock.revalidate()?;
        if lexical_exists(&entry.backup) {
            if lexical_exists(&entry.destination) {
                remove_one_locked(lock, &entry.skill)?;
            }
            #[cfg(unix)]
            {
                let backup_parent = entry.backup.parent().ok_or_else(|| {
                    LinkError::new(LinkErrorKind::UnsafePath, "backup has no parent")
                })?;
                let backup_directory =
                    open_real_directory_descriptor(backup_parent, "skill backup directory")?;
                rename_between_noreplace(
                    &backup_directory,
                    &entry.skill,
                    &lock.file,
                    &entry.skill,
                    &entry.destination,
                )?;
                backup_directory.sync_all().ok();
            }
            #[cfg(not(unix))]
            fs::rename(&entry.backup, &entry.destination).map_err(|error| {
                LinkError::io(
                    format!("cannot restore {}", entry.destination.display()),
                    error,
                )
            })?;
            lock.sync();
        } else if entry.before.status == SkillInstallationStatus::Missing
            && lexical_exists(&entry.destination)
        {
            remove_one_locked(lock, &entry.skill)?;
            lock.sync();
        }
        if !state_matches(&entry.destination, &entry.before) {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "rollback did not restore the recorded pre-apply state: {}",
                    entry.destination.display()
                ),
            ));
        }
        journal.entries[index].stage = "rolled_back".into();
        save_skill_journal(transaction, journal)?;
    }
    journal.status = "rolled_back".into();
    save_skill_journal(transaction, journal)
}

/// Replace the managed skill matrix with direct absolute links.
pub fn apply_skill_links(
    repository: &Path,
    target_roots: &[PathBuf],
    transaction: &Path,
    options: &SkillLinkApplyOptions,
) -> LinkResult<SkillLinkTransaction> {
    let repository = canonical_skill_repository(repository)?;
    let roots = canonical_skill_roots(&repository, target_roots)?;
    let identities = roots
        .iter()
        .map(|root| symlink_metadata(root, "target root").map(|metadata| identity(&metadata)))
        .collect::<LinkResult<Vec<_>>>()?;
    let lock_specs = roots
        .iter()
        .cloned()
        .zip(identities.iter().cloned())
        .collect::<Vec<_>>();
    let locks = acquire_directory_locks(&lock_specs)?;
    let plan = build_skill_link_plan(&repository, &roots, options.selected_skills.as_deref())?;
    let unsafe_entries = plan
        .entries
        .iter()
        .filter(|entry| !entry.installation.status.safe_without_override())
        .collect::<Vec<_>>();
    if !unsafe_entries.is_empty() && !options.allow_noncanonical {
        return Err(LinkError::new(
            LinkErrorKind::Conflict,
            format!(
                "refusing divergent, broken, chained, or unexpected installations without --allow-noncanonical after review: {}",
                unsafe_entries
                    .iter()
                    .map(|entry| format!(
                        "{}:{}",
                        entry.installation.status.as_str(),
                        entry.destination.display()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let transaction = prepare_skill_transaction(transaction, &repository, &roots)?;
    for lock in &locks {
        lock.revalidate()?;
    }
    let mut journal = SkillJournalDocument {
        version: SKILL_LINK_JOURNAL_VERSION,
        status: "prepared".into(),
        repository_root: repository,
        target_roots: roots,
        target_root_identities: identities.iter().map(RootIdentity::from_identity).collect(),
        skills: plan.skills.clone(),
        entries: plan
            .entries
            .iter()
            .filter(|entry| entry.installation.status != SkillInstallationStatus::DirectLink)
            .map(|entry| SkillJournalEntry {
                root_index: entry.root_index,
                target_root: entry.target_root.clone(),
                skill: entry.skill.clone(),
                destination: entry.destination.clone(),
                source: entry.source.clone(),
                source_snapshot: Some(entry.source_snapshot.clone()),
                backup: transaction
                    .join("backups")
                    .join(format!("root-{}", entry.root_index))
                    .join(&entry.skill),
                before: entry.installation.clone(),
                stage: "pending".into(),
            })
            .collect(),
        error: None,
    };
    save_skill_journal(&transaction, &journal)?;
    journal.status = "applying".into();
    save_skill_journal(&transaction, &journal)?;

    let result = (|| {
        for entry in &plan.entries {
            require_skill_source_snapshot(&entry.source, &entry.source_snapshot)?;
        }
        for (linked, index) in (0..journal.entries.len()).enumerate() {
            let entry = journal.entries[index].clone();
            let lock = lock_for(&locks, &entry.target_root)?;
            lock.revalidate()?;
            let backup_parent = entry
                .backup
                .parent()
                .ok_or_else(|| LinkError::new(LinkErrorKind::UnsafePath, "backup has no parent"))?;
            fs::create_dir_all(backup_parent)
                .map_err(|error| LinkError::io("cannot create skill backup directory", error))?;
            set_mode(backup_parent, 0o700)?;
            journal.entries[index].stage = "prepared".into();
            save_skill_journal(&transaction, &journal)?;
            if !state_matches(&entry.destination, &entry.before) {
                journal.entries[index].stage = "concurrent_change_preserved".into();
                save_skill_journal(&transaction, &journal)?;
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "installation changed after planning; refusing to move it: {}",
                        entry.destination.display()
                    ),
                ));
            }
            if lexical_exists(&entry.destination) {
                if lexical_exists(&entry.backup) {
                    return Err(LinkError::new(
                        LinkErrorKind::Conflict,
                        format!(
                            "transaction backup path unexpectedly exists: {}",
                            entry.backup.display()
                        ),
                    ));
                }
                #[cfg(unix)]
                {
                    let backup_directory =
                        open_real_directory_descriptor(backup_parent, "skill backup directory")?;
                    rename_between_noreplace(
                        &lock.file,
                        &entry.skill,
                        &backup_directory,
                        &entry.skill,
                        &entry.backup,
                    )?;
                    backup_directory.sync_all().ok();
                }
                #[cfg(not(unix))]
                fs::rename(&entry.destination, &entry.backup).map_err(|error| {
                    LinkError::io(
                        format!("cannot preserve {}", entry.destination.display()),
                        error,
                    )
                })?;
                lock.sync();
                sync_directory(backup_parent);
                if !state_matches(&entry.backup, &entry.before) {
                    if !lexical_exists(&entry.destination) {
                        #[cfg(unix)]
                        if let Ok(backup_directory) =
                            open_real_directory_descriptor(backup_parent, "skill backup directory")
                        {
                            let _ = rename_between_noreplace(
                                &backup_directory,
                                &entry.skill,
                                &lock.file,
                                &entry.skill,
                                &entry.destination,
                            );
                        }
                        #[cfg(not(unix))]
                        let _ = fs::rename(&entry.backup, &entry.destination);
                    }
                    journal.entries[index].stage = "concurrent_change_preserved".into();
                    save_skill_journal(&transaction, &journal)?;
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "installation changed during backup; preserved it: {}",
                            entry.destination.display()
                        ),
                    ));
                }
                journal.entries[index].stage = "backup_moved".into();
                save_skill_journal(&transaction, &journal)?;
            }
            if let Some(snapshot) = &entry.source_snapshot {
                require_skill_source_snapshot(&entry.source, snapshot)?;
            }
            let temporary_name = format!(".{}.devcoordinator2-agent-{}", entry.skill, unique_hex());
            let temporary = entry.target_root.join(&temporary_name);
            let install_result = (|| {
                create_file_symlink_at(lock, &entry.source, &temporary_name)?;
                rename_noreplace(lock, &temporary_name, &entry.skill)?;
                Ok(())
            })();
            if install_result.is_err() && lexical_exists(&temporary) {
                let _ = fs::remove_file(&temporary);
            }
            install_result?;
            journal.entries[index].stage = "linked".into();
            save_skill_journal(&transaction, &journal)?;
            lock.revalidate()?;
            if !absolute_link_matches(&entry.destination, &entry.source)
                || entry.source_snapshot.as_ref().is_some_and(|snapshot| {
                    require_skill_source_snapshot(&entry.source, snapshot).is_err()
                })
            {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "direct link verification failed immediately after apply: {}",
                        entry.destination.display()
                    ),
                ));
            }
            if options
                .failure_after_links
                .is_some_and(|limit| linked + 1 >= limit)
            {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    "injected partial apply failure",
                ));
            }
        }
        for entry in &plan.entries {
            let lock = lock_for(&locks, &entry.target_root)?;
            lock.revalidate()?;
            if !absolute_link_matches(&entry.destination, &entry.source)
                || require_skill_source_snapshot(&entry.source, &entry.source_snapshot).is_err()
            {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "final direct link verification failed: {}",
                        entry.destination.display()
                    ),
                ));
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        journal.status = "rollback_pending".into();
        journal.error = Some(format!("LinkError: {error}"));
        save_skill_journal(&transaction, &journal)?;
        if let Err(rollback_error) =
            rollback_skill_locked(&transaction, &mut journal, &locks, false)
        {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "apply failed ({error}); automatic rollback also failed ({rollback_error}); inspect {}",
                    transaction.display()
                ),
            ));
        }
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            format!("apply failed and was rolled back: {error}"),
        ));
    }
    journal.status = "applied".into();
    save_skill_journal(&transaction, &journal)?;
    Ok(SkillLinkTransaction::from_document(transaction, &journal))
}

/// Restore the exact objects captured by a skill-link transaction.
pub fn rollback_skill_links(transaction: &Path, force: bool) -> LinkResult<SkillLinkTransaction> {
    let (transaction, mut journal) = load_skill_journal(transaction)?;
    if journal.status == "rolled_back" {
        return Ok(SkillLinkTransaction::from_document(transaction, &journal));
    }
    let lock_specs = journal
        .target_roots
        .iter()
        .zip(&journal.target_root_identities)
        .map(|(root, expected)| {
            let metadata = symlink_metadata(root, "target root").map_err(|error| {
                LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "cannot safely open target root for locking: {}: {error}",
                        root.display()
                    ),
                )
            })?;
            let actual = identity(&metadata);
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || actual.device != expected.device
                || actual.inode != expected.inode
            {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "target root identity changed before lock acquisition: {}",
                        root.display()
                    ),
                ));
            }
            Ok((root.clone(), actual))
        })
        .collect::<LinkResult<Vec<_>>>()?;
    let locks = acquire_directory_locks(&lock_specs)?;
    rollback_skill_locked(&transaction, &mut journal, &locks, force)?;
    Ok(SkillLinkTransaction::from_document(transaction, &journal))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostFamily {
    Windows,
    Macos,
    Linux,
    Wsl,
    PosixOther,
}

impl HostFamily {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Wsl => "wsl",
            Self::PosixOther => "posix-other",
        }
    }

    fn from_str(value: &str) -> LinkResult<Self> {
        match value {
            "windows" => Ok(Self::Windows),
            "macos" => Ok(Self::Macos),
            "linux" => Ok(Self::Linux),
            "wsl" => Ok(Self::Wsl),
            "posix-other" => Ok(Self::PosixOther),
            _ => Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan platform family is invalid",
            )),
        }
    }

    pub fn detect() -> Self {
        #[cfg(windows)]
        {
            return Self::Windows;
        }
        #[cfg(target_os = "macos")]
        {
            return Self::Macos;
        }
        #[cfg(target_os = "linux")]
        {
            let release = fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if release.contains("microsoft") {
                Self::Wsl
            } else {
                Self::Linux
            }
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            Self::PosixOther
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PolicyRole {
    Codex,
    Claude,
}

impl PolicyRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    fn from_str(value: &str) -> LinkResult<Self> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            _ => Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan target role is invalid",
            )),
        }
    }

    const fn required_name(self) -> &'static str {
        match self {
            Self::Codex => "AGENTS.md",
            Self::Claude => "CLAUDE.md",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyMode {
    DirectAbsoluteSymlink,
    ClaudeWindowsImportWrapper,
}

impl PolicyMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectAbsoluteSymlink => POLICY_DIRECT_LINK_MODE,
            Self::ClaudeWindowsImportWrapper => POLICY_WINDOWS_WRAPPER_MODE,
        }
    }

    fn from_str(value: &str) -> LinkResult<Self> {
        match value {
            POLICY_DIRECT_LINK_MODE => Ok(Self::DirectAbsoluteSymlink),
            POLICY_WINDOWS_WRAPPER_MODE => Ok(Self::ClaudeWindowsImportWrapper),
            _ => Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan target mode is invalid",
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyTarget {
    pub role: PolicyRole,
    pub mode: PolicyMode,
    pub destination: PathBuf,
}

impl PolicyTarget {
    pub fn codex(destination: impl Into<PathBuf>) -> Self {
        Self {
            role: PolicyRole::Codex,
            mode: PolicyMode::DirectAbsoluteSymlink,
            destination: destination.into(),
        }
    }

    pub fn claude(destination: impl Into<PathBuf>) -> Self {
        Self {
            role: PolicyRole::Claude,
            mode: PolicyMode::DirectAbsoluteSymlink,
            destination: destination.into(),
        }
    }

    pub fn claude_windows_wrapper(destination: impl Into<PathBuf>) -> Self {
        Self {
            role: PolicyRole::Claude,
            mode: PolicyMode::ClaudeWindowsImportWrapper,
            destination: destination.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PolicySnapshot {
    Missing,
    RegularFile {
        sha256: String,
        metadata: Identity,
    },
    Symlink {
        link_text: String,
        metadata: Identity,
    },
}

impl PolicySnapshot {
    fn kind(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::RegularFile { .. } => "regular-file",
            Self::Symlink { .. } => "symlink",
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Missing => json!({"kind": "missing"}),
            Self::RegularFile { sha256, metadata } => json!({
                "kind": "regular-file",
                "sha256": sha256,
                "metadata": metadata.full_json(),
            }),
            Self::Symlink {
                link_text,
                metadata,
            } => json!({
                "kind": "symlink",
                "link_text": link_text,
                "metadata": metadata.full_json(),
            }),
        }
    }

    fn from_json(value: &Value, label: &str) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                format!("invalid {label} snapshot"),
            )
        })?;
        match object.get("kind").and_then(Value::as_str) {
            Some("missing") if object.len() == 1 => Ok(Self::Missing),
            Some("regular-file") => {
                let digest = object
                    .get("sha256")
                    .and_then(Value::as_str)
                    .filter(|value| is_hex(value, 64))
                    .ok_or_else(|| {
                        LinkError::new(
                            LinkErrorKind::InvalidTransaction,
                            format!("invalid digest in {label} snapshot"),
                        )
                    })?;
                Ok(Self::RegularFile {
                    sha256: digest.to_owned(),
                    metadata: Identity::from_full_json(
                        object.get("metadata").unwrap_or(&Value::Null),
                        label,
                    )?,
                })
            }
            Some("symlink") => Ok(Self::Symlink {
                link_text: object
                    .get("link_text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        LinkError::new(
                            LinkErrorKind::InvalidTransaction,
                            format!("invalid link text in {label} snapshot"),
                        )
                    })?
                    .to_owned(),
                metadata: Identity::from_full_json(
                    object.get("metadata").unwrap_or(&Value::Null),
                    label,
                )?,
            }),
            _ => Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                format!("invalid {label} snapshot"),
            )),
        }
    }
}

fn policy_snapshot(path: &Path, allow_missing: bool) -> LinkResult<PolicySnapshot> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing => {
            return Ok(PolicySnapshot::Missing);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(LinkError::new(
                LinkErrorKind::UnsafePath,
                format!("required path is missing: {}", path.display()),
            ));
        }
        Err(error) => {
            return Err(LinkError::io(
                format!("cannot inspect {}", path.display()),
                error,
            ));
        }
    };
    if before.file_type().is_symlink() {
        let link = fs::read_link(path).map_err(|error| {
            LinkError::io(format!("cannot read symlink {}", path.display()), error)
        })?;
        let after = fs::symlink_metadata(path).map_err(|error| {
            LinkError::io(
                format!("cannot reinspect symlink {}", path.display()),
                error,
            )
        })?;
        if identity(&before) != identity(&after) {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!("symlink changed while it was inspected: {}", path.display()),
            ));
        }
        return Ok(PolicySnapshot::Symlink {
            link_text: link.to_string_lossy().into_owned(),
            metadata: identity(&after),
        });
    }
    if before.is_file() {
        let metadata = identity(&before);
        if metadata.nlink != 1 {
            return Err(LinkError::new(
                LinkErrorKind::UnsafePath,
                format!(
                    "regular-file target must not be hard-linked: {}",
                    path.display()
                ),
            ));
        }
        let digest = file_digest(path)?;
        let after = fs::symlink_metadata(path).map_err(|error| {
            LinkError::io(format!("cannot reinspect {}", path.display()), error)
        })?;
        if identity(&after) != metadata {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "regular file changed while it was inspected: {}",
                    path.display()
                ),
            ));
        }
        return Ok(PolicySnapshot::RegularFile {
            sha256: digest,
            metadata,
        });
    }
    let kind = if before.is_dir() {
        "directory"
    } else {
        "special-object"
    };
    Err(LinkError::new(
        LinkErrorKind::UnsafePath,
        format!(
            "unsupported {kind} target; only missing files, files, and symlinks are safe: {}",
            path.display()
        ),
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PolicySourceNode {
    relative: String,
    identity: Identity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PolicySourceSnapshot {
    path: PathBuf,
    nodes: Vec<PolicySourceNode>,
    policy: PolicySnapshot,
}

impl PolicySourceSnapshot {
    fn to_json(&self) -> Value {
        json!({
            "path": self.path.to_string_lossy(),
            "nodes": self.nodes.iter().map(|node| json!({
                "relative": node.relative,
                "identity": node.identity.compact_json(),
            })).collect::<Vec<_>>(),
            "policy": self.policy.to_json(),
        })
    }

    fn from_json(value: &Value) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan canonical source is malformed",
            )
        })?;
        let path = PathBuf::from(
            object
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let nodes = object
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "plan canonical source nodes are malformed",
                )
            })?
            .iter()
            .map(|value| {
                let node = value.as_object().ok_or_else(|| {
                    LinkError::new(
                        LinkErrorKind::InvalidTransaction,
                        "plan canonical source node is malformed",
                    )
                })?;
                let compact = node
                    .get("identity")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        LinkError::new(
                            LinkErrorKind::InvalidTransaction,
                            "plan canonical source node identity is malformed",
                        )
                    })?;
                let get = |name: &str| {
                    compact.get(name).and_then(Value::as_u64).ok_or_else(|| {
                        LinkError::new(
                            LinkErrorKind::InvalidTransaction,
                            "plan canonical source node identity is malformed",
                        )
                    })
                };
                Ok(PolicySourceNode {
                    relative: node
                        .get("relative")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    identity: Identity {
                        device: get("device")?,
                        inode: get("inode")?,
                        mode: u32::try_from(get("mode")?).map_err(|_| {
                            LinkError::new(
                                LinkErrorKind::InvalidTransaction,
                                "source node mode is too large",
                            )
                        })?,
                        uid: 0,
                        gid: 0,
                        nlink: 0,
                        size: 0,
                        mtime_ns: 0,
                    },
                })
            })
            .collect::<LinkResult<Vec<_>>>()?;
        Ok(Self {
            path,
            nodes,
            policy: PolicySnapshot::from_json(
                object.get("policy").unwrap_or(&Value::Null),
                "canonical policy",
            )?,
        })
    }
}

fn canonical_policy_repository(repository: &Path) -> LinkResult<PathBuf> {
    let repository = real_directory(repository, "repository root")?;
    let source = repository.join(CANONICAL_POLICY_RELATIVE);
    let parent = source.parent().expect("canonical relative has parent");
    real_directory(parent, "canonical policy directory")?;
    let metadata = symlink_metadata(&source, "canonical policy")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!(
                "canonical policy must be one real regular file: {}",
                source.display()
            ),
        ));
    }
    if identity(&metadata).nlink != 1 {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!(
                "canonical policy must not be hard-linked: {}",
                source.display()
            ),
        ));
    }
    Ok(repository)
}

fn policy_source_snapshot(repository: &Path) -> LinkResult<PolicySourceSnapshot> {
    let repository = canonical_policy_repository(repository)?;
    let mut nodes = Vec::new();
    for relative in [".", "reference", "reference/universal"] {
        let path = if relative == "." {
            repository.clone()
        } else {
            repository.join(relative)
        };
        real_directory(&path, &format!("canonical source component {relative}"))?;
        let actual = identity(&symlink_metadata(&path, "canonical source component")?);
        nodes.push(PolicySourceNode {
            relative: relative.into(),
            // The established artifact contract records only stable directory
            // identity and mode for source components.
            identity: Identity {
                device: actual.device,
                inode: actual.inode,
                mode: actual.mode,
                uid: 0,
                gid: 0,
                nlink: 0,
                size: 0,
                mtime_ns: 0,
            },
        });
    }
    let path = repository.join(CANONICAL_POLICY_RELATIVE);
    let policy = policy_snapshot(&path, false)?;
    Ok(PolicySourceSnapshot {
        path,
        nodes,
        policy,
    })
}

fn require_policy_source_snapshot(
    repository: &Path,
    expected: &PolicySourceSnapshot,
) -> LinkResult<()> {
    let actual = policy_source_snapshot(repository).map_err(|error| {
        LinkError::new(
            LinkErrorKind::Drift,
            format!("canonical policy changed after planning: {error}"),
        )
    })?;
    if &actual != expected {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            format!(
                "canonical policy identity or bytes changed after planning: {}",
                repository.join(CANONICAL_POLICY_RELATIVE).display()
            ),
        ));
    }
    Ok(())
}

/// Normalize a local drive-absolute Windows path for Claude's import syntax.
pub fn serialize_windows_import_path(raw: &str) -> LinkResult<String> {
    if raw.is_empty() || raw.chars().any(|character| character.is_control()) {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            "Windows canonical policy path contains an empty or control-character path",
        ));
    }
    if raw.starts_with("\\\\") || raw.starts_with("//") {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            "Windows import wrappers reject UNC/network policy sources",
        ));
    }
    let normalized = raw.replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() < 4
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'/'
        || bytes[3..].is_empty()
    {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            format!("Windows import path must be drive-absolute: {raw:?}"),
        ));
    }
    let mut output = normalized;
    output.replace_range(0..1, &output[0..1].to_ascii_uppercase());
    Ok(output)
}

/// Produce the one-line Claude external import; it contains no authority text.
pub fn claude_wrapper_bytes(source: &str, windows: bool) -> LinkResult<Vec<u8>> {
    if source.is_empty() || source.chars().any(|character| character.is_control()) {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            "canonical policy path contains an empty or control-character path",
        ));
    }
    let serialized = if windows {
        serialize_windows_import_path(source)?
    } else {
        let source = Path::new(source);
        if !source.is_absolute() {
            return Err(LinkError::new(
                LinkErrorKind::InvalidInput,
                "Claude import wrapper source must be absolute",
            ));
        }
        source
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/")
    };
    Ok(format!("@{serialized}\n").into_bytes())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DesiredPolicy {
    Symlink {
        link_text: String,
    },
    RegularFile {
        format: String,
        sha256: String,
        size: u64,
    },
}

impl DesiredPolicy {
    fn to_json(&self) -> Value {
        match self {
            Self::Symlink { link_text } => {
                json!({"kind": "symlink", "link_text": link_text})
            }
            Self::RegularFile {
                format,
                sha256,
                size,
            } => json!({
                "kind": "regular-file",
                "format": format,
                "sha256": sha256,
                "size": size,
            }),
        }
    }

    fn from_json(value: &Value) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan desired state is invalid",
            )
        })?;
        match object.get("kind").and_then(Value::as_str) {
            Some("symlink") => Ok(Self::Symlink {
                link_text: object
                    .get("link_text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }),
            Some("regular-file") => Ok(Self::RegularFile {
                format: object
                    .get("format")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                sha256: object
                    .get("sha256")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                size: object
                    .get("size")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            }),
            _ => Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan desired state is invalid",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PolicyAction {
    Retain,
    Replace,
}

impl PolicyAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Retain => "retain",
            Self::Replace => "replace",
        }
    }

    fn from_str(value: &str) -> LinkResult<Self> {
        match value {
            "retain" => Ok(Self::Retain),
            "replace" => Ok(Self::Replace),
            _ => Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan target action is invalid",
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PolicyPlanEntry {
    id: String,
    order: usize,
    role: PolicyRole,
    mode: PolicyMode,
    destination: PathBuf,
    parent: PathBuf,
    parent_identity: Identity,
    before: PolicySnapshot,
    desired: DesiredPolicy,
    backup: PathBuf,
    temporary: PathBuf,
    action: PolicyAction,
}

impl PolicyPlanEntry {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "order": self.order,
            "role": self.role.as_str(),
            "mode": self.mode.as_str(),
            "destination": self.destination.to_string_lossy(),
            "parent": self.parent.to_string_lossy(),
            "parent_identity": self.parent_identity.compact_json(),
            "before": self.before.to_json(),
            "desired": self.desired.to_json(),
            "backup": self.backup.to_string_lossy(),
            "temporary": self.temporary.to_string_lossy(),
            "action": self.action.as_str(),
        })
    }

    fn from_json(value: &Value, index: usize) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan target ordering or ID is invalid",
            )
        })?;
        let string = |name: &str| -> LinkResult<String> {
            object
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    LinkError::new(
                        LinkErrorKind::InvalidTransaction,
                        format!("plan target {name} is invalid"),
                    )
                })
        };
        let id = string("id")?;
        let order = object
            .get("order")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(usize::MAX);
        if id != format!("target-{:03}", index + 1) || order != index {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan target ordering or ID is invalid",
            ));
        }
        let compact = object
            .get("parent_identity")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "plan target-parent identity is missing",
                )
            })?;
        let identity_value = |name: &str| {
            compact.get(name).and_then(Value::as_u64).ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "plan target-parent identity is missing",
                )
            })
        };
        Ok(Self {
            id,
            order,
            role: PolicyRole::from_str(&string("role")?)?,
            mode: PolicyMode::from_str(&string("mode")?)?,
            destination: PathBuf::from(string("destination")?),
            parent: PathBuf::from(string("parent")?),
            parent_identity: Identity {
                device: identity_value("device")?,
                inode: identity_value("inode")?,
                mode: u32::try_from(identity_value("mode")?).map_err(|_| {
                    LinkError::new(
                        LinkErrorKind::InvalidTransaction,
                        "plan target-parent mode is invalid",
                    )
                })?,
                uid: 0,
                gid: 0,
                nlink: 0,
                size: 0,
                mtime_ns: 0,
            },
            before: PolicySnapshot::from_json(
                object.get("before").unwrap_or(&Value::Null),
                "before",
            )?,
            desired: DesiredPolicy::from_json(object.get("desired").unwrap_or(&Value::Null))?,
            backup: PathBuf::from(string("backup")?),
            temporary: PathBuf::from(string("temporary")?),
            action: PolicyAction::from_str(&string("action")?)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyPlan {
    pub plan_id: String,
    pub platform: HostFamily,
    pub repository_root: PathBuf,
    canonical_source: PolicySourceSnapshot,
    pub transaction_dir: PathBuf,
    transaction_identity: Identity,
    entries: Vec<PolicyPlanEntry>,
}

impl PolicyPlan {
    pub fn target_count(&self) -> usize {
        self.entries.len()
    }

    pub fn targets(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .map(|entry| entry.destination.clone())
            .collect()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "schema_version": POLICY_SCHEMA_VERSION,
            "manager": POLICY_MANAGER_NAME,
            "manager_version": POLICY_MANAGER_VERSION,
            "plan_id": self.plan_id,
            "platform": {
                "family": self.platform.as_str(),
                "os_name": std::env::consts::OS,
                "python": "not-used",
                "rust": env!("CARGO_PKG_VERSION"),
            },
            "repository_root": self.repository_root.to_string_lossy(),
            "canonical_source": self.canonical_source.to_json(),
            "transaction_dir": self.transaction_dir.to_string_lossy(),
            "transaction_identity": self.transaction_identity.compact_json(),
            "entries": self.entries.iter().map(PolicyPlanEntry::to_json).collect::<Vec<_>>(),
        })
    }

    fn from_json(value: &Value, transaction: &Path) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction plan must contain a JSON object",
            )
        })?;
        if object.get("schema_version").and_then(Value::as_u64) != Some(POLICY_SCHEMA_VERSION)
            || object.get("manager").and_then(Value::as_str) != Some(POLICY_MANAGER_NAME)
        {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "unsupported or foreign global-policy plan",
            ));
        }
        let plan_id = object
            .get("plan_id")
            .and_then(Value::as_str)
            .filter(|value| is_hex(value, 32))
            .ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "invalid global-policy plan ID",
                )
            })?
            .to_owned();
        let recorded_transaction = PathBuf::from(
            object
                .get("transaction_dir")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        if recorded_transaction != transaction {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan was moved away from its bound transaction directory",
            ));
        }
        let compact = object
            .get("transaction_identity")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "plan transaction identity is malformed",
                )
            })?;
        let get_identity = |name: &str| {
            compact.get(name).and_then(Value::as_u64).ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "plan transaction identity is malformed",
                )
            })
        };
        let transaction_identity = Identity {
            device: get_identity("device")?,
            inode: get_identity("inode")?,
            mode: u32::try_from(get_identity("mode")?).map_err(|_| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "plan transaction mode is invalid",
                )
            })?,
            uid: 0,
            gid: 0,
            nlink: 0,
            size: 0,
            mtime_ns: 0,
        };
        let actual = identity(&symlink_metadata(transaction, "transaction directory")?);
        if actual.device != transaction_identity.device
            || actual.inode != transaction_identity.inode
            || actual.mode != transaction_identity.mode
        {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                "transaction directory identity changed after planning",
            ));
        }
        let repository_root = PathBuf::from(
            object
                .get("repository_root")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let canonical_source = PolicySourceSnapshot::from_json(
            object.get("canonical_source").unwrap_or(&Value::Null),
        )?;
        if canonical_source.path != repository_root.join(CANONICAL_POLICY_RELATIVE) {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan canonical source is malformed or not the required repository path",
            ));
        }
        let platform = HostFamily::from_str(
            object
                .get("platform")
                .and_then(Value::as_object)
                .and_then(|value| value.get("family"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )?;
        let entry_values = object
            .get("entries")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "plan has no explicit targets",
                )
            })?;
        let entries = entry_values
            .iter()
            .enumerate()
            .map(|(index, entry)| PolicyPlanEntry::from_json(entry, index))
            .collect::<LinkResult<Vec<_>>>()?;
        if entries.is_empty() {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan has no explicit targets",
            ));
        }
        let plan = Self {
            plan_id,
            platform,
            repository_root,
            canonical_source,
            transaction_dir: transaction.to_path_buf(),
            transaction_identity,
            entries,
        };
        validate_policy_plan(&plan)?;
        Ok(plan)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyPlanReceipt {
    pub plan: PolicyPlan,
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PolicyJournalEntry {
    id: String,
    stage: String,
    changed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PolicyJournal {
    plan_id: String,
    plan_sha256: String,
    transaction_identity: Identity,
    status: String,
    entries: Vec<PolicyJournalEntry>,
}

impl PolicyJournal {
    fn to_json(&self) -> Value {
        json!({
            "schema_version": POLICY_SCHEMA_VERSION,
            "manager": POLICY_MANAGER_NAME,
            "manager_version": POLICY_MANAGER_VERSION,
            "plan_id": self.plan_id,
            "plan_sha256": self.plan_sha256,
            "transaction_identity": self.transaction_identity.compact_json(),
            "status": self.status,
            "entries": self.entries.iter().map(|entry| json!({
                "id": entry.id,
                "stage": entry.stage,
                "changed": entry.changed,
            })).collect::<Vec<_>>(),
        })
    }

    fn from_json(value: &Value, plan: &PolicyPlan, digest: &str) -> LinkResult<Self> {
        let object = value.as_object().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal must contain a JSON object",
            )
        })?;
        let status = object
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let valid_statuses = [
            "planned",
            "applying",
            "applied",
            "verified",
            "apply-failed-rolled-back",
            "apply-failed-rollback-blocked",
            "rolling-back",
            "rolled-back",
            "rollback-blocked",
        ];
        if object.get("schema_version").and_then(Value::as_u64) != Some(POLICY_SCHEMA_VERSION)
            || object.get("manager").and_then(Value::as_str) != Some(POLICY_MANAGER_NAME)
            || object.get("manager_version").and_then(Value::as_str) != Some(POLICY_MANAGER_VERSION)
            || object.get("plan_id").and_then(Value::as_str) != Some(&plan.plan_id)
            || object.get("plan_sha256").and_then(Value::as_str) != Some(digest)
            || !valid_statuses.contains(&status)
        {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal does not match the reviewed plan",
            ));
        }
        let entry_values = object
            .get("entries")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                LinkError::new(
                    LinkErrorKind::InvalidTransaction,
                    "transaction journal target count does not match the plan",
                )
            })?;
        if entry_values.len() != plan.entries.len() {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal target count does not match the plan",
            ));
        }
        let entries = entry_values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let entry = value.as_object().ok_or_else(|| {
                    LinkError::new(
                        LinkErrorKind::InvalidTransaction,
                        "transaction journal target state is malformed",
                    )
                })?;
                let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
                let stage = entry
                    .get("stage")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let valid_stages = [
                    "pending",
                    "retained",
                    "backing-up",
                    "backup-moved",
                    "installing",
                    "installed",
                    "rolled-back",
                ];
                if id != plan.entries[index].id || !valid_stages.contains(&stage) {
                    return Err(LinkError::new(
                        LinkErrorKind::InvalidTransaction,
                        "transaction journal target state is malformed",
                    ));
                }
                Ok(PolicyJournalEntry {
                    id: id.to_owned(),
                    stage: stage.to_owned(),
                    changed: entry
                        .get("changed")
                        .and_then(Value::as_bool)
                        .ok_or_else(|| {
                            LinkError::new(
                                LinkErrorKind::InvalidTransaction,
                                "transaction journal target state is malformed",
                            )
                        })?,
                })
            })
            .collect::<LinkResult<Vec<_>>>()?;
        if entries.len() != plan.entries.len() {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal target count does not match the plan",
            ));
        }
        Ok(Self {
            plan_id: plan.plan_id.clone(),
            plan_sha256: digest.to_owned(),
            transaction_identity: plan.transaction_identity.clone(),
            status: status.to_owned(),
            entries,
        })
    }
}

fn validate_policy_target(
    target: &PolicyTarget,
    repository: &Path,
    family: HostFamily,
) -> LinkResult<(PathBuf, PathBuf, Identity)> {
    let destination = normalize_absolute(
        &target.destination,
        &format!("--{}-target", target.role.as_str()),
    )?;
    if destination.file_name().and_then(|value| value.to_str()) != Some(target.role.required_name())
    {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            format!(
                "{} target must name {} exactly: {}",
                target.role.as_str(),
                target.role.required_name(),
                destination.display()
            ),
        ));
    }
    let parent = real_directory(
        destination.parent().ok_or_else(|| {
            LinkError::new(LinkErrorKind::UnsafePath, "policy target has no parent")
        })?,
        &format!("{} target parent", target.role.as_str()),
    )?;
    if is_within(&destination, repository) || is_within(repository, &parent) {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            format!(
                "runtime policy target must be outside the canonical repository: {}",
                destination.display()
            ),
        ));
    }
    if target.role == PolicyRole::Codex && target.mode != PolicyMode::DirectAbsoluteSymlink {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            "Codex global policy supports direct-link mode only",
        ));
    }
    if target.mode == PolicyMode::ClaudeWindowsImportWrapper
        && (target.role != PolicyRole::Claude || family != HostFamily::Windows)
    {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            "Claude import-wrapper mode is explicit and available only on native Windows",
        ));
    }
    let parent_identity = identity(&symlink_metadata(&parent, "target parent")?);
    Ok((destination, parent, parent_identity))
}

fn policy_desired(
    mode: PolicyMode,
    source: &Path,
    family: HostFamily,
) -> LinkResult<DesiredPolicy> {
    if mode == PolicyMode::DirectAbsoluteSymlink {
        return Ok(DesiredPolicy::Symlink {
            link_text: source.to_string_lossy().into_owned(),
        });
    }
    let payload = claude_wrapper_bytes(&source.to_string_lossy(), family == HostFamily::Windows)?;
    Ok(DesiredPolicy::RegularFile {
        format: POLICY_WINDOWS_WRAPPER_MODE.into(),
        sha256: hex(&sha256(&payload)),
        size: u64::try_from(payload.len()).unwrap_or(u64::MAX),
    })
}

fn policy_matches_desired(path: &Path, desired: &DesiredPolicy) -> bool {
    let Ok(snapshot) = policy_snapshot(path, true) else {
        return false;
    };
    match (snapshot, desired) {
        (
            PolicySnapshot::Symlink { link_text, .. },
            DesiredPolicy::Symlink {
                link_text: expected,
            },
        ) => link_text == *expected,
        (
            PolicySnapshot::RegularFile { sha256, metadata },
            DesiredPolicy::RegularFile {
                sha256: expected,
                size,
                ..
            },
        ) => sha256 == *expected && metadata.size == *size,
        _ => false,
    }
}

fn policy_entry_paths(plan_id: &str, index: usize, destination: &Path) -> (PathBuf, PathBuf) {
    let suffix = format!("{plan_id}-{index:03}");
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("policy");
    (
        destination
            .parent()
            .unwrap_or(Path::new("/"))
            .join(format!(".{name}.{POLICY_BACKUP_MARKER}-{suffix}")),
        destination
            .parent()
            .unwrap_or(Path::new("/"))
            .join(format!(".{name}.{POLICY_TEMP_MARKER}-{suffix}")),
    )
}

fn prepare_policy_transaction(
    requested: &Path,
    repository: &Path,
    parents: &[PathBuf],
) -> LinkResult<PathBuf> {
    let requested = normalize_absolute(requested, "--transaction-dir")?;
    if lexical_exists(&requested) {
        return Err(LinkError::new(
            LinkErrorKind::Conflict,
            format!(
                "transaction directory already exists: {}",
                requested.display()
            ),
        ));
    }
    let parent = real_directory(
        requested.parent().ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::UnsafePath,
                "transaction directory has no parent",
            )
        })?,
        "transaction parent",
    )?;
    let transaction = parent.join(requested.file_name().ok_or_else(|| {
        LinkError::new(
            LinkErrorKind::UnsafePath,
            "transaction directory needs a safe leaf name",
        )
    })?);
    if is_within(&transaction, repository)
        || is_within(repository, &transaction)
        || parents.iter().any(|target_parent| {
            is_within(&transaction, target_parent) || is_within(target_parent, &transaction)
        })
    {
        return Err(LinkError::new(
            LinkErrorKind::UnsafePath,
            "transaction directory must be outside the repository and every target parent",
        ));
    }
    fs::create_dir(&transaction).map_err(|error| {
        LinkError::io(
            format!("cannot create transaction {}", transaction.display()),
            error,
        )
    })?;
    set_mode(&transaction, 0o700)?;
    let lock_marker = transaction.join(".devcoordinator2-universal-policy-transaction.lock");
    let mut lock_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_marker)
        .map_err(|error| LinkError::io("cannot create transaction lock marker", error))?;
    lock_file
        .write_all(b"devcoordinator2-universal-policy-lock-v1\n")
        .map_err(|error| LinkError::io("cannot write transaction lock marker", error))?;
    lock_file
        .sync_all()
        .map_err(|error| LinkError::io("cannot sync transaction lock marker", error))?;
    set_mode(&lock_marker, 0o600)?;
    Ok(transaction)
}

fn validate_policy_plan(plan: &PolicyPlan) -> LinkResult<()> {
    let source = plan.repository_root.join(CANONICAL_POLICY_RELATIVE);
    let mut seen = BTreeSet::new();
    for (index, entry) in plan.entries.iter().enumerate() {
        if entry.id != format!("target-{:03}", index + 1) || entry.order != index {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan target ordering or ID is invalid",
            ));
        }
        if entry.destination.parent() != Some(entry.parent.as_path()) {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan target is not an immediate child of its bound parent",
            ));
        }
        if entry
            .destination
            .file_name()
            .and_then(|value| value.to_str())
            != Some(entry.role.required_name())
        {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan target basename does not match its runtime",
            ));
        }
        if !seen.insert(entry.destination.clone()) {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan contains duplicate targets",
            ));
        }
        if entry.role == PolicyRole::Codex && entry.mode != PolicyMode::DirectAbsoluteSymlink {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan target mode is invalid",
            ));
        }
        if entry.mode == PolicyMode::ClaudeWindowsImportWrapper
            && (entry.role != PolicyRole::Claude || plan.platform != HostFamily::Windows)
        {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan contains an invalid import-wrapper target",
            ));
        }
        let expected_desired = policy_desired(entry.mode, &source, plan.platform)?;
        if entry.desired != expected_desired {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan desired policy bytes are not canonical",
            ));
        }
        let (backup, temporary) = policy_entry_paths(&plan.plan_id, index, &entry.destination);
        if entry.backup != backup || entry.temporary != temporary {
            return Err(LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "plan backup or temporary path was changed",
            ));
        }
    }
    Ok(())
}

/// Create and persist one immutable, digest-bound policy deployment plan.
pub fn create_policy_plan(
    repository: &Path,
    transaction: &Path,
    targets: &[PolicyTarget],
) -> LinkResult<PolicyPlanReceipt> {
    let repository = canonical_policy_repository(repository)?;
    if targets.is_empty() {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            "plan requires at least one explicit runtime policy target",
        ));
    }
    if targets.len() > MAX_RESULT_ENTRIES {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            format!("policy plan exceeds the bounded {MAX_RESULT_ENTRIES}-target limit"),
        ));
    }
    let family = HostFamily::detect();
    let mut specifications = targets
        .iter()
        .map(|target| {
            validate_policy_target(target, &repository, family).map(
                |(destination, parent, parent_identity)| {
                    (
                        target.role,
                        target.mode,
                        destination,
                        parent,
                        parent_identity,
                    )
                },
            )
        })
        .collect::<LinkResult<Vec<_>>>()?;
    specifications.sort_by(|left, right| {
        left.2
            .to_string_lossy()
            .cmp(&right.2.to_string_lossy())
            .then_with(|| left.0.cmp(&right.0))
    });
    for pair in specifications.windows(2) {
        if pair[0].2 == pair[1].2 {
            return Err(LinkError::new(
                LinkErrorKind::InvalidInput,
                format!("duplicate runtime policy target: {}", pair[0].2.display()),
            ));
        }
    }
    let parents = specifications
        .iter()
        .map(|specification| specification.3.clone())
        .collect::<Vec<_>>();
    let lock_specs = specifications
        .iter()
        .map(|specification| (specification.3.clone(), specification.4.clone()))
        .collect::<Vec<_>>();
    let locks = acquire_directory_locks(&lock_specs)?;
    let source_snapshot = policy_source_snapshot(&repository)?;
    require_policy_source_snapshot(&repository, &source_snapshot)?;
    let transaction = prepare_policy_transaction(transaction, &repository, &parents)?;
    let transaction_identity = identity(&symlink_metadata(&transaction, "transaction directory")?);
    let plan_id = unique_hex();
    let source = repository.join(CANONICAL_POLICY_RELATIVE);
    let planning = (|| {
        let mut entries = Vec::new();
        for (index, (role, mode, destination, parent, parent_identity)) in
            specifications.into_iter().enumerate()
        {
            let lock = lock_for(&locks, &parent)?;
            lock.revalidate()?;
            let before = policy_snapshot(&destination, true)?;
            let (backup, temporary) = policy_entry_paths(&plan_id, index, &destination);
            if lexical_exists(&backup) || lexical_exists(&temporary) {
                return Err(LinkError::new(
                    LinkErrorKind::Conflict,
                    format!(
                        "manager-owned backup or temporary path already exists: {} or {}",
                        backup.display(),
                        temporary.display()
                    ),
                ));
            }
            let desired = policy_desired(mode, &source, family)?;
            let action = if policy_matches_desired(&destination, &desired) {
                PolicyAction::Retain
            } else {
                PolicyAction::Replace
            };
            entries.push(PolicyPlanEntry {
                id: format!("target-{:03}", index + 1),
                order: index,
                role,
                mode,
                destination,
                parent,
                parent_identity,
                before,
                desired,
                backup,
                temporary,
                action,
            });
        }
        require_policy_source_snapshot(&repository, &source_snapshot)?;
        let plan = PolicyPlan {
            plan_id,
            platform: family,
            repository_root: repository.clone(),
            canonical_source: source_snapshot.clone(),
            transaction_dir: transaction.clone(),
            transaction_identity: transaction_identity.clone(),
            entries,
        };
        let payload = canonical_json(&plan.to_json())?;
        let digest = hex(&sha256(&payload));
        atomic_write(&transaction.join(POLICY_PLAN_NAME), &payload, 0o400)?;
        let journal = PolicyJournal {
            plan_id: plan.plan_id.clone(),
            plan_sha256: digest.clone(),
            transaction_identity: transaction_identity.clone(),
            status: "planned".into(),
            entries: plan
                .entries
                .iter()
                .map(|entry| PolicyJournalEntry {
                    id: entry.id.clone(),
                    stage: "pending".into(),
                    changed: false,
                })
                .collect(),
        };
        save_policy_journal(&transaction, &journal)?;
        require_policy_source_snapshot(&repository, &source_snapshot)?;
        for entry in &plan.entries {
            lock_for(&locks, &entry.parent)?.revalidate()?;
            if policy_snapshot(&entry.destination, true)? != entry.before {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "target changed while its plan was being persisted: {}",
                        entry.destination.display()
                    ),
                ));
            }
        }
        Ok(PolicyPlanReceipt { plan, digest })
    })();
    if planning.is_err() {
        for name in [
            POLICY_JOURNAL_NAME,
            POLICY_PLAN_NAME,
            ".devcoordinator2-universal-policy-transaction.lock",
        ] {
            let path = transaction.join(name);
            if fs::symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
            {
                let _ = fs::remove_file(path);
            }
        }
        let _ = fs::remove_dir(&transaction);
    }
    planning
}

fn save_policy_journal(transaction: &Path, journal: &PolicyJournal) -> LinkResult<()> {
    let metadata = symlink_metadata(transaction, "transaction directory")?;
    let actual = identity(&metadata);
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || actual.device != journal.transaction_identity.device
        || actual.inode != journal.transaction_identity.inode
        || actual.mode != journal.transaction_identity.mode
    {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            "transaction directory identity changed before journal write",
        ));
    }
    atomic_write(
        &transaction.join(POLICY_JOURNAL_NAME),
        &canonical_json(&journal.to_json())?,
        0o600,
    )?;
    let after = identity(&symlink_metadata(transaction, "transaction directory")?);
    if after.device != journal.transaction_identity.device
        || after.inode != journal.transaction_identity.inode
        || after.mode != journal.transaction_identity.mode
    {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            "transaction directory identity changed during journal write",
        ));
    }
    Ok(())
}

fn save_policy_journal_locked(
    transaction: &Path,
    journal: &PolicyJournal,
    transaction_lock: &DirectoryLock,
    allow_detached: bool,
) -> LinkResult<()> {
    let opened = identity(
        &transaction_lock
            .file
            .metadata()
            .map_err(|error| LinkError::io("cannot inspect held transaction directory", error))?,
    );
    if opened.device != journal.transaction_identity.device
        || opened.inode != journal.transaction_identity.inode
        || opened.mode != journal.transaction_identity.mode
    {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            "held transaction directory identity does not match the reviewed plan",
        ));
    }
    if !allow_detached {
        transaction_lock.revalidate()?;
    }
    #[cfg(unix)]
    atomic_write_at(
        &transaction_lock.file,
        transaction,
        POLICY_JOURNAL_NAME,
        &canonical_json(&journal.to_json())?,
        0o600,
    )?;
    #[cfg(not(unix))]
    atomic_write(
        &transaction.join(POLICY_JOURNAL_NAME),
        &canonical_json(&journal.to_json())?,
        0o600,
    )?;
    if !allow_detached {
        transaction_lock.revalidate()?;
    }
    Ok(())
}

fn load_policy_transaction(
    transaction: &Path,
    expected_digest: &str,
) -> LinkResult<(PathBuf, PolicyPlan, PolicyJournal)> {
    if !is_hex(expected_digest, 64) {
        return Err(LinkError::new(
            LinkErrorKind::InvalidInput,
            "--plan-digest must be the exact 64-character digest printed by plan",
        ));
    }
    let transaction = real_directory(transaction, "transaction directory")?;
    let (plan_value, plan_payload) =
        read_json(&transaction.join(POLICY_PLAN_NAME), "transaction plan")?;
    let actual_digest = hex(&sha256(&plan_payload));
    if actual_digest != expected_digest {
        return Err(LinkError::new(
            LinkErrorKind::InvalidTransaction,
            "reviewed plan digest does not match the persisted immutable plan",
        ));
    }
    let plan = PolicyPlan::from_json(&plan_value, &transaction)?;
    let (journal_value, _) = read_json(
        &transaction.join(POLICY_JOURNAL_NAME),
        "transaction journal",
    )?;
    let journal = PolicyJournal::from_json(&journal_value, &plan, expected_digest)?;
    let journal_identity = journal_value
        .get("transaction_identity")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            LinkError::new(
                LinkErrorKind::InvalidTransaction,
                "transaction journal does not match the reviewed plan",
            )
        })?;
    if journal_identity.get("device").and_then(Value::as_u64)
        != Some(plan.transaction_identity.device)
        || journal_identity.get("inode").and_then(Value::as_u64)
            != Some(plan.transaction_identity.inode)
        || journal_identity.get("mode").and_then(Value::as_u64)
            != Some(u64::from(plan.transaction_identity.mode))
    {
        return Err(LinkError::new(
            LinkErrorKind::InvalidTransaction,
            "transaction journal does not match the reviewed plan",
        ));
    }
    Ok((transaction, plan, journal))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandProvider {
    PosixShell,
    PowerShell,
}

impl CommandProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PosixShell => "posix-shell",
            Self::PowerShell => "powershell",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedPolicyCommand {
    pub provider: CommandProvider,
    pub argv: Vec<String>,
    pub rendered: String,
}

fn posix_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

/// Render argv for display only. The returned string is never executed here.
pub fn render_shell_command(arguments: &[String], windows: bool) -> String {
    if windows {
        let quoted = arguments
            .iter()
            .map(|argument| format!("'{}'", argument.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" ");
        format!("& {quoted}")
    } else {
        arguments
            .iter()
            .map(|argument| posix_quote(argument))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Build the reviewed apply invocation as inert argv plus provider rendering.
pub fn generate_policy_apply_command(
    plan: &PolicyPlan,
    digest: &str,
    executable: &Path,
) -> GeneratedPolicyCommand {
    let argv = vec![
        executable.to_string_lossy().into_owned(),
        "skills".into(),
        "policy".into(),
        "apply".into(),
        "--transaction-dir".into(),
        plan.transaction_dir.to_string_lossy().into_owned(),
        "--plan-digest".into(),
        digest.to_owned(),
    ];
    let windows = plan.platform == HostFamily::Windows;
    GeneratedPolicyCommand {
        provider: if windows {
            CommandProvider::PowerShell
        } else {
            CommandProvider::PosixShell
        },
        rendered: render_shell_command(&argv, windows),
        argv,
    }
}

pub fn render_policy_plan(receipt: &PolicyPlanReceipt, executable: &Path) -> String {
    let policy_hash = match &receipt.plan.canonical_source.policy {
        PolicySnapshot::RegularFile { sha256, .. } => sha256.as_str(),
        _ => "unavailable",
    };
    let mut lines = vec![
        format!("plan_id: {}", receipt.plan.plan_id),
        format!("plan_sha256: {}", receipt.digest),
        format!(
            "canonical_source: {}",
            receipt.plan.canonical_source.path.display()
        ),
        format!("canonical_sha256: {policy_hash}"),
        format!(
            "transaction_dir: {}",
            receipt.plan.transaction_dir.display()
        ),
    ];
    for entry in &receipt.plan.entries {
        lines.push(format!(
            "{:<7} {:<6} {:<35} {:<12} {}",
            entry.action.as_str(),
            entry.role.as_str(),
            entry.mode.as_str(),
            entry.before.kind(),
            entry.destination.display()
        ));
    }
    lines.push("filesystem_activation: not-applied".into());
    lines.push(
        "runtime_activation: not-observed (restart and inspect the runtime instruction surface after apply)"
            .into(),
    );
    if receipt
        .plan
        .entries
        .iter()
        .any(|entry| entry.mode == PolicyMode::ClaudeWindowsImportWrapper)
    {
        lines.push(
            "claude_external_import: may require first-use approval; confirm the canonical file with /memory"
                .into(),
        );
    }
    let command = generate_policy_apply_command(&receipt.plan, &receipt.digest, executable);
    lines.push(format!("apply: {}", command.rendered));
    lines.join("\n")
}

pub trait PolicyFaultHook {
    fn checkpoint(&mut self, point: &str, entry_id: &str) -> LinkResult<()>;
}

impl<F> PolicyFaultHook for F
where
    F: FnMut(&str, &str) -> LinkResult<()>,
{
    fn checkpoint(&mut self, point: &str, entry_id: &str) -> LinkResult<()> {
        self(point, entry_id)
    }
}

fn checkpoint(
    hook: &mut Option<&mut dyn PolicyFaultHook>,
    point: &str,
    entry_id: &str,
) -> LinkResult<()> {
    if let Some(hook) = hook.as_deref_mut() {
        hook.checkpoint(point, entry_id)?;
    }
    Ok(())
}

fn policy_parent_locks(plan: &PolicyPlan) -> LinkResult<Vec<DirectoryLock>> {
    acquire_directory_locks(
        &plan
            .entries
            .iter()
            .map(|entry| (entry.parent.clone(), entry.parent_identity.clone()))
            .collect::<Vec<_>>(),
    )
}

fn policy_apply_preflight(plan: &PolicyPlan, locks: &[DirectoryLock]) -> LinkResult<()> {
    require_policy_source_snapshot(&plan.repository_root, &plan.canonical_source)?;
    for entry in &plan.entries {
        let lock = lock_for(locks, &entry.parent)?;
        lock.revalidate()?;
        if policy_snapshot(&entry.destination, true)? != entry.before {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "target drifted after planning: {}",
                    entry.destination.display()
                ),
            ));
        }
        for artifact in [&entry.backup, &entry.temporary] {
            if lexical_exists(artifact) {
                return Err(LinkError::new(
                    LinkErrorKind::Conflict,
                    format!(
                        "manager-owned transaction path is not empty: {}",
                        artifact.display()
                    ),
                ));
            }
        }
        let expected = if policy_matches_desired(&entry.destination, &entry.desired) {
            PolicyAction::Retain
        } else {
            PolicyAction::Replace
        };
        if expected != entry.action {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "target classification changed after planning: {}",
                    entry.destination.display()
                ),
            ));
        }
    }
    Ok(())
}

fn set_policy_stage(
    transaction: &Path,
    journal: &mut PolicyJournal,
    index: usize,
    stage: &str,
    changed: Option<bool>,
    transaction_lock: &DirectoryLock,
    allow_detached: bool,
) -> LinkResult<()> {
    journal.entries[index].stage = stage.into();
    if let Some(changed) = changed {
        journal.entries[index].changed = changed;
    }
    save_policy_journal_locked(transaction, journal, transaction_lock, allow_detached)
}

fn install_desired_policy(
    lock: &DirectoryLock,
    entry: &PolicyPlanEntry,
    source: &Path,
) -> LinkResult<()> {
    if lexical_exists(&entry.destination) || lexical_exists(&entry.temporary) {
        return Err(LinkError::new(
            LinkErrorKind::Conflict,
            format!(
                "destination or temporary path is not empty before install: {}",
                entry.destination.display()
            ),
        ));
    }
    match entry.mode {
        PolicyMode::DirectAbsoluteSymlink => {
            let temporary_name = entry
                .temporary
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    LinkError::new(LinkErrorKind::UnsafePath, "unsafe temporary name")
                })?;
            create_file_symlink_at(lock, source, temporary_name).map_err(|error| {
                LinkError::new(
                    LinkErrorKind::Io,
                    format!(
                        "direct symlink creation failed; no copy or import-wrapper fallback was attempted: {}: {error}",
                        entry.destination.display()
                    ),
                )
            })?;
        }
        PolicyMode::ClaudeWindowsImportWrapper => {
            let payload = claude_wrapper_bytes(&source.to_string_lossy(), true)?;
            let temporary_name = entry
                .temporary
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    LinkError::new(LinkErrorKind::UnsafePath, "unsafe temporary name")
                })?;
            #[cfg(unix)]
            let mut file = {
                let descriptor = rustix::fs::openat(
                    &lock.file,
                    temporary_name,
                    rustix::fs::OFlags::WRONLY
                        | rustix::fs::OFlags::CREATE
                        | rustix::fs::OFlags::EXCL
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::from_raw_mode(0o600),
                )
                .map_err(|error| {
                    LinkError::io(
                        format!("cannot create {}", entry.temporary.display()),
                        error.into(),
                    )
                })?;
                File::from(descriptor)
            };
            #[cfg(not(unix))]
            let mut options = OpenOptions::new();
            #[cfg(not(unix))]
            options.write(true).create_new(true);
            #[cfg(not(unix))]
            let mut file = options.open(&entry.temporary).map_err(|error| {
                LinkError::io(
                    format!("cannot create {}", entry.temporary.display()),
                    error,
                )
            })?;
            file.write_all(&payload).map_err(|error| {
                LinkError::io(format!("cannot write {}", entry.temporary.display()), error)
            })?;
            file.sync_all().map_err(|error| {
                LinkError::io(format!("cannot sync {}", entry.temporary.display()), error)
            })?;
        }
    }
    lock.sync();
    if !policy_matches_desired(&entry.temporary, &entry.desired) {
        return Err(LinkError::new(
            LinkErrorKind::Drift,
            format!(
                "new policy artifact failed pre-install verification: {}",
                entry.temporary.display()
            ),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RollbackCase {
    Retained,
    NotMutated,
    PreparedMissing,
    InstalledWithoutBackup,
    BackupOnly,
    BackupWithPrepared,
    InstalledWithBackup,
    RestoredWithCaptured,
}

fn policy_rollback_preflight(
    plan: &PolicyPlan,
    locks: &[DirectoryLock],
) -> LinkResult<Vec<RollbackCase>> {
    let mut states = Vec::with_capacity(plan.entries.len());
    for entry in &plan.entries {
        let lock = lock_for(locks, &entry.parent)?;
        lock.revalidate()?;
        let current = policy_snapshot(&entry.destination, true)?;
        let backup = policy_snapshot(&entry.backup, true)?;
        let temporary = policy_snapshot(&entry.temporary, true)?;
        let temporary_desired = temporary != PolicySnapshot::Missing
            && policy_matches_desired(&entry.temporary, &entry.desired);
        if temporary != PolicySnapshot::Missing && !temporary_desired {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "temporary policy artifact drift blocks all-action rollback: {}",
                    entry.temporary.display()
                ),
            ));
        }
        if entry.action == PolicyAction::Retain {
            if current != entry.before
                || backup != PolicySnapshot::Missing
                || temporary != PolicySnapshot::Missing
            {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "retained target drift blocks all-action rollback: {}",
                        entry.destination.display()
                    ),
                ));
            }
            states.push(RollbackCase::Retained);
            continue;
        }
        let desired_now = policy_matches_desired(&entry.destination, &entry.desired);
        let state = if entry.before == PolicySnapshot::Missing {
            if backup != PolicySnapshot::Missing {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "unexpected backup blocks all-action rollback: {}",
                        entry.backup.display()
                    ),
                ));
            }
            if current == PolicySnapshot::Missing {
                if temporary_desired {
                    RollbackCase::PreparedMissing
                } else {
                    RollbackCase::NotMutated
                }
            } else if desired_now {
                if temporary_desired {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "duplicate desired artifact blocks all-action rollback: {}",
                            entry.destination.display()
                        ),
                    ));
                }
                RollbackCase::InstalledWithoutBackup
            } else {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "target drift blocks all-action rollback: {}",
                        entry.destination.display()
                    ),
                ));
            }
        } else if backup == entry.before {
            if current == PolicySnapshot::Missing {
                if temporary_desired {
                    RollbackCase::BackupWithPrepared
                } else {
                    RollbackCase::BackupOnly
                }
            } else if desired_now {
                if temporary_desired {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "duplicate desired artifact blocks all-action rollback: {}",
                            entry.destination.display()
                        ),
                    ));
                }
                RollbackCase::InstalledWithBackup
            } else {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "target drift blocks all-action rollback: {}",
                        entry.destination.display()
                    ),
                ));
            }
        } else if backup == PolicySnapshot::Missing && current == entry.before {
            if temporary_desired {
                RollbackCase::RestoredWithCaptured
            } else {
                RollbackCase::NotMutated
            }
        } else {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "backup drift blocks all-action rollback: {}",
                    entry.backup.display()
                ),
            ));
        };
        states.push(state);
    }
    Ok(states)
}

fn rollback_policy_locked(
    transaction: &Path,
    plan: &PolicyPlan,
    journal: &mut PolicyJournal,
    locks: &[DirectoryLock],
    transaction_lock: &DirectoryLock,
    failure_status: Option<&str>,
    mut hook: Option<&mut dyn PolicyFaultHook>,
) -> LinkResult<()> {
    let states = match policy_rollback_preflight(plan, locks) {
        Ok(states) => states,
        Err(error) => {
            journal.status = if failure_status.is_some() {
                "apply-failed-rollback-blocked".into()
            } else {
                "rollback-blocked".into()
            };
            let _ = save_policy_journal_locked(
                transaction,
                journal,
                transaction_lock,
                failure_status.is_some(),
            );
            return Err(error);
        }
    };
    journal.status = "rolling-back".into();
    save_policy_journal_locked(
        transaction,
        journal,
        transaction_lock,
        failure_status.is_some(),
    )?;
    let rollback = (|| {
        for index in (0..plan.entries.len()).rev() {
            let entry = &plan.entries[index];
            let state = states[index];
            let lock = lock_for(locks, &entry.parent)?;
            if matches!(
                state,
                RollbackCase::InstalledWithoutBackup | RollbackCase::InstalledWithBackup
            ) {
                checkpoint(&mut hook, "before-rollback-capture", &entry.id)?;
                if !policy_matches_desired(&entry.destination, &entry.desired)
                    || lexical_exists(&entry.temporary)
                {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "installed target changed at rollback capture boundary: {}",
                            entry.destination.display()
                        ),
                    ));
                }
                rename_noreplace(
                    lock,
                    entry
                        .destination
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                    entry
                        .temporary
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                )?;
                if !policy_matches_desired(&entry.temporary, &entry.desired)
                    || policy_snapshot(&entry.destination, true)? != PolicySnapshot::Missing
                {
                    if policy_snapshot(&entry.destination, true)? == PolicySnapshot::Missing {
                        let _ = rename_noreplace(
                            lock,
                            entry
                                .temporary
                                .file_name()
                                .and_then(|value| value.to_str())
                                .unwrap_or_default(),
                            entry
                                .destination
                                .file_name()
                                .and_then(|value| value.to_str())
                                .unwrap_or_default(),
                        );
                    }
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "target changed during rollback capture; no captured object was deleted: {}",
                            entry.destination.display()
                        ),
                    ));
                }
                checkpoint(&mut hook, "after-rollback-capture", &entry.id)?;
            }
            if matches!(
                state,
                RollbackCase::BackupOnly
                    | RollbackCase::BackupWithPrepared
                    | RollbackCase::InstalledWithBackup
            ) {
                checkpoint(&mut hook, "before-rollback-restore", &entry.id)?;
                if policy_snapshot(&entry.backup, true)? != entry.before
                    || policy_snapshot(&entry.destination, true)? != PolicySnapshot::Missing
                {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "rollback restore boundary drifted; prior state remains preserved at {}",
                            entry.backup.display()
                        ),
                    ));
                }
                rename_noreplace(
                    lock,
                    entry
                        .backup
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                    entry
                        .destination
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                )?;
                if policy_snapshot(&entry.destination, true)? != entry.before {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "exact rollback restore verification failed: {}",
                            entry.destination.display()
                        ),
                    ));
                }
                checkpoint(&mut hook, "after-rollback-restore", &entry.id)?;
            }
            if policy_snapshot(&entry.destination, true)? != entry.before {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "exact rollback verification failed: {}",
                        entry.destination.display()
                    ),
                ));
            }
            if lexical_exists(&entry.backup) {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "rollback left the prior object in its backup name: {}",
                        entry.backup.display()
                    ),
                ));
            }
            checkpoint(&mut hook, "before-rollback-retain", &entry.id)?;
            if lexical_exists(&entry.temporary)
                && !policy_matches_desired(&entry.temporary, &entry.desired)
            {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "captured policy artifact drifted and was preserved for inspection: {}",
                        entry.temporary.display()
                    ),
                ));
            }
            checkpoint(&mut hook, "after-rollback-retain", &entry.id)?;
            set_policy_stage(
                transaction,
                journal,
                index,
                "rolled-back",
                Some(false),
                transaction_lock,
                failure_status.is_some(),
            )?;
        }
        Ok(())
    })();
    if let Err(error) = rollback {
        journal.status = if failure_status.is_some() {
            "apply-failed-rollback-blocked".into()
        } else {
            "rollback-blocked".into()
        };
        let _ = save_policy_journal_locked(
            transaction,
            journal,
            transaction_lock,
            failure_status.is_some(),
        );
        return Err(error);
    }
    journal.status = failure_status.unwrap_or("rolled-back").into();
    save_policy_journal_locked(
        transaction,
        journal,
        transaction_lock,
        failure_status.is_some(),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyTransactionResult {
    pub status: String,
    pub filesystem_state: String,
    pub runtime_activation: String,
    pub canonical_source_changed_since_plan: Option<bool>,
    pub targets: Vec<PathBuf>,
    pub retained_artifacts: Vec<PathBuf>,
}

impl PolicyTransactionResult {
    pub fn to_json(&self) -> Value {
        json!({
            "status": self.status,
            "filesystem_state": self.filesystem_state,
            "runtime_activation": self.runtime_activation,
            "canonical_source_changed_since_plan": self.canonical_source_changed_since_plan,
            "targets": self.targets.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),
            "retained_artifacts": self.retained_artifacts.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),
        })
    }
}

fn lock_transaction_directory(transaction: &Path) -> LinkResult<DirectoryLock> {
    let transaction = real_directory(transaction, "transaction directory")?;
    let expected = identity(&symlink_metadata(&transaction, "transaction directory")?);
    DirectoryLock::acquire(&transaction, &expected)
}

pub fn apply_policy_transaction(
    transaction: &Path,
    plan_digest: &str,
) -> LinkResult<PolicyTransactionResult> {
    apply_policy_transaction_with_hook(transaction, plan_digest, None)
}

pub fn apply_policy_transaction_with_hook(
    transaction: &Path,
    plan_digest: &str,
    mut hook: Option<&mut dyn PolicyFaultHook>,
) -> LinkResult<PolicyTransactionResult> {
    let transaction_lock = lock_transaction_directory(transaction)?;
    let (transaction, plan, mut journal) =
        load_policy_transaction(&transaction_lock.path, plan_digest)?;
    transaction_lock.revalidate()?;
    if journal.status != "planned" {
        return Err(LinkError::new(
            LinkErrorKind::InvalidTransaction,
            format!(
                "apply requires a planned transaction; current status is {}",
                journal.status
            ),
        ));
    }
    let locks = policy_parent_locks(&plan)?;
    policy_apply_preflight(&plan, &locks)?;
    journal.status = "applying".into();
    save_policy_journal_locked(&transaction, &journal, &transaction_lock, false)?;
    let apply = (|| {
        let source = plan.repository_root.join(CANONICAL_POLICY_RELATIVE);
        for index in 0..plan.entries.len() {
            let entry = &plan.entries[index];
            let lock = lock_for(&locks, &entry.parent)?;
            if entry.action == PolicyAction::Retain {
                require_policy_source_snapshot(&plan.repository_root, &plan.canonical_source)?;
                if policy_snapshot(&entry.destination, true)? != entry.before
                    || !policy_matches_desired(&entry.destination, &entry.desired)
                {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "retained target drifted during apply: {}",
                            entry.destination.display()
                        ),
                    ));
                }
                set_policy_stage(
                    &transaction,
                    &mut journal,
                    index,
                    "retained",
                    Some(false),
                    &transaction_lock,
                    false,
                )?;
                continue;
            }
            require_policy_source_snapshot(&plan.repository_root, &plan.canonical_source)?;
            lock.revalidate()?;
            if policy_snapshot(&entry.destination, true)? != entry.before {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "target drifted before mutation: {}",
                        entry.destination.display()
                    ),
                ));
            }
            checkpoint(&mut hook, "before-backup", &entry.id)?;
            set_policy_stage(
                &transaction,
                &mut journal,
                index,
                "backing-up",
                None,
                &transaction_lock,
                false,
            )?;
            if entry.before != PolicySnapshot::Missing {
                if policy_snapshot(&entry.destination, true)? != entry.before
                    || lexical_exists(&entry.backup)
                {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "target or backup changed at the backup mutation boundary: {}",
                            entry.destination.display()
                        ),
                    ));
                }
                rename_noreplace(
                    lock,
                    entry
                        .destination
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                    entry
                        .backup
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                )?;
                if policy_snapshot(&entry.backup, true)? != entry.before
                    || policy_snapshot(&entry.destination, true)? != PolicySnapshot::Missing
                {
                    if policy_snapshot(&entry.destination, true)? == PolicySnapshot::Missing {
                        let _ = rename_noreplace(
                            lock,
                            entry
                                .backup
                                .file_name()
                                .and_then(|value| value.to_str())
                                .unwrap_or_default(),
                            entry
                                .destination
                                .file_name()
                                .and_then(|value| value.to_str())
                                .unwrap_or_default(),
                        );
                    }
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "target changed at the atomic backup boundary; captured state was not deleted: {}",
                            entry.destination.display()
                        ),
                    ));
                }
            }
            require_policy_source_snapshot(&plan.repository_root, &plan.canonical_source)?;
            checkpoint(&mut hook, "after-backup", &entry.id)?;
            set_policy_stage(
                &transaction,
                &mut journal,
                index,
                "backup-moved",
                Some(true),
                &transaction_lock,
                false,
            )?;
            set_policy_stage(
                &transaction,
                &mut journal,
                index,
                "installing",
                None,
                &transaction_lock,
                false,
            )?;
            install_desired_policy(lock, entry, &source)?;
            require_policy_source_snapshot(&plan.repository_root, &plan.canonical_source)?;
            checkpoint(&mut hook, "after-temp-create", &entry.id)?;
            checkpoint(&mut hook, "before-install-move", &entry.id)?;
            if !policy_matches_desired(&entry.temporary, &entry.desired) {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "prepared policy changed at install boundary: {}",
                        entry.temporary.display()
                    ),
                ));
            }
            if policy_snapshot(&entry.destination, true)? != PolicySnapshot::Missing {
                return Err(LinkError::new(
                    LinkErrorKind::Conflict,
                    format!(
                        "destination appeared at the install mutation boundary: {}",
                        entry.destination.display()
                    ),
                ));
            }
            rename_noreplace(
                lock,
                entry
                    .temporary
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default(),
                entry
                    .destination
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default(),
            )?;
            if !policy_matches_desired(&entry.destination, &entry.desired) {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "installed policy failed exact verification: {}",
                        entry.destination.display()
                    ),
                ));
            }
            require_policy_source_snapshot(&plan.repository_root, &plan.canonical_source)?;
            checkpoint(&mut hook, "after-install", &entry.id)?;
            set_policy_stage(
                &transaction,
                &mut journal,
                index,
                "installed",
                Some(true),
                &transaction_lock,
                false,
            )?;
        }
        checkpoint(&mut hook, "before-commit", "all")?;
        require_policy_source_snapshot(&plan.repository_root, &plan.canonical_source)?;
        for entry in &plan.entries {
            lock_for(&locks, &entry.parent)?.revalidate()?;
            if !policy_matches_desired(&entry.destination, &entry.desired) {
                return Err(LinkError::new(
                    LinkErrorKind::Drift,
                    format!(
                        "final installed state drifted: {}",
                        entry.destination.display()
                    ),
                ));
            }
        }
        transaction_lock.revalidate()?;
        journal.status = "applied".into();
        save_policy_journal_locked(&transaction, &journal, &transaction_lock, false)
    })();
    if let Err(error) = apply {
        if let Err(rollback_error) = rollback_policy_locked(
            &transaction,
            &plan,
            &mut journal,
            &locks,
            &transaction_lock,
            Some("apply-failed-rolled-back"),
            None,
        ) {
            return Err(LinkError::new(
                LinkErrorKind::Drift,
                format!(
                    "apply failed ({error}); rollback was blocked or failed ({rollback_error})"
                ),
            ));
        }
        return Err(LinkError::new(
            error.kind,
            format!("apply failed and exact rollback succeeded: {error}"),
        ));
    }
    Ok(PolicyTransactionResult {
        status: journal.status,
        filesystem_state: "installed".into(),
        runtime_activation: "not-observed".into(),
        canonical_source_changed_since_plan: Some(false),
        targets: plan.targets(),
        retained_artifacts: Vec::new(),
    })
}

pub fn verify_policy_transaction(
    transaction: &Path,
    plan_digest: &str,
) -> LinkResult<PolicyTransactionResult> {
    let transaction_lock = lock_transaction_directory(transaction)?;
    let (transaction, plan, mut journal) =
        load_policy_transaction(&transaction_lock.path, plan_digest)?;
    if !matches!(
        journal.status.as_str(),
        "applied" | "verified" | "rolled-back" | "apply-failed-rolled-back"
    ) {
        return Err(LinkError::new(
            LinkErrorKind::InvalidTransaction,
            format!(
                "transaction status {} is not a verifiable terminal filesystem state",
                journal.status
            ),
        ));
    }
    let locks = policy_parent_locks(&plan)?;
    let (filesystem_state, source_changed) =
        if matches!(journal.status.as_str(), "applied" | "verified") {
            let current_source = policy_source_snapshot(&plan.repository_root)?;
            for entry in &plan.entries {
                lock_for(&locks, &entry.parent)?.revalidate()?;
                if !policy_matches_desired(&entry.destination, &entry.desired) {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "installed policy verification failed: {}",
                            entry.destination.display()
                        ),
                    ));
                }
                if entry.action == PolicyAction::Replace && entry.before != PolicySnapshot::Missing
                {
                    if policy_snapshot(&entry.backup, true)? != entry.before {
                        return Err(LinkError::new(
                            LinkErrorKind::Drift,
                            format!(
                                "rollback backup verification failed: {}",
                                entry.backup.display()
                            ),
                        ));
                    }
                } else if lexical_exists(&entry.backup) {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!("unexpected rollback backup: {}", entry.backup.display()),
                    ));
                }
                if lexical_exists(&entry.temporary) {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!("temporary artifact remains: {}", entry.temporary.display()),
                    ));
                }
            }
            journal.status = "verified".into();
            save_policy_journal_locked(&transaction, &journal, &transaction_lock, false)?;
            ("installed", Some(current_source != plan.canonical_source))
        } else {
            for entry in &plan.entries {
                lock_for(&locks, &entry.parent)?.revalidate()?;
                if policy_snapshot(&entry.destination, true)? != entry.before {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "rolled-back policy verification failed: {}",
                            entry.destination.display()
                        ),
                    ));
                }
                if lexical_exists(&entry.backup) {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "rolled-back transaction left a prior-state backup: {}",
                            entry.backup.display()
                        ),
                    ));
                }
                if lexical_exists(&entry.temporary)
                    && !policy_matches_desired(&entry.temporary, &entry.desired)
                {
                    return Err(LinkError::new(
                        LinkErrorKind::Drift,
                        format!(
                            "retained rollback artifact drifted: {}",
                            entry.temporary.display()
                        ),
                    ));
                }
            }
            ("rolled-back", None)
        };
    let retained_artifacts = plan
        .entries
        .iter()
        .filter(|entry| lexical_exists(&entry.temporary))
        .map(|entry| entry.temporary.clone())
        .collect();
    Ok(PolicyTransactionResult {
        status: journal.status,
        filesystem_state: filesystem_state.into(),
        runtime_activation: "not-observed".into(),
        canonical_source_changed_since_plan: source_changed,
        targets: plan.targets(),
        retained_artifacts,
    })
}

pub fn rollback_policy_transaction(
    transaction: &Path,
    plan_digest: &str,
) -> LinkResult<PolicyTransactionResult> {
    rollback_policy_transaction_with_hook(transaction, plan_digest, None)
}

pub fn rollback_policy_transaction_with_hook(
    transaction: &Path,
    plan_digest: &str,
    hook: Option<&mut dyn PolicyFaultHook>,
) -> LinkResult<PolicyTransactionResult> {
    let transaction_lock = lock_transaction_directory(transaction)?;
    let (transaction, plan, mut journal) =
        load_policy_transaction(&transaction_lock.path, plan_digest)?;
    if matches!(
        journal.status.as_str(),
        "rolled-back" | "apply-failed-rolled-back"
    ) {
        let original_status = journal.status.clone();
        drop(transaction_lock);
        let mut result = verify_policy_transaction(&transaction, plan_digest)?;
        result.status = original_status;
        return Ok(result);
    }
    let permitted = [
        "planned",
        "applying",
        "rolling-back",
        "applied",
        "verified",
        "apply-failed-rollback-blocked",
        "rollback-blocked",
    ];
    if !permitted.contains(&journal.status.as_str()) {
        return Err(LinkError::new(
            LinkErrorKind::InvalidTransaction,
            format!(
                "transaction cannot be rolled back from status {}",
                journal.status
            ),
        ));
    }
    let locks = policy_parent_locks(&plan)?;
    rollback_policy_locked(
        &transaction,
        &plan,
        &mut journal,
        &locks,
        &transaction_lock,
        None,
        hook,
    )?;
    Ok(PolicyTransactionResult {
        status: "rolled-back".into(),
        filesystem_state: "rolled-back".into(),
        runtime_activation: "not-observed".into(),
        canonical_source_changed_since_plan: None,
        targets: plan.targets(),
        retained_artifacts: plan
            .entries
            .iter()
            .filter(|entry| lexical_exists(&entry.temporary))
            .map(|entry| entry.temporary.clone())
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "devcoordinator2-skill-links-{label}-{}",
                unique_hex()
            ));
            fs::create_dir(&root).expect("create test root");
            Self {
                root: fs::canonicalize(root).expect("canonical test root"),
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn make_skill(repository: &Path, name: &str, marker: &str) -> PathBuf {
        let skill = repository.join("skills").join(name);
        fs::create_dir_all(skill.join("scripts")).expect("create skill");
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\n---\n# {marker}\n"),
        )
        .expect("write skill");
        fs::write(skill.join("scripts/tool.rs"), format!("// {marker}\n")).expect("write helper");
        skill
    }

    fn copy_directory(source: &Path, destination: &Path) {
        fs::create_dir(destination).expect("create destination");
        for entry in fs::read_dir(source).expect("read source") {
            let entry = entry.expect("entry");
            let target = destination.join(entry.file_name());
            let metadata = fs::symlink_metadata(entry.path()).expect("metadata");
            if metadata.is_dir() {
                copy_directory(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).expect("copy file");
            }
        }
    }

    fn make_policy_repository(base: &Path, name: &str) -> (PathBuf, PathBuf) {
        let repository = base.join(name);
        let source = repository.join(CANONICAL_POLICY_RELATIVE);
        fs::create_dir_all(source.parent().expect("policy parent")).expect("create policy source");
        fs::write(&source, "# Universal Agent Instructions\n").expect("write policy source");
        (repository, source)
    }

    #[test]
    fn sha256_matches_standard_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn skill_links_plan_apply_verify_rollback_and_preserve_unrelated() {
        let tree = TempTree::new("skill-happy");
        let repository = tree.path("canonical repo with spaces");
        let alpha = make_skill(&repository, "alpha-skill", "alpha");
        make_skill(&repository, "beta-skill", "beta");
        let target = tree.path("runtime skills with spaces");
        fs::create_dir(&target).expect("target");
        copy_directory(&alpha, &target.join("alpha-skill"));
        fs::create_dir_all(alpha.join("scripts/__pycache__")).expect("cache");
        fs::write(alpha.join("scripts/__pycache__/tool.pyc"), b"canonical").expect("cache");
        fs::create_dir_all(target.join("alpha-skill/scripts/__pycache__")).expect("cache");
        fs::write(
            target.join("alpha-skill/scripts/__pycache__/tool.pyc"),
            b"installed",
        )
        .expect("cache");
        let unrelated = target.join("third-party-skill");
        fs::create_dir(&unrelated).expect("unrelated");
        fs::write(unrelated.join("owner.txt"), "do not touch\n").expect("unrelated state");

        let plan = build_skill_link_plan(&repository, std::slice::from_ref(&target), None)
            .expect("build plan");
        let states = plan
            .entries
            .iter()
            .map(|entry| (entry.skill.as_str(), entry.installation.status))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(states["alpha-skill"], SkillInstallationStatus::CopiedMatch);
        assert_eq!(states["beta-skill"], SkillInstallationStatus::Missing);

        let transaction = tree.path("skill transaction");
        let receipt = apply_skill_links(
            &repository,
            std::slice::from_ref(&target),
            &transaction,
            &SkillLinkApplyOptions::default(),
        )
        .expect("apply");
        assert_eq!(receipt.status, "applied");
        assert!(absolute_link_matches(&target.join("alpha-skill"), &alpha));
        assert!(
            build_skill_link_plan(&repository, std::slice::from_ref(&target), None)
                .expect("verify")
                .is_verified()
        );
        assert_eq!(
            fs::read_to_string(unrelated.join("owner.txt")).expect("unrelated"),
            "do not touch\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(transaction.join(SKILL_LINK_JOURNAL_NAME))
                    .expect("journal")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let rollback = rollback_skill_links(&transaction, false).expect("rollback");
        assert_eq!(rollback.status, "rolled_back");
        assert!(target.join("alpha-skill").is_dir());
        assert!(!lexical_exists(&target.join("beta-skill")));
        assert_eq!(
            fs::read_to_string(unrelated.join("owner.txt")).expect("unrelated"),
            "do not touch\n"
        );
        rollback_skill_links(&transaction, false).expect("idempotent rollback");
    }

    #[test]
    fn skill_links_refuse_divergence_and_rollback_partial_failure() {
        let tree = TempTree::new("skill-divergence");
        let repository = tree.path("repository");
        let alpha = make_skill(&repository, "alpha-skill", "alpha");
        make_skill(&repository, "beta-skill", "beta");
        let target = tree.path("target");
        fs::create_dir(&target).expect("target");
        copy_directory(&alpha, &target.join("alpha-skill"));
        copy_directory(
            &repository.join("skills/beta-skill"),
            &target.join("beta-skill"),
        );
        fs::write(target.join("alpha-skill/SKILL.md"), "divergent\n").expect("diverge");
        let refused = apply_skill_links(
            &repository,
            std::slice::from_ref(&target),
            &tree.path("refused"),
            &SkillLinkApplyOptions::default(),
        )
        .expect_err("divergence should require review");
        assert!(refused.to_string().contains("allow-noncanonical"));

        let transaction = tree.path("partial");
        let error = apply_skill_links(
            &repository,
            std::slice::from_ref(&target),
            &transaction,
            &SkillLinkApplyOptions {
                selected_skills: None,
                allow_noncanonical: true,
                failure_after_links: Some(1),
            },
        )
        .expect_err("injected partial failure");
        assert!(error.to_string().contains("rolled back"));
        assert!(target.join("alpha-skill").is_dir());
        assert!(target.join("beta-skill").is_dir());
        let (value, _) =
            read_json(&transaction.join(SKILL_LINK_JOURNAL_NAME), "journal").expect("journal");
        assert_eq!(value["status"], "rolled_back");
    }

    #[test]
    fn skill_links_accept_v2_rollback_and_refuse_post_apply_changes() {
        let tree = TempTree::new("skill-v2-rollback");
        let repository = tree.path("repository");
        let source = make_skill(&repository, "alpha-skill", "alpha");
        let target = tree.path("target");
        fs::create_dir(&target).expect("target");
        copy_directory(&source, &target.join("alpha-skill"));
        let transaction = tree.path("transaction");
        apply_skill_links(
            &repository,
            std::slice::from_ref(&target),
            &transaction,
            &SkillLinkApplyOptions::default(),
        )
        .expect("apply");

        let journal_path = transaction.join(SKILL_LINK_JOURNAL_NAME);
        let (mut value, _) = read_json(&journal_path, "journal").expect("journal");
        value["version"] = Value::from(2);
        for entry in value["entries"].as_array_mut().expect("entries") {
            entry
                .as_object_mut()
                .expect("entry")
                .remove("source_snapshot");
        }
        atomic_write(&journal_path, &canonical_json(&value).expect("JSON"), 0o600)
            .expect("rewrite legacy journal");

        fs::remove_file(target.join("alpha-skill")).expect("remove direct link");
        fs::create_dir(target.join("alpha-skill")).expect("post-apply replacement");
        fs::write(target.join("alpha-skill/new.txt"), "post apply\n").expect("replacement");
        assert!(
            rollback_skill_links(&transaction, false)
                .expect_err("post-apply state must be preserved")
                .to_string()
                .contains("post-apply change")
        );
        assert_eq!(
            fs::read_to_string(target.join("alpha-skill/new.txt")).expect("replacement"),
            "post apply\n"
        );
        rollback_skill_links(&transaction, true).expect("force reviewed rollback");
        assert!(target.join("alpha-skill/SKILL.md").is_file());
    }

    #[test]
    fn skill_links_classify_broken_chained_relative_and_unexpected() {
        let tree = TempTree::new("skill-classification");
        let repository = tree.path("repository");
        let source = make_skill(&repository, "alpha-skill", "alpha");

        let broken_root = tree.path("broken");
        fs::create_dir(&broken_root).expect("broken root");
        create_file_symlink(&tree.path("absent"), &broken_root.join("alpha-skill"))
            .expect("broken link");
        assert_eq!(
            build_skill_link_plan(&repository, &[broken_root], None)
                .expect("plan")
                .entries[0]
                .installation
                .status,
            SkillInstallationStatus::BrokenLink
        );

        let chain_root = tree.path("chain");
        fs::create_dir(&chain_root).expect("chain root");
        let intermediate = tree.path("intermediate");
        create_file_symlink(&source, &intermediate).expect("intermediate");
        create_file_symlink(&intermediate, &chain_root.join("alpha-skill")).expect("chain");
        assert_eq!(
            build_skill_link_plan(&repository, &[chain_root], None)
                .expect("plan")
                .entries[0]
                .installation
                .status,
            SkillInstallationStatus::ChainedLink
        );

        let file_root = tree.path("file");
        fs::create_dir(&file_root).expect("file root");
        fs::write(file_root.join("alpha-skill"), "owned\n").expect("file");
        assert_eq!(
            build_skill_link_plan(&repository, &[file_root], None)
                .expect("plan")
                .entries[0]
                .installation
                .status,
            SkillInstallationStatus::UnexpectedFile
        );
    }

    #[test]
    fn skill_link_input_guards_reject_aliases_nesting_and_source_links() {
        let tree = TempTree::new("skill-guards");
        let repository = tree.path("repository");
        let skill = make_skill(&repository, "alpha-skill", "alpha");
        let target = tree.path("target");
        fs::create_dir(&target).expect("target");
        assert!(
            build_skill_link_plan(&repository, &[], None)
                .expect_err("missing roots")
                .to_string()
                .contains("target-root")
        );
        let nested = target.join("nested");
        fs::create_dir(&nested).expect("nested");
        assert!(
            build_skill_link_plan(&repository, &[target.clone(), nested], None)
                .expect_err("nested")
                .to_string()
                .contains("must not be nested")
        );
        let external = tree.path("external");
        fs::write(&external, "external").expect("external");
        create_file_symlink(&external, &skill.join("payload")).expect("source link");
        assert!(
            build_skill_link_plan(&repository, &[target], None)
                .expect_err("source links")
                .to_string()
                .contains("skills tree must not contain symlinks")
        );
    }

    #[test]
    fn windows_wrapper_and_powershell_rendering_are_deterministic() {
        let raw = r"c:\Users\Zoë Smith\DevCoordinator2\reference\universal\AGENTS.md";
        let expected = "C:/Users/Zoë Smith/DevCoordinator2/reference/universal/AGENTS.md";
        assert_eq!(
            serialize_windows_import_path(raw).expect("serialize"),
            expected
        );
        assert_eq!(
            claude_wrapper_bytes(raw, true).expect("wrapper"),
            format!("@{expected}\n").into_bytes()
        );
        assert!(
            serialize_windows_import_path(r"\\server\share\AGENTS.md")
                .expect_err("UNC")
                .to_string()
                .contains("UNC")
        );
        assert!(
            serialize_windows_import_path("C:\\bad\npath")
                .expect_err("control")
                .to_string()
                .contains("control")
        );
        let rendered = render_shell_command(
            &[
                r"C:\Program Files\DevCoordinator2\devcoordinator2-tooling.exe".into(),
                "apply".into(),
                "it's-safe".into(),
            ],
            true,
        );
        assert_eq!(
            rendered,
            "& 'C:\\Program Files\\DevCoordinator2\\devcoordinator2-tooling.exe' 'apply' 'it''s-safe'"
        );
        assert!(
            !String::from_utf8(claude_wrapper_bytes(raw, true).expect("wrapper"))
                .expect("UTF-8")
                .to_ascii_lowercase()
                .contains("authorized")
        );
    }

    #[test]
    fn policy_exact_install_verify_source_update_and_rollback() {
        let tree = TempTree::new("policy-happy");
        let (repository, source) = make_policy_repository(&tree.root, "repository");
        let codex_root = tree.path("codex");
        let claude_root = tree.path("claude");
        let state = tree.path("state");
        fs::create_dir(&codex_root).expect("codex root");
        fs::create_dir(&claude_root).expect("claude root");
        fs::create_dir(&state).expect("state");
        let codex = codex_root.join("AGENTS.md");
        let claude = claude_root.join("CLAUDE.md");
        fs::write(&codex, "PRIVATE EXISTING CODEX POLICY\n").expect("codex prior");
        let legacy = tree.path("legacy claude.md");
        fs::write(&legacy, "legacy\n").expect("legacy");
        create_file_symlink(&legacy, &claude).expect("legacy link");
        let codex_before = policy_snapshot(&codex, true).expect("snapshot");
        let claude_before = policy_snapshot(&claude, true).expect("snapshot");
        let transaction = state.join("transaction");
        let receipt = create_policy_plan(
            &repository,
            &transaction,
            &[PolicyTarget::codex(&codex), PolicyTarget::claude(&claude)],
        )
        .expect("plan");
        assert!(
            !receipt
                .plan
                .to_json()
                .to_string()
                .contains("PRIVATE EXISTING")
        );
        let rendered = render_policy_plan(
            &receipt,
            Path::new("/usr/local/bin/devcoordinator2-tooling"),
        );
        assert!(rendered.contains("runtime_activation: not-observed"));
        assert!(rendered.contains(&receipt.digest));

        let applied = apply_policy_transaction(&transaction, &receipt.digest).expect("apply");
        assert_eq!(applied.status, "applied");
        assert!(absolute_link_matches(&codex, &source));
        assert!(absolute_link_matches(&claude, &source));
        assert_eq!(
            verify_policy_transaction(&transaction, &receipt.digest)
                .expect("verify")
                .filesystem_state,
            "installed"
        );

        let replacement = source.with_extension("new");
        fs::write(&replacement, "# Universal Agent Instructions\nUpdated.\n").expect("replacement");
        fs::rename(&replacement, &source).expect("replace source");
        assert_eq!(
            verify_policy_transaction(&transaction, &receipt.digest)
                .expect("verify changed source")
                .canonical_source_changed_since_plan,
            Some(true)
        );

        let rolled_back =
            rollback_policy_transaction(&transaction, &receipt.digest).expect("rollback");
        assert_eq!(rolled_back.filesystem_state, "rolled-back");
        assert_eq!(
            policy_snapshot(&codex, true).expect("snapshot"),
            codex_before
        );
        assert_eq!(
            policy_snapshot(&claude, true).expect("snapshot"),
            claude_before
        );
        rollback_policy_transaction(&transaction, &receipt.digest).expect("idempotent");
    }

    #[test]
    fn policy_retain_does_not_touch_unrelated_siblings() {
        let tree = TempTree::new("policy-retain");
        let (repository, source) = make_policy_repository(&tree.root, "repository");
        let runtime = tree.path("runtime");
        let state = tree.path("state");
        fs::create_dir(&runtime).expect("runtime");
        fs::create_dir(&state).expect("state");
        let target = runtime.join("AGENTS.md");
        create_file_symlink(&source, &target).expect("direct");
        fs::write(runtime.join("unrelated.bin"), b"do not touch\0sibling").expect("sibling");
        let before = policy_snapshot(&target, true).expect("before");
        let receipt = create_policy_plan(
            &repository,
            &state.join("transaction"),
            &[PolicyTarget::codex(&target)],
        )
        .expect("plan");
        assert_eq!(receipt.plan.entries[0].action, PolicyAction::Retain);
        apply_policy_transaction(&receipt.plan.transaction_dir, &receipt.digest).expect("apply");
        assert_eq!(policy_snapshot(&target, true).expect("after"), before);
        assert_eq!(
            fs::read(runtime.join("unrelated.bin")).expect("sibling"),
            b"do not touch\0sibling"
        );
        rollback_policy_transaction(&receipt.plan.transaction_dir, &receipt.digest)
            .expect("rollback");
        assert_eq!(policy_snapshot(&target, true).expect("rolled back"), before);
    }

    #[test]
    fn policy_rejects_source_target_and_plan_drift() {
        let tree = TempTree::new("policy-drift");
        let (repository, source) = make_policy_repository(&tree.root, "repository");
        let runtime = tree.path("runtime");
        let state = tree.path("state");
        fs::create_dir(&runtime).expect("runtime");
        fs::create_dir(&state).expect("state");
        let target = runtime.join("AGENTS.md");
        fs::write(&target, "before\n").expect("prior");
        let receipt = create_policy_plan(
            &repository,
            &state.join("source-transaction"),
            &[PolicyTarget::codex(&target)],
        )
        .expect("plan");
        fs::write(&source, "changed in place\n").expect("source drift");
        assert!(
            apply_policy_transaction(&receipt.plan.transaction_dir, &receipt.digest)
                .expect_err("source drift")
                .to_string()
                .contains("changed after planning")
        );
        assert_eq!(fs::read_to_string(&target).expect("target"), "before\n");

        let (repository2, _) = make_policy_repository(&tree.root, "repository-two");
        let target2 = runtime.join("AGENTS-two.md");
        assert!(
            create_policy_plan(
                &repository2,
                &state.join("wrong-name"),
                &[PolicyTarget::codex(target2)],
            )
            .expect_err("wrong basename")
            .to_string()
            .contains("AGENTS.md")
        );
    }

    #[test]
    fn policy_partial_failure_rolls_back_every_target() {
        let tree = TempTree::new("policy-partial");
        let (repository, _) = make_policy_repository(&tree.root, "repository");
        let first_root = tree.path("first");
        let second_root = tree.path("second");
        let state = tree.path("state");
        fs::create_dir(&first_root).expect("first");
        fs::create_dir(&second_root).expect("second");
        fs::create_dir(&state).expect("state");
        let first = first_root.join("AGENTS.md");
        let second = second_root.join("CLAUDE.md");
        fs::write(&first, "first before\n").expect("first prior");
        fs::write(&second, "second before\n").expect("second prior");
        let first_before = policy_snapshot(&first, true).expect("first snapshot");
        let second_before = policy_snapshot(&second, true).expect("second snapshot");
        let receipt = create_policy_plan(
            &repository,
            &state.join("transaction"),
            &[PolicyTarget::codex(&first), PolicyTarget::claude(&second)],
        )
        .expect("plan");
        let second_id = receipt
            .plan
            .entries
            .iter()
            .find(|entry| entry.role == PolicyRole::Claude)
            .expect("Claude entry")
            .id
            .clone();
        let mut hook = move |point: &str, entry_id: &str| {
            if point == "after-backup" && entry_id == second_id {
                Err(LinkError::new(
                    LinkErrorKind::Drift,
                    "simulated second-target failure",
                ))
            } else {
                Ok(())
            }
        };
        assert!(
            apply_policy_transaction_with_hook(
                &receipt.plan.transaction_dir,
                &receipt.digest,
                Some(&mut hook),
            )
            .is_err()
        );
        assert_eq!(policy_snapshot(&first, true).expect("first"), first_before);
        assert_eq!(
            policy_snapshot(&second, true).expect("second"),
            second_before
        );
    }

    #[test]
    fn policy_tamper_parent_swap_and_backup_collision_fail_closed() {
        let tree = TempTree::new("policy-tamper");
        let (repository, _) = make_policy_repository(&tree.root, "repository");
        let runtime = tree.path("runtime");
        let state = tree.path("state");
        fs::create_dir(&runtime).expect("runtime");
        fs::create_dir(&state).expect("state");
        let target = runtime.join("AGENTS.md");
        fs::write(&target, "before\n").expect("prior");

        let receipt = create_policy_plan(
            &repository,
            &state.join("collision"),
            &[PolicyTarget::codex(&target)],
        )
        .expect("plan");
        fs::write(&receipt.plan.entries[0].backup, "external collision\n").expect("collision");
        assert!(
            apply_policy_transaction(&receipt.plan.transaction_dir, &receipt.digest)
                .expect_err("collision")
                .to_string()
                .contains("not empty")
        );
        assert_eq!(fs::read_to_string(&target).expect("target"), "before\n");

        let receipt2 = create_policy_plan(
            &repository,
            &state.join("tamper"),
            &[PolicyTarget::codex(&target)],
        )
        .expect("plan");
        let plan_path = receipt2.plan.transaction_dir.join(POLICY_PLAN_NAME);
        let (mut plan_value, _) = read_json(&plan_path, "plan").expect("plan JSON");
        plan_value["entries"][0]["backup"] =
            Value::String(tree.path("unrelated-stolen").to_string_lossy().into_owned());
        let plan_payload = canonical_json(&plan_value).expect("plan JSON");
        let changed_digest = hex(&sha256(&plan_payload));
        atomic_write(&plan_path, &plan_payload, 0o400).expect("replace plan");
        let journal_path = receipt2.plan.transaction_dir.join(POLICY_JOURNAL_NAME);
        let (mut journal_value, _) = read_json(&journal_path, "journal").expect("journal");
        journal_value["plan_sha256"] = Value::String(changed_digest.clone());
        atomic_write(
            &journal_path,
            &canonical_json(&journal_value).expect("journal JSON"),
            0o600,
        )
        .expect("replace journal");
        assert!(
            apply_policy_transaction(&receipt2.plan.transaction_dir, &changed_digest)
                .expect_err("derived path tamper")
                .to_string()
                .contains("backup")
        );

        let receipt3 = create_policy_plan(
            &repository,
            &state.join("parent-swap"),
            &[PolicyTarget::codex(&target)],
        )
        .expect("plan");
        let moved = tree.path("runtime-old");
        fs::rename(&runtime, &moved).expect("move parent");
        fs::create_dir(&runtime).expect("replacement parent");
        fs::write(runtime.join("AGENTS.md"), "replacement parent\n").expect("replacement");
        assert!(
            apply_policy_transaction(&receipt3.plan.transaction_dir, &receipt3.digest)
                .expect_err("parent identity")
                .to_string()
                .contains("identity changed")
        );
        assert_eq!(
            fs::read_to_string(runtime.join("AGENTS.md")).expect("replacement"),
            "replacement parent\n"
        );
        assert_eq!(
            fs::read_to_string(moved.join("AGENTS.md")).expect("original"),
            "before\n"
        );
    }

    #[test]
    fn policy_interrupted_rollback_is_resumable_without_deleting_capture() {
        let tree = TempTree::new("policy-rollback-resume");
        let (repository, _) = make_policy_repository(&tree.root, "repository");
        let runtime = tree.path("runtime");
        let state = tree.path("state");
        fs::create_dir(&runtime).expect("runtime");
        fs::create_dir(&state).expect("state");
        let target = runtime.join("AGENTS.md");
        fs::write(&target, "prior\n").expect("prior");
        let before = policy_snapshot(&target, true).expect("before");
        let receipt = create_policy_plan(
            &repository,
            &state.join("transaction"),
            &[PolicyTarget::codex(&target)],
        )
        .expect("plan");
        apply_policy_transaction(&receipt.plan.transaction_dir, &receipt.digest).expect("apply");
        let mut interrupted = |point: &str, _: &str| {
            if point == "after-rollback-capture" {
                Err(LinkError::new(
                    LinkErrorKind::Drift,
                    "simulated abrupt rollback boundary",
                ))
            } else {
                Ok(())
            }
        };
        assert!(
            rollback_policy_transaction_with_hook(
                &receipt.plan.transaction_dir,
                &receipt.digest,
                Some(&mut interrupted),
            )
            .is_err()
        );
        assert!(!lexical_exists(&target));
        assert!(policy_matches_desired(
            &receipt.plan.entries[0].temporary,
            &receipt.plan.entries[0].desired
        ));
        let recovered = rollback_policy_transaction(&receipt.plan.transaction_dir, &receipt.digest)
            .expect("resume rollback");
        assert_eq!(recovered.status, "rolled-back");
        assert_eq!(policy_snapshot(&target, true).expect("restored"), before);
        assert_eq!(recovered.retained_artifacts.len(), 1);
    }

    #[test]
    fn policy_rollback_preflights_all_targets_before_mutation() {
        let tree = TempTree::new("policy-all-action");
        let (repository, source) = make_policy_repository(&tree.root, "repository");
        let first_root = tree.path("first");
        let second_root = tree.path("second");
        let state = tree.path("state");
        fs::create_dir(&first_root).expect("first");
        fs::create_dir(&second_root).expect("second");
        fs::create_dir(&state).expect("state");
        let first = first_root.join("AGENTS.md");
        let second = second_root.join("CLAUDE.md");
        fs::write(&first, "first before\n").expect("first prior");
        fs::write(&second, "second before\n").expect("second prior");
        let receipt = create_policy_plan(
            &repository,
            &state.join("transaction"),
            &[PolicyTarget::codex(&first), PolicyTarget::claude(&second)],
        )
        .expect("plan");
        apply_policy_transaction(&receipt.plan.transaction_dir, &receipt.digest).expect("apply");
        fs::remove_file(&second).expect("remove second");
        fs::write(&second, "external post-apply drift\n").expect("drift");
        assert!(
            rollback_policy_transaction(&receipt.plan.transaction_dir, &receipt.digest)
                .expect_err("drift blocks rollback")
                .to_string()
                .contains("drift blocks")
        );
        assert!(absolute_link_matches(&first, &source));
        assert_eq!(
            fs::read_to_string(&second).expect("drift"),
            "external post-apply drift\n"
        );
    }
}
