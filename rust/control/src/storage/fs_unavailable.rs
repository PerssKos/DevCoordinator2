//! No weaker traversal fallback on platforms without Linux mount guarantees.
use super::Measurement;
use crate::storage::unavailable;
use devcoordinator2_api::{ProtocolError, storage::Filesystem};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

fn unsupported<T>() -> Result<T, ProtocolError> {
    Err(unavailable("storage_filesystem_requires_linux"))
}

pub fn open_directory(_: &Path) -> Result<File, ProtocolError> {
    unsupported()
}
pub fn identity(_: &Path) -> Result<(u64, u64), ProtocolError> {
    unsupported()
}
pub fn absent(_: &Path) -> Result<bool, ProtocolError> {
    unsupported()
}
pub fn measure(_: &Path) -> Result<Measurement, ProtocolError> {
    unsupported()
}
pub fn mount_targets() -> Result<Vec<PathBuf>, ProtocolError> {
    unsupported()
}
pub fn remove_tree(_: &Path, _: (u64, u64), _: bool) -> Result<(), ProtocolError> {
    unsupported()
}
pub fn filesystem(_: &Path, _: u64, _: &str) -> Result<Filesystem, ProtocolError> {
    unsupported()
}
pub fn unescape_mount(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}
