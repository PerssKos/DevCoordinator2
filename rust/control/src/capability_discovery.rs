//! Provider-owned model and effort discovery for harness routing.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use devcoordinator2_api::ClientKind;
use devcoordinator2_api::agent_routing::{Capabilities, Pair};
use serde_json::Value;

use crate::config::{self, AgentCapabilitySource, Config};
use crate::usage::{run_probe_with_limit, user_record};

const DEFAULT_EXPIRY_MS: u64 = 24 * 60 * 60 * 1000;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const CAPABILITY_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
type ModelCatalog = (Vec<String>, Vec<String>, Vec<Pair>);

pub(crate) fn discover(
    config: &Config,
    harness: ClientKind,
    now_ms: u64,
) -> Result<Capabilities, String> {
    let source = sources(config, harness)?
        .into_iter()
        .next()
        .ok_or_else(|| "source_unavailable".to_owned())?;
    let output = run_source(&source, harness)?;
    if !output.status.success() || output.stdout_truncated {
        return Err("source_unavailable".to_owned());
    }
    let (models, efforts, pairs) = match harness {
        ClientKind::Codex => parse_codex(&output.stdout)?,
        ClientKind::Antigravity => parse_antigravity(&output.stdout)?,
        _ => return Err("provider_discovery_unsupported".to_owned()),
    };
    if pairs.is_empty() {
        return Err("catalog_empty".to_owned());
    }
    Ok(Capabilities {
        harness,
        models,
        efforts,
        pairs,
        reported_at_ms: Some(now_ms),
        expires_at_ms: Some(now_ms + DEFAULT_EXPIRY_MS),
        fresh: true,
    })
}

fn sources(config: &Config, harness: ClientKind) -> Result<Vec<AgentCapabilitySource>, String> {
    let configured = config::read_agent_capability_sources()
        .map_err(|_| "capability_policy_unavailable".to_owned())?;
    let mut matched: Vec<_> = configured
        .into_iter()
        .filter(|source| source.harness == harness)
        .collect();
    if matched.is_empty() && harness == ClientKind::Codex {
        matched = config
            .codex_usage_sources
            .iter()
            .map(|source| AgentCapabilitySource {
                harness,
                uid: source.uid,
                executable: source.executable.clone(),
                codex_home: Some(source.codex_home.clone()),
                home: None,
            })
            .collect();
    }
    Ok(matched)
}

fn run_source(
    source: &AgentCapabilitySource,
    harness: ClientKind,
) -> Result<crate::usage::ProbeOutput, String> {
    if !source.executable.is_absolute() || !source.executable.is_file() {
        return Err("source_unavailable".to_owned());
    }
    let user = user_record(source.uid).map_err(|_| "source_unavailable".to_owned())?;
    let home = source
        .home
        .as_deref()
        .unwrap_or_else(|| Path::new(&user.home));
    let effective = rustix::process::geteuid().as_raw();
    let mut command = if effective == 0 && source.uid != 0 {
        let setpriv = ["/usr/bin/setpriv", "/bin/setpriv"]
            .into_iter()
            .map(Path::new)
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| "source_unavailable".to_owned())?;
        let mut command = Command::new(setpriv);
        command
            .arg(format!("--reuid={}", source.uid))
            .arg(format!("--regid={}", user.gid))
            .arg("--init-groups")
            .arg("--")
            .arg(&source.executable);
        command
    } else if effective == source.uid {
        Command::new(&source.executable)
    } else {
        return Err("source_unavailable".to_owned());
    };
    match harness {
        ClientKind::Codex => {
            command.args(["debug", "models"]);
        }
        ClientKind::Antigravity => {
            command.arg("models");
        }
        _ => return Err("provider_discovery_unsupported".to_owned()),
    }
    command
        .current_dir(home)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("USER", &user.name)
        .env("LOGNAME", &user.name)
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let (ClientKind::Codex, Some(codex_home)) = (harness, source.codex_home.as_ref()) {
        command.env("CODEX_HOME", codex_home);
    }
    run_probe_with_limit(
        command,
        Instant::now() + DISCOVERY_TIMEOUT,
        CAPABILITY_OUTPUT_BYTES,
    )
}

fn parse_codex(bytes: &[u8]) -> Result<ModelCatalog, String> {
    let document: Value = serde_json::from_slice(bytes).map_err(|_| "catalog_invalid")?;
    let models = document
        .get("models")
        .and_then(Value::as_array)
        .ok_or("catalog_invalid")?;
    let mut model_ids = Vec::new();
    let mut efforts = Vec::new();
    let mut pairs = Vec::new();
    for model in models {
        if model.get("visibility").and_then(Value::as_str) != Some("list") {
            continue;
        }
        let Some(slug) = model.get("slug").and_then(Value::as_str) else {
            continue;
        };
        let Some(levels) = model
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for effort in levels
            .iter()
            .filter_map(|level| level.get("effort").and_then(Value::as_str))
        {
            model_ids.push(slug.to_owned());
            efforts.push(effort.to_owned());
            pairs.push(Pair {
                model: slug.to_owned(),
                effort: effort.to_owned(),
            });
        }
    }
    normalize(model_ids, efforts, pairs)
}

fn parse_antigravity(bytes: &[u8]) -> Result<ModelCatalog, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "catalog_invalid")?;
    let mut models = Vec::new();
    let mut efforts = Vec::new();
    let mut pairs = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(slug) = fields.next() else {
            continue;
        };
        let label = fields.collect::<Vec<_>>().join(" ");
        if label.is_empty() || !valid_slug(slug) {
            continue;
        }
        let effort = ["low", "medium", "high"]
            .into_iter()
            .find(|candidate| slug.ends_with(&format!("-{candidate}")))
            .map(str::to_owned)
            .or_else(|| {
                label
                    .to_ascii_lowercase()
                    .contains("thinking")
                    .then(|| "thinking".to_owned())
            });
        let Some(effort) = effort else {
            continue;
        };
        models.push(slug.to_owned());
        efforts.push(effort.clone());
        pairs.push(Pair {
            model: slug.to_owned(),
            effort,
        });
    }
    normalize(models, efforts, pairs)
}

fn normalize(
    models: Vec<String>,
    efforts: Vec<String>,
    pairs: Vec<Pair>,
) -> Result<ModelCatalog, String> {
    let models: Vec<_> = models
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let efforts: Vec<_> = efforts
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let pairs: Vec<_> = pairs
        .into_iter()
        .map(|pair| (pair.model, pair.effort))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|(model, effort)| Pair { model, effort })
        .collect();
    if models.is_empty() || efforts.is_empty() || pairs.is_empty() {
        return Err("catalog_empty".to_owned());
    }
    Ok((models, efforts, pairs))
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_parser_keeps_only_visible_models_and_exact_effort_pairs() {
        let catalog = br#"{
            "models": [
                {"slug":"hidden","visibility":"hide","supported_reasoning_levels":[{"effort":"max"}]},
                {"slug":"fixture-model","visibility":"list","supported_reasoning_levels":[{"effort":"high"},{"effort":"high"}]},
                {"slug":"fixture-model","visibility":"list","supported_reasoning_levels":[{"effort":"max"}]},
                {"slug":"invalid","visibility":"list"}
            ]
        }"#;
        let (models, efforts, pairs) = parse_codex(catalog).unwrap();
        assert_eq!(models, vec!["fixture-model"]);
        assert_eq!(efforts, vec!["high", "max"]);
        assert_eq!(
            pairs
                .iter()
                .map(|pair| (pair.model.as_str(), pair.effort.as_str()))
                .collect::<Vec<_>>(),
            vec![("fixture-model", "high"), ("fixture-model", "max")]
        );
        assert_eq!(
            parse_codex(br#"{"models":[]}"#).unwrap_err(),
            "catalog_empty"
        );
        assert_eq!(parse_codex(b"not json").unwrap_err(), "catalog_invalid");
    }

    #[test]
    fn antigravity_parser_requires_valid_slugs_and_explicit_effort_labels() {
        let output = b"gemini-pro-low stable\ngemini-pro-high stable\ngemini-thinking Thinking mode\ninvalid/slash high\nmodel-without-effort stable\n";
        let (models, efforts, pairs) = parse_antigravity(output).unwrap();
        assert_eq!(
            models,
            vec![
                "gemini-pro-high".to_owned(),
                "gemini-pro-low".to_owned(),
                "gemini-thinking".to_owned()
            ]
        );
        assert_eq!(efforts, vec!["high", "low", "thinking"]);
        assert_eq!(pairs.len(), 3);
        assert_eq!(
            parse_antigravity(b"model-without-effort stable\n").unwrap_err(),
            "catalog_empty"
        );
    }

    #[test]
    fn discovery_slug_validation_rejects_paths_and_unbounded_values() {
        assert!(valid_slug("provider.model-v2"));
        assert!(!valid_slug("provider/model"));
        assert!(!valid_slug(&"x".repeat(161)));
    }
}
