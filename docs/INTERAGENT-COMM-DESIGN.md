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
| `escalate_generates_notification` | Escalate triggers NotificationDispatcher event |
| `merge_with_common_base` | Both agents modify from same base, non-overlapping |
| `merge_empty_content` | One side empty, other has content → pick non-empty |

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
