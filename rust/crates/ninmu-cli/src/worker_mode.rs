use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ninmu_runtime::harness_contract::{
    HarnessCompletionEvidence, HarnessConfidence, HarnessEvent, HarnessEventKind,
    HarnessProtocolVersion, HarnessTaskRequest, HarnessTaskResult, HarnessTaskStatus, HarnessUsage,
    TaskLease, TaskLeaseResult, WorkerCapability, WorkerHeartbeat, WorkerPolicyDecision,
    WorkerRegistration,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::args::CliOutputFormat;
use crate::run_task::{self, RunTaskError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerAction {
    Connect { project: String, server: String },
    Status,
}

#[derive(Debug)]
pub(crate) enum WorkerError {
    Process(String),
    Contract(String),
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process(message) | Self::Contract(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for WorkerError {}

#[derive(Debug, Deserialize)]
struct RegistrationAck {
    worker_id: String,
}

pub(crate) fn run_worker(
    action: WorkerAction,
    output_format: CliOutputFormat,
) -> Result<(), WorkerError> {
    match action {
        WorkerAction::Status => print_status(output_format),
        WorkerAction::Connect { project, server } => connect(&project, &server, output_format),
    }
}

fn print_status(output_format: CliOutputFormat) -> Result<(), WorkerError> {
    let status = json!({
        "protocol": HarnessProtocolVersion::V1Alpha1,
        "status": "idle",
        "active_leases": [],
    });
    match output_format {
        CliOutputFormat::Json => {
            println!("{}", serde_json::to_string(&status).map_err(to_process)?);
            Ok(())
        }
        CliOutputFormat::Text => {
            println!("worker status: idle");
            Ok(())
        }
    }
}

fn connect(project: &str, server: &str, output_format: CliOutputFormat) -> Result<(), WorkerError> {
    let client = reqwest::blocking::Client::new();
    let server = server.trim_end_matches('/');
    let registration = WorkerRegistration {
        protocol: HarnessProtocolVersion::V1Alpha1,
        project_id: project.to_string(),
        worker_id: format!("local-{}", std::process::id()),
        idempotency_key: idempotency_key("register"),
        capabilities: vec![
            WorkerCapability {
                name: "contract".to_string(),
                values: vec!["ninmu.harness/v1alpha1".to_string()],
            },
            WorkerCapability {
                name: "execution".to_string(),
                values: vec!["run-task".to_string()],
            },
        ],
    };
    let ack: RegistrationAck = client
        .post(format!("{server}/workers/register"))
        .json(&registration)
        .send()
        .map_err(to_process)?
        .error_for_status()
        .map_err(to_process)?
        .json()
        .map_err(to_process)?;

    let heartbeat = WorkerHeartbeat {
        protocol: HarnessProtocolVersion::V1Alpha1,
        worker_id: ack.worker_id.clone(),
        project_id: project.to_string(),
        idempotency_key: idempotency_key("heartbeat"),
        status: "idle".to_string(),
    };
    client
        .post(format!("{server}/workers/{}/heartbeat", ack.worker_id))
        .json(&heartbeat)
        .send()
        .map_err(to_process)?
        .error_for_status()
        .map_err(to_process)?;

    let mut processed_leases = 0_u64;
    loop {
        let response = client
            .get(format!("{server}/projects/{project}/leases/next"))
            .send()
            .map_err(to_process)?
            .error_for_status()
            .map_err(to_process)?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            break;
        }
        let lease: TaskLease = response.json().map_err(to_process)?;
        lease.validate().map_err(to_contract)?;
        let policy = policy_decision(project, &lease);
        policy.validate().map_err(to_contract)?;
        let result = if !policy.accepted {
            blocked_result(
                &lease.task_request,
                policy.reason.as_deref().unwrap_or("policy rejected lease"),
            )
        } else if lease.cancelled {
            cancelled_result(&lease.task_request)
        } else {
            run_task::execute_task_request(&lease.task_request).map_err(run_task_error)?
        };
        post_event(&client, server, &lease, &result)?;
        let lease_result = TaskLeaseResult {
            protocol: HarnessProtocolVersion::V1Alpha1,
            lease_id: lease.lease_id.clone(),
            idempotency_key: lease.idempotency_key.clone(),
            result,
        };
        client
            .post(format!("{server}/leases/{}/complete", lease.lease_id))
            .json(&lease_result)
            .send()
            .map_err(to_process)?
            .error_for_status()
            .map_err(to_process)?;
        processed_leases += 1;
    }

    match output_format {
        CliOutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "protocol": HarnessProtocolVersion::V1Alpha1,
                    "status": "idle",
                    "processed_leases": processed_leases,
                }))
                .map_err(to_process)?
            );
        }
        CliOutputFormat::Text => println!("worker processed {processed_leases} lease(s)"),
    }
    Ok(())
}

fn post_event(
    client: &reqwest::blocking::Client,
    server: &str,
    lease: &TaskLease,
    result: &HarnessTaskResult,
) -> Result<(), WorkerError> {
    let event = HarnessEvent {
        protocol: HarnessProtocolVersion::V1Alpha1,
        mission_id: lease.task_request.mission_id.clone(),
        task_id: lease.task_request.task_id.clone(),
        event_id: idempotency_key("event"),
        sequence: 1,
        timestamp: timestamp(),
        kind: HarnessEventKind::new("task.started".to_string()),
        payload: json!({
            "lease_id": lease.lease_id,
            "status": result.status,
        }),
    };
    client
        .post(format!("{server}/leases/{}/events", lease.lease_id))
        .json(&event)
        .send()
        .map_err(to_process)?
        .error_for_status()
        .map_err(to_process)?;
    Ok(())
}

fn policy_decision(project: &str, lease: &TaskLease) -> WorkerPolicyDecision {
    if let Some(profile_project) = lease
        .task_request
        .project_profile
        .as_ref()
        .and_then(|profile| profile.get("project_id"))
        .and_then(Value::as_str)
    {
        if profile_project != project {
            return reject(lease, "project ID does not match worker project");
        }
    }
    if let Some(sandbox) = &lease.task_request.sandbox {
        if !sandbox.allowed_roots.is_empty()
            && !path_within_allowed_roots(&lease.task_request.workdir, &sandbox.allowed_roots)
        {
            return reject(lease, "workdir is outside allowed roots");
        }
    }
    if let Err(error) = crate::normalize_allowed_tools(&lease.task_request.allowed_tools) {
        return reject(lease, &format!("requested tools are not allowed: {error}"));
    }
    WorkerPolicyDecision {
        protocol: HarnessProtocolVersion::V1Alpha1,
        lease_id: lease.lease_id.clone(),
        idempotency_key: lease.idempotency_key.clone(),
        accepted: true,
        reason: None,
    }
}

fn reject(lease: &TaskLease, reason: &str) -> WorkerPolicyDecision {
    WorkerPolicyDecision {
        protocol: HarnessProtocolVersion::V1Alpha1,
        lease_id: lease.lease_id.clone(),
        idempotency_key: lease.idempotency_key.clone(),
        accepted: false,
        reason: Some(reason.to_string()),
    }
}

fn path_within_allowed_roots(workdir: &str, allowed_roots: &[String]) -> bool {
    let workdir =
        std::fs::canonicalize(workdir).unwrap_or_else(|_| std::path::PathBuf::from(workdir));
    allowed_roots.iter().any(|root| {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| std::path::PathBuf::from(root));
        workdir.starts_with(root)
    })
}

fn cancelled_result(request: &HarnessTaskRequest) -> HarnessTaskResult {
    HarnessTaskResult {
        protocol: HarnessProtocolVersion::V1Alpha1,
        mission_id: request.mission_id.clone(),
        task_id: request.task_id.clone(),
        status: HarnessTaskStatus::Cancelled,
        summary: "Task lease was cancelled before execution.".to_string(),
        output: json!({"message": "Task lease was cancelled before execution."}),
        changed_files: Vec::new(),
        artifacts: Vec::new(),
        tests: Vec::new(),
        usage: HarnessUsage::default(),
        estimated_cost: 0.0,
        retryable: false,
        block_reason: None,
        sandbox: crate::task_sandbox::summarize_sandbox(request),
        evidence: HarnessCompletionEvidence {
            items: vec!["remote lease cancellation received".to_string()],
            acceptance_tests_passed: None,
            changed_files_observed: false,
            unresolved_blockers: Vec::new(),
        },
        confidence: HarnessConfidence {
            score: 1.0,
            level: "high".to_string(),
        },
        status_reason: Some("cancelled by managed service".to_string()),
        orchestrator_recommendation: Some("block".to_string()),
        diff: None,
        commit_sha: None,
        risk: None,
        tool_uses: Vec::new(),
        tool_results: Vec::new(),
        applied_skills: request
            .skill_profile
            .as_ref()
            .map(|profile| {
                profile
                    .skills
                    .iter()
                    .map(|skill| skill.id.clone())
                    .collect()
            })
            .unwrap_or_default(),
        skill_confidence_delta: None,
        skill_evaluations: Vec::new(),
    }
}

fn blocked_result(request: &HarnessTaskRequest, reason: &str) -> HarnessTaskResult {
    HarnessTaskResult {
        protocol: HarnessProtocolVersion::V1Alpha1,
        mission_id: request.mission_id.clone(),
        task_id: request.task_id.clone(),
        status: HarnessTaskStatus::Blocked,
        summary: reason.to_string(),
        output: json!({"message": reason}),
        changed_files: Vec::new(),
        artifacts: Vec::new(),
        tests: Vec::new(),
        usage: HarnessUsage::default(),
        estimated_cost: 0.0,
        retryable: false,
        block_reason: Some(reason.to_string()),
        sandbox: crate::task_sandbox::summarize_sandbox(request),
        evidence: HarnessCompletionEvidence {
            items: vec!["local worker policy rejected lease".to_string()],
            acceptance_tests_passed: None,
            changed_files_observed: false,
            unresolved_blockers: vec![reason.to_string()],
        },
        confidence: HarnessConfidence {
            score: 1.0,
            level: "high".to_string(),
        },
        status_reason: Some(reason.to_string()),
        orchestrator_recommendation: Some("block".to_string()),
        diff: None,
        commit_sha: None,
        risk: None,
        tool_uses: Vec::new(),
        tool_results: Vec::new(),
        applied_skills: request
            .skill_profile
            .as_ref()
            .map(|profile| {
                profile
                    .skills
                    .iter()
                    .map(|skill| skill.id.clone())
                    .collect()
            })
            .unwrap_or_default(),
        skill_confidence_delta: None,
        skill_evaluations: Vec::new(),
    }
}

fn idempotency_key(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{prefix}-{}-{}-{counter}",
        std::process::id(),
        timestamp_millis()
    )
}

fn timestamp() -> String {
    format!("{}Z", timestamp_millis())
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn run_task_error(error: RunTaskError) -> WorkerError {
    WorkerError::Process(error.to_string())
}

fn to_process(error: impl std::fmt::Display) -> WorkerError {
    WorkerError::Process(error.to_string())
}

fn to_contract(error: impl std::fmt::Display) -> WorkerError {
    WorkerError::Contract(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lease_with_request(request: HarnessTaskRequest) -> TaskLease {
        TaskLease {
            lease_id: "lease-test".to_string(),
            idempotency_key: "idem-test".to_string(),
            task_request: request,
            cancelled: false,
        }
    }

    fn request(workdir: &std::path::Path, allowed_root: &std::path::Path) -> HarnessTaskRequest {
        HarnessTaskRequest {
            protocol: HarnessProtocolVersion::V1Alpha1,
            mission_id: "mission-test".to_string(),
            task_id: "task-test".to_string(),
            objective: "test policy".to_string(),
            workdir: workdir.display().to_string(),
            model: Some("mock-runtime".to_string()),
            permission_mode: Some("read-only".to_string()),
            allowed_tools: Vec::new(),
            acceptance_tests: Vec::new(),
            timeout_seconds: None,
            sandbox: Some(ninmu_runtime::harness_contract::HarnessSandboxRequest {
                allowed_roots: vec![allowed_root.display().to_string()],
                permission_mode: Some("read-only".to_string()),
                network_policy: Some("disabled".to_string()),
            }),
            skill_profile: None,
            project_profile: Some(json!({"project_id": "project_123"})),
            previous_context: None,
        }
    }

    #[test]
    fn policy_rejects_project_mismatch() {
        let root = std::env::temp_dir();
        let lease = lease_with_request(request(&root, &root));

        let decision = policy_decision("project_other", &lease);

        assert!(!decision.accepted);
        assert_eq!(
            decision.reason.as_deref(),
            Some("project ID does not match worker project")
        );
    }

    #[test]
    fn policy_rejects_workdir_outside_allowed_roots() {
        let allowed = std::env::temp_dir().join("ninmu-worker-allowed");
        let outside = std::env::temp_dir().join("ninmu-worker-outside");
        std::fs::create_dir_all(&allowed).expect("allowed root should exist");
        std::fs::create_dir_all(&outside).expect("outside workdir should exist");
        let lease = lease_with_request(request(&outside, &allowed));

        let decision = policy_decision("project_123", &lease);

        assert!(!decision.accepted);
        assert_eq!(
            decision.reason.as_deref(),
            Some("workdir is outside allowed roots")
        );
    }
}
