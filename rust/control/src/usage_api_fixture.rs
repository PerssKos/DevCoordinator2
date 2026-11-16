//! Real local websocket transport with synthetic, content-free usage facts.
use crate::config::CodexUsageSource;
use devcoordinator2_api::outcomes::{OutcomeEffort, OutcomeMeasurement, OutcomeRow};
use devcoordinator2_api::review::ReviewUsage;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tungstenite::Message;

pub(crate) const KEY: &str = "repo_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Clone, Default)]
pub(crate) struct ResponseGate(Arc<(Mutex<bool>, Condvar)>);

impl ResponseGate {
    pub(crate) fn wait(&self) {
        self.wait_for(Duration::from_secs(5));
    }

    pub(crate) fn wait_for(&self, timeout: Duration) {
        let (state, changed) = &*self.0;
        let (released, _) = changed
            .wait_timeout_while(state.lock().unwrap(), timeout, |released| !*released)
            .unwrap();
        assert!(*released, "fixture response was not explicitly released");
    }

    pub(crate) fn release_on_drop(&self) -> ResponseRelease {
        ResponseRelease(self.clone())
    }
}

pub(crate) struct ResponseRelease(ResponseGate);

impl ResponseRelease {
    pub(crate) fn release(self) {
        drop(self);
    }
}

impl Drop for ResponseRelease {
    fn drop(&mut self) {
        let (state, changed) = &*self.0.0;
        *state.lock().unwrap() = true;
        changed.notify_all();
    }
}

pub(crate) fn capability() -> Value {
    json!({"version":1,"supportedVersions":[1],"methods":["localUsage/summary","localUsage/repositoryIdentity"],"storage":"usage-v2","supportsWorkContextBindings":true,"supportsWorkstreamOutcomes":true,"snapshotRetentionMs":5000})
}

pub(crate) fn response(params: &Value, generated: u64, tokens: u64) -> Value {
    let mut value: Value = serde_json::from_str(include_str!(
        "../../../test-fixtures/usage-v2-bound-response.json"
    ))
    .unwrap();
    let key = params["repositoryKey"].as_str();
    value["generatedAt"] = json!(generated);
    value["snapshot"] = json!({"sourceWatermark":"usage-v2:7:10:10","generatedAt":generated,"generation":7,"watermark":10,"journalWatermark":10,"watermarkAtMs":generated,"updatedAtMs":generated,"freshness":"fresh","pendingEntries":0,"pendingCorrections":0,"refreshId":null});
    value["report"]["scope"] = json!({"type":if key.is_some(){"repository"}else{"all"},"id":key});
    value["report"]["timeRange"] = json!({"startMs":params["fromAt"],"endMs":params["toAt"]});
    for token in value["report"]["providerTokens"].as_array_mut().unwrap() {
        token["repositoryBucket"] = json!(key.unwrap_or(KEY));
        if matches!(
            token["category"].as_str(),
            Some("total_tokens" | "input_tokens")
        ) {
            token["measuredTokens"] = json!(tokens);
            token["exactTokens"] = json!(tokens);
        }
    }
    value["report"]["providerTokensByActivity"][0]["measuredTokens"] = json!(tokens);
    value["report"]["providerTokensByActivity"][0]["exactTokens"] = json!(tokens);
    let mut review: ReviewUsage = serde_json::from_value(value["review"].clone()).unwrap();
    review.totals.total_tokens = Some(tokens);
    let effort = OutcomeEffort {
        operations: 2,
        provider_total_tokens: OutcomeMeasurement {
            measured: tokens,
            exact: Some(tokens),
            unknown: 0,
        },
        ..Default::default()
    };
    review.outcomes.rows.clear();
    review.outcomes.totals = effort.clone();
    review.outcomes.unattributed = effort.clone();
    review.outcomes.attributed = OutcomeEffort::default();
    review.outcomes.unattributed_reasons = [("missing_work_context".into(), 2)].into();
    if let Some(bindings) = params["workContextBindings"].as_array()
        && let Some(binding) = bindings.iter().find(|b| b["operationId"] == "governed-op")
    {
        let context = &binding["workContext"];
        if context["threadId"] == "fixture-thread"
            && context["turnId"] == "fixture-turn"
            && let Some(outcome) = context["outcomeId"].as_str()
        {
            let stream = context["workstreamId"].as_str();
            if params["workstreamId"].is_null() || params["workstreamId"].as_str() == stream {
                let row_effort = OutcomeEffort {
                    operations: 1,
                    provider_total_tokens: OutcomeMeasurement {
                        measured: tokens - 15,
                        exact: Some(tokens - 15),
                        unknown: 0,
                    },
                    ..Default::default()
                };
                review.outcomes.attributed = row_effort.clone();
                review.outcomes.unattributed = OutcomeEffort {
                    operations: 1,
                    provider_total_tokens: OutcomeMeasurement {
                        measured: 15,
                        exact: Some(15),
                        unknown: 0,
                    },
                    ..Default::default()
                };
                review.outcomes.rows.push(OutcomeRow {
                    outcome_id: outcome.into(),
                    workstream_id: stream.map(str::to_owned),
                    kind: None,
                    title: None,
                    effort: row_effort,
                });
                review.outcomes.unattributed_reasons = [("missing_work_context".into(), 1)].into();
            }
        }
    }
    review.outcomes.total_rows = review.outcomes.rows.len();
    review.outcomes.next_cursor = None;
    review.outcomes.coverage = if review.outcomes.rows.is_empty() {
        "unavailable"
    } else {
        "partial"
    }
    .into();
    value["review"] = serde_json::to_value(review).unwrap();
    value
}

pub(crate) struct Peer {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    pub requests: Arc<Mutex<Vec<Value>>>,
    pub source: CodexUsageSource,
}

impl Peer {
    pub(crate) fn start(
        root: &Path,
        home: &Path,
        respond: impl Fn(&Value) -> Value + Send + 'static,
    ) -> Self {
        Self::with_capability(root, home, Some(capability()), respond)
    }
    pub(crate) fn with_capability(
        root: &Path,
        home: &Path,
        cap: Option<Value>,
        respond: impl Fn(&Value) -> Value + Send + 'static,
    ) -> Self {
        std::fs::create_dir_all(home.join("usage-v2")).unwrap();
        std::fs::write(home.join("usage-v2/manifest.json"), "fixture-v3").unwrap();
        let socket = root.join("usage-api.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let home = home.to_path_buf();
        let peer_home = home.clone();
        let worker = std::thread::spawn(move || {
            while !ending.load(Ordering::SeqCst) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut ws = tungstenite::accept(stream).unwrap();
                let init: Value =
                    serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(init["method"], "initialize");
                let mut result = json!({"codexHome":peer_home});
                if let Some(cap) = &cap {
                    result["localUsageAccounting"] = cap.clone();
                }
                ws.send(Message::Text(
                    json!({"id":init["id"],"result":result}).to_string().into(),
                ))
                .unwrap();
                if ws.read().is_err() {
                    continue;
                }
                let Ok(request) = ws.read() else { continue };
                let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
                observed.lock().unwrap().push(request.clone());
                let response = if request["method"] == "localUsage/repositoryIdentity" {
                    assert_eq!(request["params"]["contractVersion"], 1);
                    assert!(
                        PathBuf::from(request["params"]["path"].as_str().unwrap()).is_absolute()
                    );
                    json!({"contractVersion":1,"source":"usage-v2","repositoryId":KEY,"formatVersion":3})
                } else {
                    assert_eq!(request["method"], "localUsage/summary");
                    assert_eq!(request["params"]["contractVersion"], 1);
                    // A missing includeReview used to make every real review unattributed.
                    assert_eq!(request["params"]["includeReview"], true);
                    respond(&request["params"])
                };
                let _ = ws.send(Message::Text(
                    json!({"id":request["id"],"result":response})
                        .to_string()
                        .into(),
                ));
            }
        });
        Self {
            stop,
            worker: Some(worker),
            requests,
            source: CodexUsageSource {
                uid: rustix::process::getuid().as_raw(),
                codex_home: home,
                api_socket: Some(socket),
                executable: "/must-not-execute-retired-cli".into(),
            },
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
