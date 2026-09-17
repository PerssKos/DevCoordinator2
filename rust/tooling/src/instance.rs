//! Minimal thin-tool instance lookup without loading daemon-private policy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const INSTALLED_ENV: &str = "/etc/devcoordinator2/instance.env";
const DEFAULT_SOCKET: &str = "/run/devcoordinator2/daemon.sock";

pub fn socket_path() -> PathBuf {
    if let Some(value) = std::env::var_os("DEVCOORDINATOR2_SOCKET").filter(|v| !v.is_empty()) {
        return value.into();
    }
    let source = std::env::var_os("DEVCOORDINATOR2_INSTANCE_ENV")
        .map(PathBuf::from)
        .or_else(|| {
            let local = PathBuf::from(".env");
            local.is_file().then_some(local)
        })
        .unwrap_or_else(|| PathBuf::from(INSTALLED_ENV));
    parse_env(&source)
        .remove("DEVCOORDINATOR2_SOCKET")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
}

pub(crate) fn parse_env(path: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|raw| {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, raw_value) = line.split_once('=')?;
            let key = key.trim();
            if !key.starts_with("DEVCOORDINATOR2_") {
                return None;
            }
            let mut value = raw_value.trim();
            if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                value = &value[1..value.len() - 1];
            }
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_coordinator_values_and_quotes() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("instance.env");
        std::fs::write(
            &path,
            "# ignored\nDEVCOORDINATOR2_SOCKET='/tmp/example.sock'\nOTHER=x\n",
        )
        .unwrap();
        assert_eq!(
            parse_env(&path).get("DEVCOORDINATOR2_SOCKET"),
            Some(&"/tmp/example.sock".to_owned())
        );
        assert!(!parse_env(&path).contains_key("OTHER"));
    }
}
