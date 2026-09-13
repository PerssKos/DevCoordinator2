use super::*;
use crate::docker::{DockerOutput, LogFollower};
use std::collections::VecDeque;
use std::sync::Mutex;
use tempfile::{TempDir, tempdir};

struct FakeDocker {
    replies: Mutex<VecDeque<DockerOutput>>,
    requests: Mutex<Vec<Vec<String>>>,
}

impl DockerControl for FakeDocker {
    fn invoke(&self, invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
        assert!(invocation.timeout() <= Duration::from_secs(30));
        self.requests.lock().unwrap().push(
            invocation
                .args()
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
        );
        let response = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("bounded request");
        assert!(response.stdout.len() <= 256 * 1024);
        Ok(response)
    }

    fn spawn_follow_logs(&self, _: &ExactContainerId) -> Result<LogFollower, DockerError> {
        Err(DockerError::InvalidRequest(
            "unexpected log operation".into(),
        ))
    }
}

struct Fixture {
    _temporary: TempDir,
    docker: Arc<FakeDocker>,
    inventory: ContainerInventory,
}

impl Fixture {
    fn new(replies: Vec<DockerOutput>) -> Self {
        let temporary = tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let docker = Arc::new(FakeDocker {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        });
        Self {
            _temporary: temporary,
            inventory: ContainerInventory::new(database, "fixture", docker.clone()),
            docker,
        }
    }
}

fn reply(stdout: String) -> DockerOutput {
    DockerOutput {
        exit_code: 0,
        stdout,
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

fn clipped() -> DockerOutput {
    DockerOutput {
        stdout_truncated: true,
        ..reply(String::new())
    }
}

#[test]
fn bounded_batches_preserve_large_inventory_and_foreign_classification() {
    let identities = (0..65)
        .map(|index| format!("{index:064x}"))
        .collect::<Vec<_>>();
    let records = identities
        .iter()
        .map(|identity| {
            serde_json::json!({
                "ID":identity,"Names":"foreign","State":"running",
                "Labels":format!("unrelated={}", "x".repeat(4096))
            })
            .to_string()
        })
        .collect::<Vec<_>>();
    let complete = records.join("\n");
    assert!(complete.len() > 256 * 1024);
    let mut replies = vec![
        DockerOutput {
            stdout: complete[..256 * 1024].into(),
            ..clipped()
        },
        reply(identities.join("\n")),
    ];
    replies.extend(
        records
            .chunks(INVENTORY_BATCH_SIZE)
            .map(|batch| reply(batch.join("\n"))),
    );
    let fixture = Fixture::new(replies);
    let rows = fixture.inventory.containers().unwrap();
    assert_eq!(
        rows.containers
            .iter()
            .map(|row| (&row.id, &row.classification))
            .collect::<Vec<_>>(),
        identities
            .iter()
            .map(|identity| (identity, &ContainerClassification::Unmanaged))
            .collect::<Vec<_>>()
    );
    let requests = fixture.docker.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests[1],
        ["ps", "--all", "--no-trunc", "--format", "{{.ID}}"]
    );
    for (request, batch) in requests[2..]
        .iter()
        .zip(identities.chunks(INVENTORY_BATCH_SIZE))
    {
        let mut expected = vec![
            "ps".into(),
            "--all".into(),
            "--no-trunc".into(),
            "--format".into(),
            "{{json .}}".into(),
        ];
        for identity in batch {
            expected.extend(["--filter".into(), format!("id={identity}")]);
        }
        assert_eq!(request, &expected);
    }
}

#[test]
fn empty_or_disappearing_containers_do_not_trigger_unfiltered_retries() {
    let fixture = Fixture::new(vec![clipped(), reply(String::new())]);
    assert!(fixture.inventory.all_containers().unwrap().is_empty());
    assert_eq!(fixture.docker.requests.lock().unwrap().len(), 2);
    let identity = "a".repeat(64);
    let fixture = Fixture::new(vec![clipped(), reply(identity), reply(String::new())]);
    assert!(fixture.inventory.all_containers().unwrap().is_empty());
    assert_eq!(fixture.docker.requests.lock().unwrap().len(), 3);
}

#[test]
fn partial_failed_or_malformed_batches_never_become_successful_inventory() {
    for failed in [
        clipped(),
        DockerOutput {
            stderr_truncated: true,
            ..reply(String::new())
        },
        DockerOutput {
            exit_code: 1,
            ..reply(String::new())
        },
        reply("not JSON".into()),
        reply(serde_json::json!({"ID":"b".repeat(64)}).to_string()),
        reply(format!(
            "{}\n{}",
            serde_json::json!({"ID":"a".repeat(64)}),
            serde_json::json!({"ID":"a".repeat(64)})
        )),
    ] {
        let fixture = Fixture::new(vec![clipped(), reply("a".repeat(64)), failed]);
        assert!(fixture.inventory.all_containers().is_err());
    }
}

#[test]
fn oversized_duplicate_or_invalid_indexes_fail_before_batch_queries() {
    for index in [
        (0..=INVENTORY_LIMIT)
            .map(|index| format!("{index:064x}"))
            .collect::<Vec<_>>()
            .join("\n"),
        format!("{}\n{}", "a".repeat(64), "a".repeat(64)),
        "short".into(),
    ] {
        let fixture = Fixture::new(vec![clipped(), reply(index)]);
        assert!(fixture.inventory.all_containers().is_err());
        assert_eq!(fixture.docker.requests.lock().unwrap().len(), 2);
    }
}

#[test]
fn expired_shared_budget_does_not_start_another_query() {
    let fixture = Fixture::new(Vec::new());
    assert!(
        fixture
            .inventory
            .batched_containers(Instant::now())
            .is_err()
    );
    assert!(fixture.docker.requests.lock().unwrap().is_empty());
}
