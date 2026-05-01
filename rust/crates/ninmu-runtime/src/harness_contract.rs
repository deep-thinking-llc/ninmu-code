use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const HARNESS_PROTOCOL_V1ALPHA1: &str = "ninmu.harness/v1alpha1";
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HarnessProtocolVersion {
    #[serde(rename = "ninmu.harness/v1alpha1")]
    V1Alpha1,
}

impl HarnessProtocolVersion {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1Alpha1 => HARNESS_PROTOCOL_V1ALPHA1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessValidationError {
    message: String,
}

impl HarnessValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HarnessValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HarnessValidationError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessTaskRequest {
    pub protocol: HarnessProtocolVersion,
    #[serde(default)]
    pub mission_id: String,
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub workdir: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub acceptance_tests: Vec<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub sandbox: Option<HarnessSandboxRequest>,
    #[serde(default)]
    pub skill_profile: Option<HarnessSkillProfile>,
    #[serde(default)]
    pub project_profile: Option<Value>,
    #[serde(default)]
    pub previous_context: Option<Value>,
}

impl HarnessTaskRequest {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("mission_id", &self.mission_id)?;
        require_non_empty("task_id", &self.task_id)?;
        require_non_empty("objective", &self.objective)?;
        require_non_empty("workdir", &self.workdir)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessSandboxRequest {
    #[serde(default)]
    pub allowed_roots: Vec<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub network_policy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessTaskResult {
    pub protocol: HarnessProtocolVersion,
    #[serde(default)]
    pub mission_id: String,
    #[serde(default)]
    pub task_id: String,
    pub status: HarnessTaskStatus,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub output: Value,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<HarnessArtifact>,
    #[serde(default)]
    pub tests: Vec<HarnessTestReport>,
    #[serde(default)]
    pub usage: HarnessUsage,
    #[serde(default)]
    pub estimated_cost: f64,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub block_reason: Option<String>,
    pub sandbox: HarnessSandboxSummary,
    pub evidence: HarnessCompletionEvidence,
    pub confidence: HarnessConfidence,
    #[serde(default)]
    pub status_reason: Option<String>,
    #[serde(default)]
    pub orchestrator_recommendation: Option<String>,
    #[serde(default)]
    pub diff: Option<String>,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub risk: Option<String>,
    #[serde(default)]
    pub tool_uses: Vec<Value>,
    #[serde(default)]
    pub tool_results: Vec<Value>,
    #[serde(default)]
    pub applied_skills: Vec<String>,
    #[serde(default)]
    pub skill_confidence_delta: Option<f64>,
    #[serde(default)]
    pub skill_evaluations: Vec<HarnessSkillEvaluation>,
}

impl HarnessTaskResult {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("mission_id", &self.mission_id)?;
        require_non_empty("task_id", &self.task_id)?;
        require_non_empty("summary", &self.summary)?;
        if self.status == HarnessTaskStatus::Blocked
            && self
                .block_reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
        {
            return Err(HarnessValidationError::new(
                "block_reason is required when status is blocked",
            ));
        }
        self.sandbox.validate()?;
        self.confidence.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessTaskStatus {
    Completed,
    Failed,
    Blocked,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessEvent {
    pub protocol: HarnessProtocolVersion,
    #[serde(default)]
    pub mission_id: String,
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub event_id: String,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub timestamp: String,
    pub kind: HarnessEventKind,
    #[serde(default)]
    pub payload: Value,
}

impl HarnessEvent {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("mission_id", &self.mission_id)?;
        require_non_empty("task_id", &self.task_id)?;
        require_non_empty("event_id", &self.event_id)?;
        require_non_empty("timestamp", &self.timestamp)?;
        self.kind.validate()?;
        let payload_len = serde_json::to_vec(&self.payload)
            .map_err(|error| HarnessValidationError::new(error.to_string()))?
            .len();
        if payload_len > MAX_EVENT_PAYLOAD_BYTES {
            return Err(HarnessValidationError::new(
                "event payload exceeds maximum inline size",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HarnessEventKind(String);

impl HarnessEventKind {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("kind", &self.0)?;
        if !matches!(
            self.0.as_str(),
            "task.started"
                | "turn.started"
                | "tool.started"
                | "tool.completed"
                | "file.changed"
                | "test.started"
                | "test.completed"
                | "task.blocked"
                | "task.failed"
                | "task.completed"
                | "task.cancelled"
        ) {
            return Err(HarnessValidationError::new(format!(
                "unsupported harness event kind: {}",
                self.0
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessTestReport {
    pub command: String,
    pub status: HarnessTestStatus,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub artifact: Option<HarnessArtifact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessTestStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessArtifact {
    pub path: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSkillProfile {
    #[serde(default)]
    pub skills: Vec<HarnessSkillProfileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSkillProfileEntry {
    pub id: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub prompt_fragments: Vec<String>,
    #[serde(default)]
    pub behavioural_examples: Vec<String>,
    #[serde(default)]
    pub steering_strength: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessSkillEvaluation {
    pub skill_id: String,
    #[serde(default)]
    pub applied: bool,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub prompt_fragment_ids: Vec<String>,
    #[serde(default)]
    pub behavioural_example_ids: Vec<String>,
    #[serde(default)]
    pub steering_strength: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSandboxSummary {
    #[serde(default)]
    pub actual_workdir: String,
    #[serde(default)]
    pub allowed_roots: Vec<String>,
    #[serde(default)]
    pub permission_mode: String,
    #[serde(default)]
    pub network_policy: String,
    #[serde(default)]
    pub denied_paths: Vec<String>,
    #[serde(default)]
    pub policy_violations: Vec<String>,
}

impl HarnessSandboxSummary {
    fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("sandbox.actual_workdir", &self.actual_workdir)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessCompletionEvidence {
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub acceptance_tests_passed: Option<bool>,
    #[serde(default)]
    pub changed_files_observed: bool,
    #[serde(default)]
    pub unresolved_blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessConfidence {
    pub score: f64,
    pub level: String,
}

impl HarnessConfidence {
    fn validate(&self) -> Result<(), HarnessValidationError> {
        if !(0.0..=1.0).contains(&self.score) {
            return Err(HarnessValidationError::new(
                "confidence.score must be between 0 and 1",
            ));
        }
        require_non_empty("confidence.level", &self.level)
    }
}

fn require_non_empty(name: &str, value: &str) -> Result<(), HarnessValidationError> {
    if value.trim().is_empty() {
        return Err(HarnessValidationError::new(format!(
            "{name} must be non-empty"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRegistration {
    pub protocol: HarnessProtocolVersion,
    pub project_id: String,
    pub worker_id: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub capabilities: Vec<WorkerCapability>,
}

impl WorkerRegistration {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("project_id", &self.project_id)?;
        require_non_empty("worker_id", &self.worker_id)?;
        require_non_empty("idempotency_key", &self.idempotency_key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCapability {
    pub name: String,
    #[serde(default)]
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHeartbeat {
    pub protocol: HarnessProtocolVersion,
    pub worker_id: String,
    pub project_id: String,
    pub idempotency_key: String,
    pub status: String,
}

impl WorkerHeartbeat {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("worker_id", &self.worker_id)?;
        require_non_empty("project_id", &self.project_id)?;
        require_non_empty("idempotency_key", &self.idempotency_key)?;
        require_non_empty("status", &self.status)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskLease {
    pub lease_id: String,
    pub idempotency_key: String,
    pub task_request: HarnessTaskRequest,
    #[serde(default)]
    pub cancelled: bool,
}

impl TaskLease {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("lease_id", &self.lease_id)?;
        require_non_empty("idempotency_key", &self.idempotency_key)?;
        self.task_request.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLeaseAck {
    pub protocol: HarnessProtocolVersion,
    pub lease_id: String,
    pub idempotency_key: String,
    pub accepted: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

impl TaskLeaseAck {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("lease_id", &self.lease_id)?;
        require_non_empty("idempotency_key", &self.idempotency_key)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskLeaseResult {
    pub protocol: HarnessProtocolVersion,
    pub lease_id: String,
    pub idempotency_key: String,
    pub result: HarnessTaskResult,
}

impl TaskLeaseResult {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("lease_id", &self.lease_id)?;
        require_non_empty("idempotency_key", &self.idempotency_key)?;
        self.result.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerPolicyDecision {
    pub protocol: HarnessProtocolVersion,
    pub lease_id: String,
    pub idempotency_key: String,
    pub accepted: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

impl WorkerPolicyDecision {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        require_non_empty("lease_id", &self.lease_id)?;
        require_non_empty("idempotency_key", &self.idempotency_key)
    }
}
