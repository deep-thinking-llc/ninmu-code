use std::fs;
use std::path::PathBuf;

use ninmu_runtime::harness_contract::{
    HarnessEvent, HarnessTaskRequest, HarnessTaskResult, HarnessTaskStatus, TaskLease,
    TaskLeaseAck, TaskLeaseResult, WorkerHeartbeat, WorkerPolicyDecision, WorkerRegistration,
};
use serde_json::json;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
}

#[test]
fn valid_request_fixture_deserializes_and_validates() {
    let raw = fs::read_to_string(repo_root().join("examples/harness/task-request.json"))
        .expect("fixture should exist");
    let request: HarnessTaskRequest =
        serde_json::from_str(&raw).expect("request fixture should deserialize");

    request.validate().expect("request fixture should validate");
    assert_eq!(request.protocol.as_str(), "ninmu.harness/v1alpha1");
    assert_eq!(request.mission_id, "mission-018f");
    assert_eq!(request.task_id, "task-018f");
}

#[test]
fn valid_result_fixture_deserializes_and_validates() {
    let raw = fs::read_to_string(repo_root().join("examples/harness/task-result.json"))
        .expect("fixture should exist");
    let result: HarnessTaskResult =
        serde_json::from_str(&raw).expect("result fixture should deserialize");

    result.validate().expect("result fixture should validate");
    assert_eq!(result.status, HarnessTaskStatus::Completed);
    assert!(!result.retryable);
}

#[test]
fn valid_event_deserializes_and_validates() {
    let raw = json!({
        "protocol": "ninmu.harness/v1alpha1",
        "mission_id": "mission-018f",
        "task_id": "task-018f",
        "event_id": "event-001",
        "sequence": 1,
        "timestamp": "2026-04-29T10:00:00Z",
        "kind": "task.started",
        "payload": {
            "objective": "Inspect the repository"
        }
    });
    let event: HarnessEvent =
        serde_json::from_value(raw).expect("event fixture should deserialize");

    event.validate().expect("event should validate");
    assert_eq!(event.kind.as_str(), "task.started");
}

#[test]
fn phase4_event_kinds_deserialize_and_validate() {
    for kind in [
        "task.started",
        "turn.started",
        "tool.started",
        "tool.completed",
        "file.changed",
        "test.started",
        "test.completed",
        "task.blocked",
        "task.failed",
        "task.completed",
    ] {
        let raw = json!({
            "protocol": "ninmu.harness/v1alpha1",
            "mission_id": "mission-018f",
            "task_id": "task-018f",
            "event_id": format!("event-{kind}"),
            "sequence": 1,
            "timestamp": "2026-04-29T10:00:00Z",
            "kind": kind,
            "payload": {}
        });
        let event: HarnessEvent =
            serde_json::from_value(raw).expect("event fixture should deserialize");

        event.validate().expect("event kind should validate");
        assert_eq!(event.kind.as_str(), kind);
    }
}

#[test]
fn unknown_event_kind_fails_validation() {
    let raw = json!({
        "protocol": "ninmu.harness/v1alpha1",
        "mission_id": "mission-018f",
        "task_id": "task-018f",
        "event_id": "event-unknown",
        "sequence": 1,
        "timestamp": "2026-04-29T10:00:00Z",
        "kind": "task.waiting",
        "payload": {}
    });
    let event: HarnessEvent =
        serde_json::from_value(raw).expect("event fixture should deserialize");

    let error = event
        .validate()
        .expect_err("unknown event kind should fail validation");
    assert_eq!(
        error.to_string(),
        "unsupported harness event kind: task.waiting"
    );
}

#[test]
fn invalid_status_fails_to_deserialize() {
    let mut raw: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo_root().join("examples/harness/task-result.json"))
            .expect("fixture should exist"),
    )
    .expect("fixture should parse");
    raw["status"] = json!("waiting");

    let error =
        serde_json::from_value::<HarnessTaskResult>(raw).expect_err("unknown status should fail");
    assert!(error.to_string().contains("unknown variant"));
}

#[test]
fn missing_objective_fails_validation() {
    let mut raw: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo_root().join("examples/harness/task-request.json"))
            .expect("fixture should exist"),
    )
    .expect("fixture should parse");
    raw.as_object_mut()
        .expect("request should be object")
        .remove("objective");
    let request: HarnessTaskRequest =
        serde_json::from_value(raw).expect("missing objective defaults before validation");

    let error = request
        .validate()
        .expect_err("missing objective should fail validation");
    assert_eq!(error.to_string(), "objective must be non-empty");
}

#[test]
fn missing_workdir_fails_validation() {
    let mut raw: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo_root().join("examples/harness/task-request.json"))
            .expect("fixture should exist"),
    )
    .expect("fixture should parse");
    raw.as_object_mut()
        .expect("request should be object")
        .remove("workdir");
    let request: HarnessTaskRequest =
        serde_json::from_value(raw).expect("missing workdir defaults before validation");

    let error = request
        .validate()
        .expect_err("missing workdir should fail validation");
    assert_eq!(error.to_string(), "workdir must be non-empty");
}

#[test]
fn worker_mutations_require_idempotency_keys() {
    let request: HarnessTaskRequest = serde_json::from_str(
        &fs::read_to_string(repo_root().join("examples/harness/task-request.json"))
            .expect("fixture should exist"),
    )
    .expect("request fixture should deserialize");
    let result: HarnessTaskResult = serde_json::from_str(
        &fs::read_to_string(repo_root().join("examples/harness/task-result.json"))
            .expect("fixture should exist"),
    )
    .expect("result fixture should deserialize");

    let registration: WorkerRegistration = serde_json::from_value(json!({
        "protocol": "ninmu.harness/v1alpha1",
        "project_id": "project_123",
        "worker_id": "worker_123",
        "idempotency_key": "idem-register",
        "capabilities": [{"name": "execution", "values": ["run-task"]}]
    }))
    .expect("registration should deserialize");
    registration
        .validate()
        .expect("registration should validate");

    let heartbeat = WorkerHeartbeat {
        protocol: registration.protocol,
        worker_id: registration.worker_id.clone(),
        project_id: registration.project_id.clone(),
        idempotency_key: "idem-heartbeat".to_string(),
        status: "idle".to_string(),
    };
    heartbeat.validate().expect("heartbeat should validate");

    let lease = TaskLease {
        lease_id: "lease_123".to_string(),
        idempotency_key: "idem-lease".to_string(),
        task_request: request,
        cancelled: false,
    };
    lease.validate().expect("lease should validate");

    let ack = TaskLeaseAck {
        protocol: registration.protocol,
        lease_id: lease.lease_id.clone(),
        idempotency_key: "idem-ack".to_string(),
        accepted: true,
        reason: None,
    };
    ack.validate().expect("ack should validate");

    let lease_result = TaskLeaseResult {
        protocol: registration.protocol,
        lease_id: lease.lease_id.clone(),
        idempotency_key: "idem-result".to_string(),
        result,
    };
    lease_result
        .validate()
        .expect("lease result should validate");

    let policy = WorkerPolicyDecision {
        protocol: registration.protocol,
        lease_id: lease.lease_id,
        idempotency_key: String::new(),
        accepted: false,
        reason: Some("missing key test".to_string()),
    };
    let error = policy
        .validate()
        .expect_err("empty idempotency key should fail");
    assert_eq!(error.to_string(), "idempotency_key must be non-empty");
}
