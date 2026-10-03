//! Filesystem cleanup is Linux-native; other clients keep the typed interface.
#[derive(Clone, Debug)]
pub struct Measurement {
    pub device: u64,
    pub inode: u64,
    pub bytes: u64,
    pub newest_modified_ns: i128,
    pub entries: usize,
    pub nested_git: bool,
    pub protected_metadata: bool,
    pub multiply_linked: bool,
}

pub(crate) fn protected_metadata_name(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".ssh"
                | ".aws"
                | ".gnupg"
                | "sessions"
                | "archived_sessions"
                | ".env"
                | ".netrc"
                | ".npmrc"
                | "auth.json"
                | "credentials.json"
                | "id_rsa"
                | "id_ed25519"
        )
    )
}

#[cfg(target_os = "linux")]
#[path = "fs_linux.rs"]
mod platform;
#[cfg(not(target_os = "linux"))]
#[path = "fs_unavailable.rs"]
mod platform;
pub use platform::*;
