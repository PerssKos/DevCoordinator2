use devcoordinator2_api::{delivery::Verification, mcp_tool, operation, parse_request};
use serde_json::{Value, json};

fn delivery_request(kind: &str) -> Value {
    json!({
        "protocol": 2,
        "id": "web-preview-contract",
        "operation": "release.deliver_evidence",
        "client": {"kind": "other"},
        "params": {
            "release_id": "release-preview",
            "path": "/fixture/repository",
            "run_id": "run-preview",
            "check": "verified-route",
            "artifact": "route-evidence",
            "manifest_sha256": "a".repeat(64),
            "source_sha256": "b".repeat(64),
            "target": "web-preview",
            "kind": kind,
            "verification_file": "delivery.json"
        }
    })
}

#[test]
fn web_preview_evidence_is_accepted_by_protocol_and_advertised_to_mcp_clients() {
    let request = delivery_request("web-deployment");
    let parsed = parse_request(&serde_json::to_vec(&request).unwrap())
        .expect("a verified website must have a delivery-evidence request kind");
    let mcp = mcp_tool("release_deliver_evidence").unwrap();
    (mcp.validate_params)(&parsed.params).unwrap();
    for schema in [
        (operation("release.deliver_evidence").unwrap().input_schema)(),
        (mcp.input_schema)(),
        (operation("release.evidence").unwrap().output_schema)(),
    ] {
        let kinds = schema.pointer("/$defs/Kind/enum").unwrap();
        assert!(kinds.as_array().unwrap().contains(&json!("web-deployment")));
    }
}

#[test]
fn retained_web_observation_round_trips_without_client_supplied_qualification() {
    let proof = json!({
        "version": 1,
        "kind": "web-deployment",
        "target": "web-preview",
        "source_sha256": "b".repeat(64),
        "file": "response.html",
        "observed_sha256": "c".repeat(64),
        "checked_at_ms": 1789185600123_u64,
        "access": "https://preview.example.test/journey",
        "observation": "web_route_passed",
        "deployment": {
            "deployment_id": "deployment-preview",
            "generation_number": 3,
            "http_status": 200,
            "content_type": "text/html; charset=utf-8"
        }
    });
    let parsed: Verification = serde_json::from_value(proof.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), proof);

    let mut forged = proof;
    forged["qualified"] = json!(true);
    assert!(serde_json::from_value::<Verification>(forged).is_err());
    let mut request = delivery_request("web-deployment");
    request["params"]["qualified"] = json!(true);
    assert!(parse_request(&serde_json::to_vec(&request).unwrap()).is_err());
}

#[test]
fn existing_delivery_clients_do_not_require_web_metadata() {
    for (kind, observation) in [
        ("artifact", "download_matched"),
        ("registry-package", "registry_download_matched"),
        ("local-executable", "executable_smoke_passed"),
    ] {
        let request = delivery_request(kind);
        parse_request(&serde_json::to_vec(&request).unwrap()).unwrap();
        let proof = json!({
            "version": 1, "kind": kind, "target": "linux-cli",
            "source_sha256": "b".repeat(64), "file": "cli",
            "observed_sha256": "c".repeat(64), "checked_at_ms": 1000,
            "access": "https://downloads.example.test/cli", "observation": observation
        });
        let parsed: Verification = serde_json::from_value(proof.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), proof);
    }
}
