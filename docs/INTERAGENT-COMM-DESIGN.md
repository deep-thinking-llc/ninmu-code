# Inter-Agent Communication — Design & Implementation Plan

## 1. Overview

Extend the existing `AgentOrchestrator` (Phase 4.1) with typed message channels, a shared file staging area, conflict detection, and merge strategies — enabling agents to communicate in real-time and collaborate on shared files without stepping on each other.

## 2. Current State

Already implemented:
- `AgentContext` — thread-safe KV store for sharing string data between agents
- `AgentOrchestrator` — task queue, resource locks, lifecycle management
- `ResourceLock` — named mutex-like locking for named resources

Missing (ROADMAP 4.2):
- Typed message channels between agents
- Shared file staging area
- Conflict detection when two agents edit the same file
- Merge strategies (auto-merge, queue, escalate)

## 3. Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    AgentOrchestrator                      │
├─────────────────────────────────────────────────────────┤
│  MessageBus              SharedStaging                   │
│  ├── publish(channel,    ├── write(path, content)       │
│  │     message)          ├── read(path) → content        │
│  ├── subscribe(channel)  ├── lock(path) → token          │
│  │   → Receiver<Message> ├── release(token)              │
│  └── channels: Map       └── staging_dir: Path            │
│      └── topic → tx/rx                                    │
├─────────────────────────────────────────────────────────┤
│  ConflictDetector         MergeEngine                     │
│  ├── detect(task_id,     ├── auto_merge(base, a, b)      │
│  │     path) → Conflict  │   → merged_content             │
│  └── history: Map        ├── queue_for_human(path)       │
│      └── path → version  └── strategies: enum             │
└─────────────────────────────────────────────────────────┘
```

## 4. MessageBus

### Typed Channels

```rust
/// A strongly-typed message that agents can send to each other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub from_agent: String,
    pub to_agent: Option<String>,  // None = broadcast
    pub channel: String,
    pub payload: serde_json::Value,
    pub timestamp_ms: u64,
    pub correlation_id: Option<String>,
}

/// Agent-to-agent message bus, owned by the orchestrator.
#[derive(Debug)]
pub struct MessageBus {
    // topic -> broadcast channel sender
    channels: Arc<Mutex<HashMap<String, broadcast::Sender<AgentMessage>>>>,
    history: Arc<Mutex<VecDeque<AgentMessage>>>,
    max_history: usize,
}
```

### API

| Method | Description |
|--------|-------------|
| `publish(topic, message)` | Send a message to all subscribers of a topic |
| `subscribe(topic) -> Receiver` | Subscribe to a topic, returns a broadcast receiver |
| `subscribe_all() -> Receiver` | Subscribe to all messages (wildcard) |
| `history(topic) -> Vec<Message>` | Recent messages on a topic |
| `history_all() -> Vec<Message>` | Recent messages across all topics |

### Topics Convention

| Topic | Purpose |
|-------|---------|
| `agent.{id}.status` | Agent sends status updates (idle, working, blocked, error) |
| `agent.{id}.result` | Agent publishes task results |
| `agent.{id}.error` | Agent publishes error details |
| `orchestrator.command` | Orchestrator sends commands (pause, resume, cancel) |
| `orchestrator.replan` | Orchestrator notifies agents of plan changes |
| `task.{id}.progress` | Per-task progress updates |
| `system.alert` | System-wide alerts (resource pressure, errors) |

### Wire Example

```rust
// Agent A publishes a result
bus.publish("agent.coder.result", AgentMessage {
    from_agent: "coder".into(),
    to_agent: Some("reviewer".into()),
    channel: "agent.coder.result".into(),
    payload: json!({"files_changed": 3, "summary": "...", "risk": "medium"}),
    timestamp_ms: now_ms(),
    correlation_id: Some("task-42".into()),
});

// Agent B subscribes to results
let mut rx = bus.subscribe("agent.coder.result");
while let Ok(msg) = rx.recv() {
    if msg.from_agent != "reviewer" {
        // review the changes
    }
}
```

## 5. SharedStaging

### File-Based Staging Area

A shared directory (`<session_dir>/staging/`) where agents can write intermediate artifacts. Files are organized by agent or task.

```
.staging/
├── task-42/
│   ├── coder/
│   │   └── src/main.rs          # Agent A's version
│   └── reviewer/
│       └── review-report.md     # Agent B's review
└── shared/
    └── merged/
        └── src/main.rs          # Merged result
```

### API

```rust
pub struct SharedStaging {
    root: PathBuf,
    file_locks: Arc<Mutex<HashMap<String, String>>>, // path -> agent_id
}

impl SharedStaging {
    /// Write a file to the staging area, scoped by task and agent.
    pub fn write(&self, task_id: &str, agent_id: &str,
                 rel_path: &str, content: &str) -> Result<(), String>;

    /// Read a file from the staging area.
    pub fn read(&self, task_id: &str, rel_path: &str) -> Result<String, String>;

    /// List all files for a task.
    pub fn list(&self, task_id: &str) -> Result<Vec<String>, String>;

    /// Acquire an exclusive write lock on a file within a task.
    pub fn lock(&self, task_id: &str, agent_id: &str,
                rel_path: &str) -> Result<StagingLock, String>;

    /// Release a lock.
    pub fn unlock(&self, lock: StagingLock);

    /// Copy a file from staging to the actual workspace.
    pub fn promote(&self, task_id: &str, rel_path: &str,
                   workspace_root: &Path) -> Result<(), String>;
}
```

### Locking Strategy

- Per-file, per-task locks using the existing `ResourceLock`
- `lock()` returns a `StagingLock` token; agent must hold it to write
- Lock timeout (configurable, default 5 min) prevents deadlocks
- Read is always allowed (no read lock needed)

## 6. ConflictDetector

### Version-Tracking

```rust
pub struct ConflictDetector {
    // path -> (last_known_version, last_modified_by)
    versions: Arc<Mutex<HashMap<String, FileVersion>>>,
}

struct FileVersion {
    version: u64,
    modified_by: String,
    modified_at_ms: u64,
    checksum: String,  // sha256 of content
}
```

### Detection

```rust
impl ConflictDetector {
    pub fn record_write(&self, agent_id: &str, path: &str,
                        content: &str) -> FileVersion;
    
    pub fn check_conflict(&self, agent_id: &str, path: &str,
                          base_content: &str) -> Option<Conflict>;
    
    pub fn conflicts(&self) -> Vec<Conflict>;
}
```

A conflict is detected when:
1. Agent B calls `write(path)` but Agent A has written to `path` since Agent B last read it
2. The checksum of Agent B's base content doesn't match the current version's checksum
3. The file has been locked by another agent

## 7. MergeEngine

### Strategy Enum

```rust
pub enum MergeStrategy {
    /// Automatically merge using line-based three-way merge
    AutoMerge,
    /// Queue the conflicting write and retry after the other agent finishes
    QueueAndRetry,
    /// Escalate to human for manual resolution
    EscalateToHuman,
    /// The most recent write wins (last-writer-wins)
    LastWriterWins,
}
```

### Three-Way Merge (AutoMerge)

```rust
pub fn auto_merge(base: &str, ours: &str, theirs: &str) -> Result<String, MergeConflict> {
    // Simple line-based diff3 merge
    // If no overlapping hunks: apply both sets of changes
    // If overlapping hunks: return MergeConflict with details
}
```

### MergeResult

```rust
pub enum MergeResult {
    Clean(String),                              // Automatically merged
    Conflict { path: String, ours: String,
               theirs: String, base: String },  // Needs human resolution
}
```

## 8. Test Plan

### MessageBus Tests

| Test | Description |
|------|-------------|
| `publish_subscribe_roundtrip` | Publish message, verify subscriber receives it |
| `multiple_subscribers` | 3 subscribers, verify all receive the message |
| `topic_filtering` | Subscribe to topic A, publish to topic B, verify no cross-talk |
| `broadcast_message` | Publish with no `to_agent`, verify all subscribers receive |
| `correlation_id_tracking` | Publish with correlation ID, verify it's preserved |
| `history_retention` | Publish 10 messages, verify history limited to max |
| `unsubscribe_dropped` | Drop receiver, verify no memory leak |

### SharedStaging Tests

| Test | Description |
|------|-------------|
| `write_read_roundtrip` | Write file, read it back, verify content |
| `list_files_by_task` | Write 3 files across 2 tasks, verify correct listing |
| `lock_prevents_concurrent_write` | Lock file, try second lock, verify failure |
| `lock_release_allows_write` | Lock, release, lock again, verify success |
| `lock_timeout` | Lock, wait past timeout, verify lock auto-released |
| `promote_to_workspace` | Write to staging, promote to workspace, verify file exists |
| `read_nonexistent_returns_error` | Read a file that doesn't exist, verify error |

### ConflictDetector Tests

| Test | Description |
|------|-------------|
| `no_conflict_on_sequential_writes` | Agent A writes, Agent B reads, Agent B writes → no conflict |
| `conflict_on_concurrent_writes` | Agent A writes, Agent B writes without reading → conflict |
| `conflict_checksum_mismatch` | Stale base content triggers conflict |
| `multiple_conflicts_listed` | 3 files with conflicts, verify all returned |
| `no_false_positive_same_content` | Both agents write same content → no conflict |

### MergeEngine Tests

| Test | Description |
|------|-------------|
| `auto_merge_non_overlapping` | Two edits to different lines, verify clean merge |
| `auto_merge_overlapping_returns_conflict` | Two edits to same lines, verify conflict returned |
| `last_writer_wins` | LWW strategy, verify later write preserved |
| `queue_and_retry_records` | Verify queued writes stored for retry |
| `escalate_generates_notification` | Escalate triggers notification event |
| `merge_with_common_base` | Both agents modify from same base, non-overlapping |
| `merge_empty_content` | One side empty, other has content → pick non-empty |

## 8.5 Policy Engine (Phase 4.3)

The Policy Engine governs when agents may auto-proceed, what approval gates they must pass, and how the orchestrator routes notifications.

### Architecture

```
┌─────────────────────────────────────────────┐
│  PolicyEngine                                │
│  ├── evaluate(action, context) → Decision    │
│  ├── register_policy(policy)                 │
│  └── resolve_policy_conflict(policies)       │
├─────────────────────────────────────────────┤
│  PolicyRegistry                              │
│  ├── execution_policies          (auto-proceed rules)    │
│  ├── branch_policies             (git branch rules)      │
│  ├── test_policies               (test requirements)     │
│  ├── deployment_policies         (preview env rules)     │
│  └── notification_policies       (routing rules)         │
├─────────────────────────────────────────────┤
│  Evaluated by                                │
│  └── AgentOrchestrator (before/after tasks)  │
└─────────────────────────────────────────────┘
```

### Core Types

```rust
/// The outcome of evaluating a policy against an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Agent may proceed without human intervention.
    Allow,
    /// Agent must pause and wait for human approval before proceeding.
    RequestApproval { reason: String, risk_level: RiskLevel },
    /// Agent must not proceed; the action is blocked.
    Deny { reason: String },
    /// Defer to a more specific policy (used for policy chaining).
    Defer,
}

/// Risk classification assigned to an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,    // Safe to auto-proceed (e.g., read-only file access)
    Medium, // Notify human, auto-proceed if no response in N minutes
    High,   // Must wait for explicit human approval
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
    pub priority: u32,  // Higher = evaluated first
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PolicyKind {
    Execution,    // Auto-proceed rules
    Branch,       // Git branch creation/push rules
    Test,         // Test coverage requirements
    Deployment,   // Preview environment rules
    Notification, // Notification routing rules
}

/// A condition expression that evaluates an action context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PolicyCondition {
    /// Match by tool name (e.g., "bash", "write_file")
    ToolName(String),
    /// Match by file path pattern (e.g., "src/**/*.rs")
    FilePattern(String),
    /// Match by permission mode
    PermissionMode(PermissionMode),
    /// Match by estimated risk score
    RiskScore { min: f64, max: f64 },
    /// Match by git branch pattern
    BranchPattern(String),
    /// Match by test result
    TestResult { min_pass_pct: f64 },
    /// Match by deployment environment
    DeploymentEnv(String),
    /// Combine multiple conditions (AND)
    All(Vec<PolicyCondition>),
    /// Combine multiple conditions (OR)
    Any(Vec<PolicyCondition>),
    /// Negate a condition
    Not(Box<PolicyCondition>),
}
```

### PolicyEngine API

```rust
#[derive(Debug, Default)]
pub struct PolicyEngine {
    registry: PolicyRegistry,
}

impl PolicyEngine {
    pub fn new() -> Self;
    
    /// Register a single policy.
    pub fn add_policy(&mut self, policy: Policy);
    
    /// Remove a policy by ID.
    pub fn remove_policy(&mut self, id: &str);
    
    /// Evaluate an action against all matching policies.
    /// Returns the highest-priority non-Defer decision.
    pub fn evaluate(&self, action: &PolicyAction) -> PolicyDecision;
    
    /// Serialize all policies to JSON for persistence.
    pub fn to_json(&self) -> String;
    
    /// Load policies from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, String>;
}

/// Context about the action being evaluated.
#[derive(Debug, Clone)]
pub struct PolicyAction {
    pub kind: PolicyKind,
    pub tool_name: Option<String>,
    pub file_paths: Vec<String>,
    pub permission_mode: PermissionMode,
    pub branch_name: Option<String>,
    pub test_pass_pct: Option<f64>,
    pub deployment_env: Option<String>,
    pub agent_id: String,
    pub task_id: String,
}
```

### Default Policies

When a `PolicyEngine` is created with `::new()`, it comes pre-loaded with sensible defaults:

```rust
impl Default for PolicyEngine {
    fn default() -> Self {
        let mut engine = Self::new();
        
        // Execution policies
        engine.add_policy(Policy {
            id: "exec-read-only".into(),
            name: "Read-only tools auto-proceed".into(),
            kind: PolicyKind::Execution,
            condition: PolicyCondition::PermissionMode(PermissionMode::ReadOnly),
            action: PolicyDecision::Allow,
            priority: 10,
            enabled: true,
            description: "Tools in read-only mode never need approval".into(),
        });
        
        engine.add_policy(Policy {
            id: "exec-write-approval".into(),
            name: "Write operations need approval".into(),
            kind: PolicyKind::Execution,
            condition: PolicyCondition::All(vec![
                PolicyCondition::PermissionMode(PermissionMode::WorkspaceWrite),
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
            description: "Write operations require human approval in workspace-write mode".into(),
        });
        
        // Branch policy
        engine.add_policy(Policy {
            id: "branch-protect-main".into(),
            name: "Main branch is protected".into(),
            kind: PolicyKind::Branch,
            condition: PolicyCondition::BranchPattern("main".into()),
            action: PolicyDecision::Deny {
                reason: "Cannot push directly to main branch".into(),
            },
            priority: 100,
            enabled: true,
            description: "Direct pushes to main are blocked".into(),
        });
        
        // Test policy
        engine.add_policy(Policy {
            id: "test-min-pass".into(),
            name: "Tests must pass at 90%+".into(),
            kind: PolicyKind::Test,
            condition: PolicyCondition::TestResult { min_pass_pct: 90.0 },
            action: PolicyDecision::RequestApproval {
                reason: "Test coverage below 90% threshold".into(),
                risk_level: RiskLevel::Medium,
            },
            priority: 50,
            enabled: true,
            description: "Flag for review if test pass rate < 90%".into(),
        });
        
        engine
    }
}
```

### Integration with AgentOrchestrator

The `AgentOrchestrator` calls `policy_engine.evaluate()` at three checkpoints:

1. **Before tool execution**: checks `Execution` policies → may block or request approval
2. **Before branch push**: checks `Branch` policies → may block or require review
3. **After test suite**: checks `Test` policies → may flag for human review
4. **Before deployment**: checks `Deployment` policies → may restrict environment

```rust
// In AgentOrchestrator::execute_tool():
let action = PolicyAction {
    kind: PolicyKind::Execution,
    tool_name: Some(tool_name.clone()),
    permission_mode: self.permission_mode,
    ..Default::default()
};
match self.policy_engine.evaluate(&action) {
    PolicyDecision::Allow => {
        // Execute normally
    }
    PolicyDecision::RequestApproval { reason, risk_level } => {
        // Show permission prompt, wait for response
        self.request_human_approval(reason, risk_level).await;
    }
    PolicyDecision::Deny { reason } => {
        // Return error to model
        return Err(ToolError::new(reason));
    }
    PolicyDecision::Defer => {
        // Fall through to permission system
    }
}
```

### Policy Engine Test Plan

| # | Test | Description |
|---|------|-------------|
| PE1 | `read_only_auto_allows` | ReadOnly mode → Allow |
| PE2 | `write_needs_approval` | WorkspaceWrite + write_file → RequestApproval |
| PE3 | `bash_in_read_only_denied` | ReadOnly mode + bash → Deny |
| PE4 | `branch_main_denied` | Branch "main" → Deny |
| PE5 | `branch_feature_allowed` | Branch "feature/foo" → Allow |
| PE6 | `test_below_threshold` | 85% pass → RequestApproval |
| PE7 | `test_above_threshold` | 95% pass → Allow |
| PE8 | `custom_policy_overrides_default` | Add policy with higher priority → takes precedence |
| PE9 | `disabled_policy_skipped` | enabled=false → not evaluated |
| PE10| `all_condition_match` | All conditions met → matches |
| PE11| `any_condition_match` | One of N conditions met → matches |
| PE12| `not_condition_inverts` | Negated condition → inverse result |
| PE13| `serialize_deserialize_roundtrip` | to_json → from_json → identical policies |
| PE14| `no_matching_policy_defers` | No condition matches → Defer |
| PE15| `priority_ordering` | Higher priority policy chosen over lower |
| PE16| `remove_policy` | Add then remove → no longer evaluated |
| PE17| `high_risk_blocks_always` | High risk level → always RequestApproval regardless of other policies |

**Policy Engine total: 17 tests**

### Combined Test Totals (Phase 4)

| Component | Unit Tests | Integration Tests | E2E Tests |
|-----------|-----------|-------------------|-----------|
| MessageBus | 7 | 0 | 0 |
| SharedStaging | 7 | 0 | 0 |
| ConflictDetector | 5 | 0 | 0 |
| MergeEngine | 7 | 0 | 0 |
| PolicyEngine | 17 | 0 | 0 |
| **Full orchestration** | 0 | 6 (I1-I6) | 4 (E1-E4) |
| **Total** | **43** | **6** | **4** |

### Integration Tests (Full Orchestration)

| # | Test | Description |
|---|------|-------------|
| I1 | `two_agent_coding_review_roundtrip` | Coder agent + Reviewer agent: coder writes file, reviewer reads and approves |
| I2 | `conflict_detected_and_escalated` | Two agents edit same file → conflict detected → escalated to human |
| I3 | `auto_merge_resolves_non_overlapping` | Two agents edit different lines → auto-merge succeeds |
| I4 | `policy_blocks_dangerous_action` | Policy denies write to protected path → blocked |
| I5 | `message_bus_agent_coordination` | Agent A publishes, Agent B subscribes, orchestrator routes |
| I6 | `shared_staging_promotion` | Agent writes to staging → promotes to workspace → consumed by tests |

### E2E Tests

| # | Test | Description |
|---|------|-------------|
| E1 | `multi_agent_plan_execute_review` | Full pipeline: orchestrator decomposes plan → two agents execute → reviewer approves |
| E2 | `policy_gate_human_approval` | Policy requires human approval → agent pauses → human approves → agent continues |
| E3 | `conflict_resolution_human` | Conflict detected → human resolves via prompt → merge continues |
| E4 | `concurrent_agents_same_repo` | 3 agents working in same repo simultaneously → no data loss |

### Testing Gaps Checklist

- [ ] **MessageBus edge cases**: topic name contains special chars, message payload > 1MB, zero subscribers, rapid subscribe/unsubscribe cycling
- [ ] **SharedStaging edge cases**: concurrent `promote` races, symlink escape attacks (path traversal in `rel_path`), staging dir disk full, simultaneous lock/unlock from same agent
- [ ] **ConflictDetector edge cases**: binary files (checksum works but merge fails), 0-byte files, files with same checksum but different metadata
- [ ] **MergeEngine edge cases**: CRLF vs LF line endings, trailing newlines, UTF-8 BOM, extremely long lines (100K chars), 10,000-line files
- [ ] **PolicyEngine edge cases**: circular condition references, 1000+ policies performance, serialize/deserialize with unknown future fields (forward compat), empty policy set, `All([])` and `Any([])` behaviors
- [ ] **Concurrency**: 10 agents publishing to same topic, 5 agents writing same file via staging, 3 simultaneous merges
- [ ] **Resource cleanup**: staging dir cleaned on session end, locks released on agent crash, message bus channels dropped on unsubscribe
- [ ] **Persistence**: policy engine state serialized/restored across session boundaries, merge history persisted for audit
- [ ] **Security**: staging directory permissions (world-readable?), lock token forgery, policy engine privilege escalation
- [ ] **Performance**: 10K messages in history, 1000-file staging directory, 500 policies in registry

## 9. Implementation Phases

### Phase 1: MessageBus (2 days)
- [ ] `MessageBus` struct with `publish`, `subscribe`, `subscribe_all`
- [ ] `AgentMessage` type with full metadata
- [ ] History retention (ring buffer)
- [ ] Integration with `AgentOrchestrator`
- [ ] Unit tests (6+)

### Phase 2: SharedStaging (2 days)
- [ ] `SharedStaging` struct with `write`, `read`, `list`, `lock`, `unlock`, `promote`
- [ ] File locks using `ResourceLock`
- [ ] Lock timeout mechanism
- [ ] Unit tests (7+)

### Phase 3: ConflictDetector + MergeEngine (2 days)
- [ ] `ConflictDetector` with version tracking and checksum verification
- [ ] `MergeEngine` with 4 strategies
- [ ] Auto-merge (line-based diff3)
- [ ] Integration with NotificationDispatcher for escalations
- [ ] Unit tests (7+)

## 10. Files Changed

| File | Action |
|------|--------|
| `rust/crates/sdk/src/message_bus.rs` | NEW — MessageBus + AgentMessage |
| `rust/crates/sdk/src/shared_staging.rs` | NEW — SharedStaging + StagingLock |
| `rust/crates/sdk/src/conflict.rs` | NEW — ConflictDetector + MergeEngine |
| `rust/crates/sdk/src/orchestrator.rs` | MODIFY — wire MessageBus and SharedStaging |
| `rust/crates/sdk/src/lib.rs` | MODIFY — export new modules |
