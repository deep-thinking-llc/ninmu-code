//! Policy engine for governing agent actions.
//!
//! The [`PolicyEngine`] evaluates actions against a registry of policies
//! and returns a decision: allow, request approval, or deny.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// The outcome of evaluating a policy against an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyDecision {
    Allow,
    RequestApproval { reason: String, risk_level: RiskLevel },
    Deny { reason: String },
    Defer,
}

/// Risk classification assigned to an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

/// A single policy rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub id: String,
    pub name: String,
    pub description: String,
    pub kind: PolicyKind,
    pub condition: PolicyCondition,
    pub action: PolicyDecision,
    pub priority: u32,
    pub enabled: bool,
}

/// The domain of a policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyKind {
    Execution,
    Branch,
    Test,
    Deployment,
    Notification,
}

impl Default for PolicyKind {
    fn default() -> Self {
        Self::Execution
    }
}

/// A condition expression evaluated against an action context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PolicyCondition {
    ToolName(String),
    FilePattern(String),
    PermissionMode(String),
    RiskScore { min: f64, max: f64 },
    BranchPattern(String),
    TestResult { min_pass_pct: f64 },
    DeploymentEnv(String),
    All(Vec<PolicyCondition>),
    Any(Vec<PolicyCondition>),
    Not(Box<PolicyCondition>),
}

/// Context about the action being evaluated.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyAction {
    pub kind: PolicyKind,
    pub tool_name: Option<String>,
    pub file_paths: Vec<String>,
    pub permission_mode: Option<String>,
    pub branch_name: Option<String>,
    pub test_pass_pct: Option<f64>,
    pub deployment_env: Option<String>,
    pub agent_id: String,
    pub task_id: String,
}

/// A registry of all policies.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PolicyRegistry {
    policies: Vec<Policy>,
}

// ---------------------------------------------------------------------------
// PolicyEngine
// ---------------------------------------------------------------------------

/// Evaluates actions against a set of policies.
#[derive(Debug)]
pub struct PolicyEngine {
    registry: PolicyRegistry,
}

impl PolicyEngine {
    #[must_use]
    pub fn new() -> Self {
        let mut engine = Self {
            registry: PolicyRegistry { policies: Vec::new() },
        };
        engine.register_defaults();
        engine
    }

    fn register_defaults(&mut self) {
        // Read-only tools auto-approve
        self.add_policy(Policy {
            id: "exec-read-only".into(),
            name: "Read-only tools auto-proceed".into(),
            description: "Tools in read-only mode never need approval".into(),
            kind: PolicyKind::Execution,
            condition: PolicyCondition::PermissionMode("read_only".into()),
            action: PolicyDecision::Allow,
            priority: 10,
            enabled: true,
        });

        // Write operations need approval
        self.add_policy(Policy {
            id: "exec-write-approval".into(),
            name: "Write operations need approval".into(),
            description: "Write operations require human approval in workspace-write mode".into(),
            kind: PolicyKind::Execution,
            condition: PolicyCondition::All(vec![
                PolicyCondition::PermissionMode("workspace_write".into()),
                PolicyCondition::Any(vec![
                    PolicyCondition::ToolName("write_file".into()),
                    PolicyCondition::ToolName("edit_file".into()),
                    PolicyCondition::ToolName("bash".into()),
                ]),
            ]),
            action: PolicyDecision::RequestApproval {
                reason: "File modification requires approval".into(),
                risk_level: RiskLevel::Medium,
            },
            priority: 20,
            enabled: true,
        });

        // Main branch is protected
        self.add_policy(Policy {
            id: "branch-protect-main".into(),
            name: "Main branch is protected".into(),
            description: "Direct pushes to main are blocked".into(),
            kind: PolicyKind::Branch,
            condition: PolicyCondition::BranchPattern("main".into()),
            action: PolicyDecision::Deny {
                reason: "Cannot push directly to main branch".into(),
            },
            priority: 100,
            enabled: true,
        });

        // Test pass threshold
        self.add_policy(Policy {
            id: "test-min-pass".into(),
            name: "Tests must pass at 90%+".into(),
            description: "Flag for review if test pass rate < 90%".into(),
            kind: PolicyKind::Test,
            condition: PolicyCondition::TestResult { min_pass_pct: 90.0 },
            action: PolicyDecision::RequestApproval {
                reason: "Test coverage below 90% threshold".into(),
                risk_level: RiskLevel::Medium,
            },
            priority: 50,
            enabled: true,
        });
    }

    /// Register a single policy.
    pub fn add_policy(&mut self, policy: Policy) {
        self.registry.policies.push(policy);
    }

    /// Remove a policy by ID.
    pub fn remove_policy(&mut self, id: &str) {
        self.registry.policies.retain(|p| p.id != id);
    }

    /// Evaluate an action against all matching policies.
    /// Returns the highest-priority non-Defer decision.
    #[must_use]
    pub fn evaluate(&self, action: &PolicyAction) -> PolicyDecision {
        let mut candidates: Vec<&Policy> = self
            .registry
            .policies
            .iter()
            .filter(|p| p.enabled && p.kind == action.kind)
            .collect();
        candidates.sort_by(|a, b| b.priority.cmp(&a.priority));

        for policy in &candidates {
            if Self::matches_condition(&policy.condition, action) {
                match &policy.action {
                    PolicyDecision::Defer => continue,
                    decision => return decision.clone(),
                }
            }
        }
        PolicyDecision::Defer
    }

    fn matches_condition(condition: &PolicyCondition, action: &PolicyAction) -> bool {
        match condition {
            PolicyCondition::ToolName(name) => {
                action.tool_name.as_deref() == Some(name.as_str())
            }
            PolicyCondition::FilePattern(_pattern) => {
                // Simple check: does any file path contain the pattern?
                // TODO: proper glob matching
                !action.file_paths.is_empty()
            }
            PolicyCondition::PermissionMode(mode) => {
                action.permission_mode.as_deref() == Some(mode.as_str())
            }
            PolicyCondition::RiskScore { min, max } => {
                // Placeholder: always returns false for now
                let _ = (min, max);
                false
            }
            PolicyCondition::BranchPattern(pattern) => {
                action.branch_name.as_deref() == Some(pattern.as_str())
            }
            PolicyCondition::TestResult { min_pass_pct } => {
                action
                    .test_pass_pct
                    .map_or(false, |pct| pct < *min_pass_pct)
            }
            PolicyCondition::DeploymentEnv(_env) => false,
            PolicyCondition::All(conditions) => {
                conditions.iter().all(|c| Self::matches_condition(c, action))
            }
            PolicyCondition::Any(conditions) => {
                conditions.iter().any(|c| Self::matches_condition(c, action))
            }
            PolicyCondition::Not(condition) => {
                !Self::matches_condition(condition, action)
            }
        }
    }

    /// Serialize all policies to JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.registry).unwrap_or_default()
    }

    /// Load policies from a JSON string.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is malformed.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let registry: PolicyRegistry =
            serde_json::from_str(json).map_err(|e| format!("invalid policy JSON: {e}"))?;
        Ok(Self { registry })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_only_action() -> PolicyAction {
        PolicyAction {
            kind: PolicyKind::Execution,
            permission_mode: Some("read_only".into()),
            ..Default::default()
        }
    }

    fn write_action() -> PolicyAction {
        PolicyAction {
            kind: PolicyKind::Execution,
            tool_name: Some("write_file".into()),
            permission_mode: Some("workspace_write".into()),
            ..Default::default()
        }
    }

    #[test]
    fn read_only_auto_allows() {
        let engine = PolicyEngine::new();
        assert_eq!(
            engine.evaluate(&read_only_action()),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn write_needs_approval() {
        let engine = PolicyEngine::new();
        let decision = engine.evaluate(&write_action());
        assert!(matches!(decision, PolicyDecision::RequestApproval { .. }));
    }

    #[test]
    fn branch_main_denied() {
        let engine = PolicyEngine::new();
        let action = PolicyAction {
            kind: PolicyKind::Branch,
            branch_name: Some("main".into()),
            ..Default::default()
        };
        assert!(matches!(engine.evaluate(&action), PolicyDecision::Deny { .. }));
    }

    #[test]
    fn branch_feature_allowed() {
        let engine = PolicyEngine::new();
        let action = PolicyAction {
            kind: PolicyKind::Branch,
            branch_name: Some("feature/foo".into()),
            ..Default::default()
        };
        assert_eq!(engine.evaluate(&action), PolicyDecision::Defer);
    }

    #[test]
    fn test_below_threshold() {
        let engine = PolicyEngine::new();
        let action = PolicyAction {
            kind: PolicyKind::Test,
            test_pass_pct: Some(85.0),
            ..Default::default()
        };
        assert!(matches!(
            engine.evaluate(&action),
            PolicyDecision::RequestApproval { .. }
        ));
    }

    #[test]
    fn test_above_threshold() {
        let engine = PolicyEngine::new();
        let action = PolicyAction {
            kind: PolicyKind::Test,
            test_pass_pct: Some(95.0),
            ..Default::default()
        };
        assert_eq!(engine.evaluate(&action), PolicyDecision::Defer);
    }

    #[test]
    fn custom_policy_overrides_default() {
        let mut engine = PolicyEngine::new();
        engine.add_policy(Policy {
            id: "override".into(),
            name: "Override".into(),
            description: "".into(),
            kind: PolicyKind::Execution,
            condition: PolicyCondition::PermissionMode("read_only".into()),
            action: PolicyDecision::Deny {
                reason: "blocked by custom".into(),
            },
            priority: 200,
            enabled: true,
        });
        assert!(matches!(
            engine.evaluate(&read_only_action()),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn disabled_policy_skipped() {
        let mut engine = PolicyEngine::new();
        engine.add_policy(Policy {
            id: "disabled".into(),
            name: "Disabled".into(),
            description: "".into(),
            kind: PolicyKind::Execution,
            condition: PolicyCondition::PermissionMode("read_only".into()),
            action: PolicyDecision::Deny {
                reason: "should not trigger".into(),
            },
            priority: 200,
            enabled: false,
        });
        assert_eq!(
            engine.evaluate(&read_only_action()),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn all_condition_match() {
        let condition = PolicyCondition::All(vec![
            PolicyCondition::PermissionMode("workspace_write".into()),
            PolicyCondition::ToolName("bash".into()),
        ]);
        let action = PolicyAction {
            kind: PolicyKind::Execution,
            tool_name: Some("bash".into()),
            permission_mode: Some("workspace_write".into()),
            ..Default::default()
        };
        assert!(PolicyEngine::matches_condition(&condition, &action));
    }

    #[test]
    fn any_condition_match() {
        let condition = PolicyCondition::Any(vec![
            PolicyCondition::ToolName("bash".into()),
            PolicyCondition::ToolName("write_file".into()),
        ]);
        let action = PolicyAction {
            tool_name: Some("bash".into()),
            ..Default::default()
        };
        assert!(PolicyEngine::matches_condition(&condition, &action));
    }

    #[test]
    fn not_condition_inverts() {
        let condition = PolicyCondition::Not(Box::new(PolicyCondition::ToolName("bash".into())));
        let action = PolicyAction {
            tool_name: Some("write_file".into()),
            ..Default::default()
        };
        assert!(PolicyEngine::matches_condition(&condition, &action));
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let engine = PolicyEngine::new();
        let json = engine.to_json();
        let loaded = PolicyEngine::from_json(&json).unwrap();
        assert_eq!(
            engine.evaluate(&write_action()),
            loaded.evaluate(&write_action())
        );
    }

    #[test]
    fn no_matching_policy_defers() {
        let engine = PolicyEngine::new();
        let action = PolicyAction {
            kind: PolicyKind::Deployment,
            ..Default::default()
        };
        assert_eq!(engine.evaluate(&action), PolicyDecision::Defer);
    }

    #[test]
    fn remove_policy() {
        let mut engine = PolicyEngine::new();
        engine.remove_policy("exec-read-only");
        let action = read_only_action();
        assert_eq!(engine.evaluate(&action), PolicyDecision::Defer);
    }
}
