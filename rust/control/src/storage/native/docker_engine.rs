//! Bounded reads from the configured local Docker Engine API. Buildx 0.13 does
//! not expose JSON disk-usage output; the Engine contract already does.
use super::HostBackend;
use crate::storage::{blocked, unavailable};
use devcoordinator2_api::ProtocolError;
use serde_json::Value;
use std::{io::Read, path::Path, time::Duration};

impl HostBackend {
    fn engine_client(&self) -> Result<reqwest::blocking::Client, ProtocolError> {
        let socket = if let Some(socket) = self.fixture_engine_socket()? {
            socket
        } else {
            let endpoint = self.docker_output(vec![
                "context".into(),
                "inspect".into(),
                "default".into(),
                "--format".into(),
                "{{json .Endpoints.docker.Host}}".into(),
            ])?;
            let endpoint: String = serde_json::from_str(endpoint.trim())
                .map_err(|_| unavailable("docker_endpoint_unverified"))?;
            let socket = endpoint
                .strip_prefix("unix://")
                .filter(|p| Path::new(p).is_absolute())
                .ok_or_else(|| blocked("remote_builder_requires_provider"))?;
            Path::new(socket).to_path_buf()
        };
        reqwest::blocking::Client::builder()
            .unix_socket(socket)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| unavailable("builder_observation_unavailable"))
    }

    pub(super) fn engine_cache_rows(&self) -> Result<Vec<Value>, ProtocolError> {
        let response = self
            .engine_client()?
            .get("http://localhost/v1.40/system/df?type=build-cache")
            .send()
            .map_err(|_| unavailable("builder_observation_unavailable"))?;
        let value = read_json(response)?;
        let rows = value
            .get("BuildCache")
            .and_then(Value::as_array)
            .filter(|rows| rows.len() <= 10000)
            .ok_or_else(|| unavailable("builder_metadata_invalid"))?;
        Ok(rows.clone())
    }

    pub(super) fn engine_data_root(&self) -> Result<std::path::PathBuf, ProtocolError> {
        let response = self
            .engine_client()?
            .get("http://localhost/v1.40/info")
            .send()
            .map_err(|_| unavailable("builder_observation_unavailable"))?;
        let value = read_json(response)?;
        value
            .get("DockerRootDir")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| unavailable("builder_filesystem_unavailable"))
    }

    pub(super) fn engine_cache_remove(&self, id: &str) -> Result<(), ProtocolError> {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b":._-".contains(&c))
        {
            return Err(blocked("build_cache_identity_unverified"));
        }
        let mut url = reqwest::Url::parse("http://localhost/v1.40/build/prune")
            .map_err(|_| unavailable("builder_endpoint_invalid"))?;
        let filters = serde_json::json!({"id":[id]}).to_string();
        url.query_pairs_mut().append_pair("filters", &filters);
        let response = self
            .engine_client()?
            .post(url)
            .send()
            .map_err(|_| unavailable("build_cache_removal_failed"))?;
        let result = read_json(response)?;
        let removed = result
            .get("CachesDeleted")
            .and_then(Value::as_array)
            .ok_or_else(|| unavailable("build_cache_receipt_invalid"))?;
        if removed.iter().any(|value| value.as_str() != Some(id)) {
            return Err(unavailable("build_cache_receipt_scope_changed"));
        }
        if !removed.iter().any(|value| value.as_str() == Some(id)) {
            return Err(blocked("build_cache_not_reclaimable"));
        }
        Ok(())
    }
}

fn read_json(response: reqwest::blocking::Response) -> Result<Value, ProtocolError> {
    if !response.status().is_success() {
        return Err(unavailable("builder_observation_unavailable"));
    }
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    let mut bytes = Vec::new();
    response
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable("builder_observation_incomplete"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(unavailable("builder_observation_limit"));
    }
    serde_json::from_slice(&bytes).map_err(|_| unavailable("builder_metadata_invalid"))
}
