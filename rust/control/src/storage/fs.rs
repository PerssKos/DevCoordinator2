//! Filesystem cleanup is Linux-native; other clients keep the typed interface.
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

#[cfg(target_os = "linux")]
#[path = "fs_linux.rs"]
mod platform;
#[cfg(not(target_os = "linux"))]
#[path = "fs_unavailable.rs"]
mod platform;
pub use platform::*;
