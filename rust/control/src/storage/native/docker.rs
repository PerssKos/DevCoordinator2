use super::{HostBackend, candidate, processes_reference};
use crate::docker::DockerInvocation;
use crate::storage::{
    blocked, fs,
    model::{Context, Discovery, Locator, Record},
    unavailable,
};
use devcoordinator2_api::{ProtocolError, storage as api};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

const CONTAINER_FORMAT: &str = r#"{"id":{{json .Id}},"name":{{json .Name}},"created":{{json .Created}},"state":{{json .State.Status}},"finished":{{json .State.FinishedAt}},"image":{{json .Image}},"instance":{{json (index .Config.Labels "devcoordinator2.instance")}},"repository":{{json (index .Config.Labels "devcoordinator2.repository")}},"deployment":{{json (index .Config.Labels "devcoordinator2.deployment")}},"project":{{json (index .Config.Labels "com.docker.compose.project")}},"mounts":[{{range $i,$m := .Mounts}}{{if $i}},{{end}}{"type":{{json $m.Type}},"name":{{json (index $m "Name")}},"source":{{json (index $m "Source")}}}{{end}}]}"#;
const VOLUME_FORMAT: &str = r#"{"name":{{json .Name}},"created":{{json .CreatedAt}},"driver":{{json .Driver}},"mountpoint":{{json .Mountpoint}},"options":{{len .Options}},"project":{{json (index .Labels "com.docker.compose.project")}}}"#;
const IMAGE_FORMAT: &str = r#"{"id":{{json .Id}},"created":{{json .Created}},"size":{{json .Size}},"tags":{{json .RepoTags}},"digests":{{json .RepoDigests}}}"#;
const NETWORK_FORMAT: &str = r#"{"id":{{json .Id}},"name":{{json .Name}},"created":{{json .Created}},"driver":{{json .Driver}},"project":{{json (index .Labels "com.docker.compose.project")}},"containers":[{{$sep := ""}}{{range $i,$c := .Containers}}{{$sep}}{{json $i}}{{$sep = ","}}{{end}}]}"#;

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn inactive(v: &Value) -> bool {
    matches!(s(v, "state"), "exited" | "dead" | "created")
}

impl HostBackend {
    fn fixture_namespace(&self) -> Option<&str> {
        #[cfg(feature = "root-acceptance")]
        if self
            .config
            .unit_prefix
            .starts_with("devcoordinator2-rustint-")
        {
            return Some(&self.config.unit_prefix);
        }
        None
    }

    fn discovery_arguments(&self, mut args: Vec<String>) -> Vec<String> {
        if let Some(namespace) = self.fixture_namespace() {
            args.extend([
                "--filter".into(),
                format!("label=devcoordinator2.instance={namespace}"),
            ]);
        }
        args
    }

    pub(super) fn save_private_definition(
        &self,
        r: &Record,
        job_id: &str,
    ) -> Result<(), ProtocolError> {
        let Locator::Docker {
            object_type,
            identity,
            ..
        } = &r.locator
        else {
            return Ok(());
        };
        if object_type == "build_cache" {
            return Ok(());
        }
        let directory = self.config.state_dir.join("storage-recovery");
        std::fs::create_dir_all(&directory)
            .map_err(|_| unavailable("private_recovery_unavailable"))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| unavailable("private_recovery_unavailable"))?;
        let path = super::mounts::request_path(&directory, job_id, &r.artifact.artifact_id)?;
        if path.exists() {
            // Never overwrite the original definition with a partially retired
            // resource. Metadata stays private and outside normal logs/results.
            super::mounts::private_read(&path, 16 * 1024 * 1024, true)?;
            return Ok(());
        }
        let definition = self.docker_output(vec![
            object_type.clone(),
            "inspect".into(),
            identity.clone(),
        ])?;
        let _: Value = serde_json::from_str(&definition)
            .map_err(|_| unavailable("private_recovery_invalid"))?;
        super::mounts::write_private_new(&path, definition.as_bytes())
    }

    fn containers_for(
        &self,
        context: &Context,
    ) -> Result<std::sync::Arc<Vec<Value>>, ProtocolError> {
        if let Some(rows) = &context.docker_snapshot {
            return Ok(rows.clone());
        }
        if context.docker_unavailable {
            return Err(unavailable("docker_observation_incomplete"));
        }
        self.container_rows().map(std::sync::Arc::new)
    }

    pub(super) fn validate_mount_consumers(
        &self,
        r: &Record,
        context: &Context,
        selected: &[String],
    ) -> Result<(), ProtocolError> {
        let expected = r
            .resource_key
            .strip_prefix("fs:")
            .and_then(|s| s.split_once(':'))
            .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)));
        let Some(expected) = expected else {
            return Ok(());
        };
        for container in self.containers_for(context)?.iter() {
            let locator = Locator::Docker {
                object_type: "container".into(),
                identity: s(container, "id").into(),
                created: s(container, "created").into(),
            };
            let container_id =
                crate::storage::stable_id("sa", crate::storage::json(&locator)?.as_bytes());
            let current = context
                .current_container_ids
                .iter()
                .any(|id| id == s(container, "id"))
                || context
                    .current_projects
                    .contains_key(s(container, "project"));
            if inactive(container) && !current && selected.contains(&container_id) {
                continue;
            }
            for mount in array(container, "mounts") {
                let source = std::path::Path::new(s(mount, "source"));
                if !source.is_absolute() {
                    continue;
                }
                let mut related = r
                    .private_aliases
                    .iter()
                    .any(|path| path.starts_with(source) || source.starts_with(path));
                let mut parent = Some(source);
                while !related {
                    let Some(path) = parent else {
                        break;
                    };
                    related = fs::identity(path).is_ok_and(|identity| identity == expected);
                    parent = path.parent();
                }
                if related {
                    return Err(blocked(if current || !inactive(container) {
                        "active_consumer"
                    } else {
                        "consumer_not_in_cleanup"
                    }));
                }
            }
        }
        Ok(())
    }

    fn docker_output(&self, args: Vec<String>) -> Result<String, ProtocolError> {
        let request = DockerInvocation::new(
            args.into_iter().map(Into::into).collect(),
            Duration::from_secs(120),
        )
        .map_err(|_| blocked("invalid_native_identity"))?;
        let output = self
            .docker
            .invoke(request)
            .map_err(|_| unavailable("docker_unavailable"))?;
        if !output.success() || output.stdout_truncated {
            return Err(unavailable("docker_observation_incomplete"));
        }
        Ok(output.stdout)
    }

    fn inspect_rows(
        &self,
        kind: &str,
        ids: &[String],
        format: &str,
    ) -> Result<Vec<Value>, ProtocolError> {
        let mut result = Vec::new();
        for batch in ids.chunks(32) {
            let mut args = vec![
                kind.into(),
                "inspect".into(),
                "--format".into(),
                format.into(),
            ];
            args.extend(batch.iter().cloned());
            for line in self.docker_output(args)?.lines() {
                result.push(
                    serde_json::from_str(line)
                        .map_err(|_| unavailable("docker_metadata_invalid"))?,
                );
            }
        }
        Ok(result)
    }

    pub(super) fn container_rows(&self) -> Result<Vec<Value>, ProtocolError> {
        let text = self.docker_output(vec![
            "ps".into(),
            "--all".into(),
            "--quiet".into(),
            "--no-trunc".into(),
        ])?;
        let ids = text
            .lines()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if ids.len() > 10_000 {
            return Err(unavailable("docker_inventory_limit"));
        }
        self.inspect_rows("container", &ids, CONTAINER_FORMAT)
    }

    pub(super) fn discover_docker(
        &self,
        context: &Context,
        out: &mut Discovery,
    ) -> Result<(), ProtocolError> {
        let containers = self.containers_for(context)?;
        let mut volume_owners: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
        for c in containers.iter() {
            if let Some((repository, deployment)) = context.recorded_containers.get(s(c, "id")) {
                for m in array(c, "mounts")
                    .iter()
                    .filter(|m| s(m, "type") == "volume")
                {
                    volume_owners
                        .entry(s(m, "name").into())
                        .or_default()
                        .insert((repository.clone(), deployment.clone()));
                }
            }
        }
        let mut container_records = BTreeMap::new();
        for c in containers.iter() {
            if self
                .fixture_namespace()
                .is_some_and(|namespace| s(c, "instance") != namespace)
            {
                continue;
            }
            let id = s(c, "id");
            let mut r = candidate(
                api::Kind::Container,
                api::Effect::RuntimeResource,
                s(c, "name").trim_start_matches('/').into(),
                Locator::Docker {
                    object_type: "container".into(),
                    identity: id.into(),
                    created: s(c, "created").into(),
                },
                context,
            )?;
            let exact = context.recorded_containers.get(id).cloned();
            let dependent = if exact.is_none() {
                array(c, "mounts")
                    .iter()
                    .filter_map(|m| volume_owners.get(s(m, "name")))
                    .flatten()
                    .find(|(_, dep)| {
                        context
                            .observed
                            .get(dep)
                            .is_some_and(|(_, project)| project == s(c, "project"))
                    })
                    .cloned()
            } else {
                None
            };
            if let Some((repo, dep)) = exact.or(dependent) {
                assign(&mut r, &repo, Some(&dep), context);
                r.artifact.ownership = "observed_cleanup_candidate".into();
                r.blockers.push("disposal_not_authorized".into());
            } else if let Some(repo) = context.deployment_repositories.get(s(c, "deployment")) {
                assign(&mut r, repo, Some(s(c, "deployment")), context);
            } else if let Some(dep) = context.current_projects.get(s(c, "project")) {
                if let Some(repo) = context.deployment_repositories.get(dep) {
                    assign(&mut r, repo, Some(dep), context);
                }
            } else if context
                .repositories
                .iter()
                .any(|r| r.id == s(c, "repository"))
            {
                assign(&mut r, s(c, "repository"), None, context);
            } else {
                r.blockers.push("ownership_unknown".into());
                r.discovered_effect = api::Effect::Unknown;
            }
            if !inactive(c) {
                r.blockers.push("active_consumer".into());
            }
            if context.current_container_ids.iter().any(|v| v == id)
                || r.owner_deployment
                    .as_ref()
                    .is_some_and(|d| context.deployment_repositories.contains_key(d))
            {
                r.blockers.push("current_deployment".into());
            }
            r.last_activity_signature = format!("{}:{}", s(c, "state"), s(c, "finished"));
            r.artifact.last_used_at_ms = date_ms(s(c, "finished")).filter(|n| *n > 0);
            container_records.insert(id.to_owned(), r.clone());
            out.records.push(r);
        }
        let mut volumes = self
            .docker_output(self.discovery_arguments(vec![
                "volume".into(),
                "ls".into(),
                "--quiet".into(),
            ]))?
            .lines()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if self.fixture_namespace().is_some() {
            // Managed persistent volumes need not carry the fixture label, but
            // their exact consumers do. Include only those owned references.
            for container in containers
                .iter()
                .filter(|c| container_records.contains_key(s(c, "id")))
            {
                for mount in array(container, "mounts")
                    .iter()
                    .filter(|m| s(m, "type") == "volume")
                {
                    volumes.insert(s(mount, "name").to_owned());
                }
            }
        }
        let volumes = volumes.into_iter().collect::<Vec<_>>();
        let mount_entries = self.mount_entries()?;
        for v in self.inspect_rows("volume", &volumes, VOLUME_FORMAT)? {
            let name = s(&v, "name");
            let path = PathBuf::from(s(&v, "mountpoint"));
            let mut r = candidate(
                api::Kind::Volume,
                api::Effect::PermanentData,
                name.into(),
                Locator::Docker {
                    object_type: "volume".into(),
                    identity: name.into(),
                    created: s(&v, "created").into(),
                },
                context,
            )?;
            let consumers = containers
                .iter()
                .filter(|c| {
                    array(c, "mounts")
                        .iter()
                        .any(|m| s(m, "type") == "volume" && s(m, "name") == name)
                })
                .collect::<Vec<_>>();
            let owners = consumers
                .iter()
                .filter_map(|c| container_records.get(s(c, "id")))
                .filter_map(|r| {
                    r.artifact
                        .repository_id
                        .as_ref()
                        .map(|repo| (repo.clone(), r.owner_deployment.clone()))
                })
                .collect::<BTreeSet<_>>();
            if owners.len() == 1 {
                let (repo, dep) = owners.iter().next().unwrap();
                assign(&mut r, repo, dep.as_deref(), context);
            } else {
                r.blockers.push(
                    if owners.is_empty() {
                        "ownership_unknown"
                    } else {
                        "shared_owners"
                    }
                    .into(),
                );
                r.discovered_effect = api::Effect::Unknown;
            }
            if context
                .scan_repository_id
                .as_ref()
                .is_some_and(|id| r.artifact.repository_id.as_ref() != Some(id))
            {
                continue;
            }
            for c in consumers {
                if !inactive(c) {
                    r.blockers.push("active_consumer".into());
                }
                if let Some(container) = container_records.get(s(c, "id")) {
                    r.artifact
                        .dependencies
                        .push(container.artifact.artifact_id.clone());
                    if container.blockers.iter().any(|s| s == "current_deployment") {
                        r.blockers.push("current_deployment".into());
                    }
                }
            }
            if r.owner_deployment
                .as_ref()
                .is_some_and(|id| context.observed.contains_key(id))
            {
                r.blockers.push("disposal_not_authorized".into());
                r.artifact.ownership = "observed_cleanup_candidate".into();
            }
            if s(&v, "driver") != "local"
                || v.get("options").and_then(Value::as_u64).unwrap_or(0) != 0
            {
                r.blockers.push("external_volume_driver".into());
            }
            match fs::measure(&path) {
                Ok(m) => {
                    r.resource_key = format!("fs:{}:{}", m.device, m.inode);
                    r.artifact.filesystem_id = Some(format!("fs-{}", m.device));
                    r.artifact.allocated_bytes = Some(m.bytes);
                    r.last_activity_signature =
                        format!("{}:{}:{}", m.newest_modified_ns, m.bytes, m.entries);
                }
                Err(_) => r.blockers.push("volume_measurement_unavailable".into()),
            }
            r.private_aliases.push(path.clone());
            if let Err(error) = self.validate_mount_consumers(&r, context, &r.artifact.dependencies)
            {
                r.blockers.push(error.message);
            }
            for entry in mount_entries.iter().filter(|m| m.target == path) {
                let mut mount = self.mount_candidate(entry, context)?;
                mount.owner_deployment = r.owner_deployment.clone();
                mount.artifact.repository_id = r.artifact.repository_id.clone();
                mount.artifact.repository_name = r.artifact.repository_name.clone();
                mount.artifact.group_id = r.artifact.group_id.clone();
                mount.artifact.group_name = r.artifact.group_name.clone();
                mount.artifact.dependencies = r.artifact.dependencies.clone();
                mount.blockers.extend(r.blockers.clone());
                mount.artifact.ownership = r.artifact.ownership.clone();
                let mount_id = mount.artifact.artifact_id.clone();
                r.artifact.dependencies.push(mount_id);
                r.private_aliases.push(entry.source.clone());
                let parent = entry
                    .source
                    .parent()
                    .ok_or_else(|| blocked("invalid_backing_path"))?;
                match self.directory_candidate(
                    &entry.source,
                    parent,
                    None,
                    api::Kind::BackingDirectory,
                    true,
                    context,
                ) {
                    Ok(mut backing) => {
                        backing.owner_deployment = r.owner_deployment.clone();
                        backing.artifact.repository_id = r.artifact.repository_id.clone();
                        backing.artifact.repository_name = r.artifact.repository_name.clone();
                        backing.artifact.group_id = r.artifact.group_id.clone();
                        backing.artifact.group_name = r.artifact.group_name.clone();
                        backing.artifact.name = format!("{} backing data", name);
                        backing.private_aliases.push(path.clone());
                        if self
                            .validate_mount_consumers(&backing, context, &r.artifact.dependencies)
                            .is_ok()
                        {
                            backing
                                .blockers
                                .retain(|code| code != "consumer_not_in_cleanup");
                        }
                        backing.artifact.dependencies = vec![r.artifact.artifact_id.clone()];
                        backing.blockers.extend(r.blockers.clone());
                        backing.artifact.ownership = r.artifact.ownership.clone();
                        out.records.push(backing);
                    }
                    Err(_) => {
                        r.blockers.push("backing_data_unverified".into());
                        out.coverage_gaps
                            .push("backing_data_observation_incomplete".into());
                    }
                }
                out.records.push(mount);
            }
            out.records.push(r);
        }
        self.discover_images_networks(context, out, &containers, &container_records)?;
        if context.scan_repository_id.is_none() {
            self.discover_build_cache(context, out);
        }
        if let Some(repo) = &context.scan_repository_id {
            let mut keep = out
                .records
                .iter()
                .filter(|r| r.artifact.repository_id.as_ref() == Some(repo))
                .map(|r| r.artifact.artifact_id.clone())
                .collect::<BTreeSet<_>>();
            loop {
                let before = keep.len();
                for r in &out.records {
                    if keep.contains(&r.artifact.artifact_id) {
                        keep.extend(r.artifact.dependencies.clone());
                    }
                }
                if keep.len() == before {
                    break;
                }
            }
            out.records
                .retain(|r| keep.contains(&r.artifact.artifact_id));
        }
        Ok(())
    }

    fn discover_images_networks(
        &self,
        context: &Context,
        out: &mut Discovery,
        containers: &[Value],
        known: &BTreeMap<String, Record>,
    ) -> Result<(), ProtocolError> {
        let ids = self
            .docker_output(self.discovery_arguments(vec![
                "image".into(),
                "ls".into(),
                "--quiet".into(),
                "--no-trunc".into(),
            ]))?
            .lines()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        for image in self.inspect_rows("image", &ids, IMAGE_FORMAT)? {
            let id = s(&image, "id");
            let name = array(&image, "tags")
                .first()
                .and_then(Value::as_str)
                .unwrap_or(id);
            let mut r = candidate(
                api::Kind::Image,
                api::Effect::Rebuildable,
                name.into(),
                Locator::Docker {
                    object_type: "image".into(),
                    identity: id.into(),
                    created: s(&image, "created").into(),
                },
                context,
            )?;
            r.artifact.allocated_bytes = None; // Image sizes include shared layers, not independently reclaimable bytes.
            r.artifact.ownership = "unknown".into();
            for c in containers.iter().filter(|c| s(c, "image") == id) {
                if let Some(c) = known.get(s(c, "id")) {
                    r.artifact.dependencies.push(c.artifact.artifact_id.clone());
                    if c.blockers
                        .iter()
                        .any(|b| b == "current_deployment" || b == "active_consumer")
                    {
                        r.blockers.push("active_consumer".into());
                    }
                }
            }
            if array(&image, "tags")
                .iter()
                .chain(array(&image, "digests"))
                .filter_map(Value::as_str)
                .any(|tag| context.current_images.iter().any(|i| i == tag))
            {
                r.blockers.push("current_deployment".into());
            }
            let owners = containers
                .iter()
                .filter(|c| s(c, "image") == id)
                .filter_map(|c| known.get(s(c, "id")))
                .filter_map(|r| r.artifact.repository_id.as_deref())
                .collect::<BTreeSet<_>>();
            if owners.len() == 1 {
                assign(&mut r, owners.iter().next().unwrap(), None, context);
            } else {
                r.blockers.push("ownership_unknown".into());
            }
            r.last_activity_signature = r.artifact.dependencies.join(",");
            out.records.push(r);
        }
        let ids = self
            .docker_output(self.discovery_arguments(vec![
                "network".into(),
                "ls".into(),
                "--quiet".into(),
                "--no-trunc".into(),
            ]))?
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for network in self.inspect_rows("network", &ids, NETWORK_FORMAT)? {
            let mut r = candidate(
                api::Kind::Network,
                api::Effect::RuntimeResource,
                s(&network, "name").into(),
                Locator::Docker {
                    object_type: "network".into(),
                    identity: s(&network, "id").into(),
                    created: s(&network, "created").into(),
                },
                context,
            )?;
            if matches!(s(&network, "name"), "host" | "bridge" | "none") {
                r.blockers.push("docker_system_network".into());
            }
            r.artifact.ownership = "docker_host_network".into();
            for id in array(&network, "containers")
                .iter()
                .filter_map(Value::as_str)
            {
                if let Some(c) = known.get(id) {
                    r.artifact.dependencies.push(c.artifact.artifact_id.clone());
                    if c.blockers
                        .iter()
                        .any(|s| s == "active_consumer" || s == "current_deployment")
                    {
                        r.blockers.push("active_consumer".into());
                    }
                } else {
                    r.blockers.push("consumer_unverified".into());
                }
            }
            r.last_activity_signature = r.artifact.dependencies.join(",");
            out.records.push(r);
        }
        Ok(())
    }

    fn discover_build_cache(&self, context: &Context, out: &mut Discovery) {
        if self.fixture_namespace().is_some() {
            // The host's default builder is not owned by an isolated fixture.
            out.coverage_gaps
                .push("fixture_builder_not_configured".into());
            return;
        }
        let result=self.docker_output(vec!["buildx".into(),"du".into(),"--builder".into(),"default".into(),"--format".into(),r#"{"id":{{json .ID}},"in_use":{{json .InUse}},"shared":{{json .Shared}},"size":{{json .Size}},"last_used":{{json .LastUsedAt}}}"#.into()]);
        let Ok(text) = result else {
            out.coverage_gaps
                .push("build_cache_observation_unavailable".into());
            return;
        };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                out.coverage_gaps
                    .push("build_cache_metadata_invalid".into());
                continue;
            };
            let id = s(&v, "id");
            if id.is_empty() {
                continue;
            }
            let Ok(mut r) = candidate(
                api::Kind::BuildCache,
                api::Effect::Rebuildable,
                format!("Build cache {}", id),
                Locator::Docker {
                    object_type: "build_cache".into(),
                    identity: id.into(),
                    created: String::new(),
                },
                context,
            ) else {
                continue;
            };
            r.artifact.ownership = "local_builder_cache".into();
            r.artifact.allocated_bytes = v.get("size").and_then(Value::as_u64);
            if v.get("in_use").and_then(Value::as_bool).unwrap_or(true) {
                r.blockers.push("active_consumer".into());
            }
            if v.get("shared").and_then(Value::as_bool).unwrap_or(true) {
                r.blockers.push("shared_image_layers".into());
                r.artifact.allocated_bytes = None;
            }
            r.last_activity_signature = s(&v, "last_used").into();
            r.artifact.last_used_at_ms = date_ms(s(&v, "last_used"));
            out.records.push(r);
        }
    }

    pub(super) fn validate_docker(
        &self,
        r: &Record,
        context: &Context,
        selected: &[String],
    ) -> Result<(), ProtocolError> {
        let Locator::Docker {
            object_type,
            identity,
            created,
        } = &r.locator
        else {
            return Err(blocked("native_identity_invalid"));
        };
        if context.leased_artifacts.contains(&r.artifact.artifact_id) {
            return Err(blocked("active_lease"));
        }
        if r.owner_deployment
            .as_ref()
            .is_some_and(|d| context.deployment_repositories.contains_key(d))
        {
            return Err(blocked("current_deployment"));
        }
        if object_type == "build_cache" {
            let mut scan = Discovery::default();
            self.discover_build_cache(context, &mut scan);
            let current = scan
                .records
                .iter()
                .find(|c| c.artifact.artifact_id == r.artifact.artifact_id)
                .ok_or_else(|| blocked("build_cache_identity_unverified"))?;
            if !current.blockers.is_empty() {
                return Err(blocked(&current.blockers[0]));
            }
            return Ok(());
        }
        let format = match object_type.as_str() {
            "container" => CONTAINER_FORMAT,
            "volume" => VOLUME_FORMAT,
            "image" => IMAGE_FORMAT,
            "network" => NETWORK_FORMAT,
            _ => return Err(blocked("native_identity_invalid")),
        };
        let values = self.inspect_rows(object_type, std::slice::from_ref(identity), format)?;
        let value = values.first().ok_or_else(|| blocked("identity_missing"))?;
        if s(value, "created") != created {
            return Err(blocked("identity_changed"));
        }
        let containers = self.container_rows()?;
        if object_type == "container" {
            if !inactive(value) {
                return Err(blocked("active_consumer"));
            }
            if context.current_container_ids.contains(identity)
                || context.current_projects.contains_key(s(value, "project"))
            {
                return Err(blocked("current_deployment"));
            }
        } else {
            let consumers = containers.iter().filter(|c| match object_type.as_str() {
                "volume" => array(c, "mounts").iter().any(|m| s(m, "name") == identity),
                "image" => s(c, "image") == identity,
                "network" => array(value, "containers")
                    .iter()
                    .any(|v| v.as_str() == Some(s(c, "id"))),
                _ => false,
            });
            for c in consumers {
                if !inactive(c) {
                    return Err(blocked("active_consumer"));
                }
                let locator = Locator::Docker {
                    object_type: "container".into(),
                    identity: s(c, "id").into(),
                    created: s(c, "created").into(),
                };
                let id =
                    crate::storage::stable_id("sa", crate::storage::json(&locator)?.as_bytes());
                if !selected.contains(&id) {
                    return Err(blocked("consumer_not_in_cleanup"));
                }
            }
        }
        if object_type == "volume" {
            let mountpoint = std::path::Path::new(s(value, "mountpoint"));
            let identity = fs::identity(mountpoint)?;
            if r.resource_key != format!("fs:{}:{}", identity.0, identity.1) {
                // Retiring a bind mount exposes Docker's original empty _data
                // directory. Accept only that recorded transition, while the
                // exact backing inode still exists and the mount receipt passed.
                let dependencies = r.artifact.dependencies.clone();
                let mounts = self.database.call(move |c| {
                    let mut records = Vec::new();
                    for id in dependencies {
                        let value = c.query_row("SELECT record_json FROM storage_artifacts WHERE artifact_id=?1 AND removed_at_ms IS NOT NULL AND EXISTS(SELECT 1 FROM storage_item_receipts WHERE artifact_id=?1 AND json_extract(receipt_json,'$.status')='removed')",[id],|r|r.get::<_,String>(0)).optional()?;
                        if let Some(value) = value { records.push(value); }
                    }
                    Ok(records)
                }).map_err(crate::storage::db_error)?;
                let mut retired = false;
                for value in mounts {
                    let record: Record = crate::storage::parse(&value)?;
                    if let Locator::Mount {
                        source,
                        target,
                        device,
                        inode,
                        ..
                    } = record.locator
                        && target == mountpoint
                        && r.resource_key == format!("fs:{device}:{inode}")
                        && fs::identity(&source)? == (device, inode)
                        && !fs::mount_targets()?.contains(&target)
                        && !self
                            .mount_entries()?
                            .iter()
                            .any(|entry| entry.target == target)
                    {
                        retired = true;
                    }
                }
                if !retired || fs::measure(mountpoint)?.entries != 0 {
                    return Err(blocked("volume_identity_changed"));
                }
            }
            self.validate_mount_consumers(r, context, selected)?;
            if s(value, "driver") != "local"
                || value.get("options").and_then(Value::as_u64).unwrap_or(0) != 0
            {
                return Err(blocked("external_volume_driver"));
            }
            if processes_reference(&r.private_aliases)? {
                return Err(blocked("active_process"));
            }
        }
        Ok(())
    }

    pub(super) fn docker_absent(
        &self,
        r: &Record,
        context: &Context,
    ) -> Result<bool, ProtocolError> {
        let Locator::Docker {
            object_type,
            identity,
            ..
        } = &r.locator
        else {
            return Err(blocked("native_identity_invalid"));
        };
        if object_type == "build_cache" {
            let mut result = Discovery::default();
            self.discover_build_cache(context, &mut result);
            if !result.coverage_gaps.is_empty() {
                return Err(unavailable("docker_observation_incomplete"));
            }
            return Ok(!result
                .records
                .iter()
                .any(|v| v.artifact.artifact_id == r.artifact.artifact_id));
        }
        let args = match object_type.as_str() {
            "container" => vec!["ps", "--all", "--quiet", "--no-trunc"],
            "image" => vec!["image", "ls", "--all", "--quiet", "--no-trunc"],
            "volume" => vec!["volume", "ls", "--quiet"],
            "network" => vec!["network", "ls", "--quiet", "--no-trunc"],
            _ => return Err(blocked("native_identity_invalid")),
        };
        Ok(!self
            .docker_output(args.into_iter().map(str::to_owned).collect())?
            .lines()
            .any(|id| id == identity))
    }

    pub(super) fn remove_docker(&self, r: &Record) -> Result<(), ProtocolError> {
        let Locator::Docker {
            object_type,
            identity,
            ..
        } = &r.locator
        else {
            return Err(blocked("native_identity_invalid"));
        };
        if identity.is_empty()
            || identity.starts_with('-')
            || identity.len() > 256
            || identity.chars().any(char::is_control)
        {
            return Err(blocked("native_identity_invalid"));
        }
        let args = if object_type == "build_cache" {
            vec![
                "buildx".into(),
                "prune".into(),
                "--builder".into(),
                "default".into(),
                "--filter".into(),
                format!("id={identity}"),
                "--filter".into(),
                "inuse=false".into(),
                "--force".into(),
            ]
        } else {
            vec![object_type.clone(), "rm".into(), identity.clone()]
        };
        self.docker_output(args)?;
        Ok(())
    }
}

fn assign(r: &mut Record, repo: &str, deployment: Option<&str>, context: &Context) {
    r.artifact.repository_id = Some(repo.into());
    r.artifact.repository_name = context
        .repositories
        .iter()
        .find(|p| p.id == repo)
        .map(|p| p.name.clone());
    r.owner_deployment = deployment.map(str::to_owned);
    r.artifact.group_id = deployment.map(str::to_owned);
    r.artifact.group_name = deployment.and_then(|id| {
        context
            .deployment_names
            .get(id)
            .cloned()
            .or_else(|| context.observed.get(id).map(|(_, name)| name.clone()))
    });
    r.artifact.ownership = "managed".into();
}
fn date_ms(value: &str) -> Option<u64> {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .ok()
        .and_then(|d| u64::try_from(d.unix_timestamp_nanos() / 1_000_000).ok())
}
