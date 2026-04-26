//! Multi-agent orchestration and coordination.
//!
//! The `AgentOrchestrator` manages a pool of agent types, delegates tasks,
//! enforces resource locks, and broadcasts lifecycle events.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::agent_context::{AgentContext, AgentTask};
use crate::notification::{EventType, Notification, NotificationDispatcher, Severity};

// ---------------------------------------------------------------------------
// Orchestrator state
// ---------------------------------------------------------------------------

/// Execution state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// A task instance managed by the orchestrator.
#[derive(Debug, Clone)]
pub struct OrchestratedTask {
    /// The original task definition.
    pub task: AgentTask,
    /// Current state.
    pub state: TaskState,
    /// Assigned agent type.
    pub assigned_agent: String,
    /// Timestamp ms when task was queued.
    pub queued_at_ms: u64,
    /// Timestamp ms when task started (None if not yet started).
    pub started_at_ms: Option<u64>,
    /// Timestamp ms when task finished (None if not yet finished).
    pub finished_at_ms: Option<u64>,
    /// Result output from the agent.
    pub result: Option<String>,
    /// Error message if the task failed.
    pub error: Option<String>,
    /// Resources this task has locked.
    pub locked_resources: Vec<String>,
}

// ---------------------------------------------------------------------------
// Resource locking
// ---------------------------------------------------------------------------

/// A set of named resources with at-most-one concurrent claim.
#[derive(Debug, Clone, Default)]
pub struct ResourceLock {
    locks: HashMap<String, String>, // resource -> task_id
}

impl ResourceLock {
    /// Try to acquire one or more resources for a task.
    /// Returns the set of already-locked resources if any conflict.
    pub fn acquire(&mut self, task_id: &str, resources: &[String]) -> Result<(), HashSet<String>> {
        let mut conflicts = HashSet::new();
        for res in resources {
            if let Some(owner) = self.locks.get(res) {
                if owner != task_id {
                    conflicts.insert(res.clone());
                }
            }
        }

        if !conflicts.is_empty() {
            return Err(conflicts);
        }

        for res in resources {
            self.locks.insert(res.clone(), task_id.to_string());
        }
        Ok(())
    }

    /// Release all resources held by a task.
    pub fn release_by_task(&mut self, task_id: &str) {
        self.locks.retain(|_, owner| owner != task_id);
    }

    /// Check which resources a task currently holds.
    #[must_use]
    pub fn held_by(&self, task_id: &str) -> Vec<String> {
        self.locks
            .iter()
            .filter(|(_, owner)| *owner == task_id)
            .map(|(res, _)| res.clone())
            .collect()
    }

    /// Total number of locked resources.
    #[must_use]
    pub fn lock_count(&self) -> usize {
        self.locks.len()
    }
}

// ---------------------------------------------------------------------------
// Agent definition
// ---------------------------------------------------------------------------

/// A type of agent that can be assigned tasks.
#[derive(Debug, Clone, Default)]
pub struct AgentDefinition {
    /// Public identifier for this agent type.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// System prompt used when this agent runs.
    pub system_prompt: Vec<String>,
    /// Default tools enabled for this agent.
    pub default_tools: Vec<String>,
    /// Maximum number of concurrent tasks this agent can run.
    pub max_concurrent: usize,
    /// Currently running tasks for this agent type.
    pub running: Vec<String>,
}

impl AgentDefinition {
    /// Create a new agent definition.
    #[must_use]
    pub fn new(id: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            system_prompt: Vec::new(),
            default_tools: Vec::new(),
            max_concurrent: 1,
            running: Vec::new(),
        }
    }

    /// Set the max concurrent tasks.
    #[must_use]
    pub fn with_max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = n;
        self
    }

    /// Set the system prompt.
    #[must_use]
    pub fn with_system_prompt(mut self, prompts: Vec<String>) -> Self {
        self.system_prompt = prompts;
        self
    }

    /// Set default tools.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.default_tools = tools;
        self
    }

    /// Check whether this agent can accept a new task.
    #[must_use]
    pub fn can_accept(&self) -> bool {
        self.running.len() < self.max_concurrent
    }
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Coordinates multiple agents, task queues, resource locks, and events.
pub struct AgentOrchestrator {
    /// Registered agent types.
    agents: BTreeMap<String, AgentDefinition>,
    /// All known tasks by id.
    tasks: BTreeMap<String, OrchestratedTask>,
    /// Task queue per agent type.
    queues: BTreeMap<String, VecDeque<String>>,
    /// Resource lock manager.
    resources: ResourceLock,
    /// Shared context for all tasks.
    global_context: AgentContext,
    /// Optional notification dispatcher for task events.
    notifier: Option<Arc<Mutex<NotificationDispatcher>>>,
    /// Counter for auto-generated task ids.
    next_task_id: u64,
}

impl fmt::Debug for AgentOrchestrator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentOrchestrator")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .field("tasks", &self.tasks.len())
            .field("queues", &self.queues)
            .field("resources", &self.resources)
            .finish()
    }
}

impl AgentOrchestrator {
    /// Create a new orchestrator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            agents: BTreeMap::new(),
            tasks: BTreeMap::new(),
            queues: BTreeMap::new(),
            resources: ResourceLock::default(),
            global_context: AgentContext::new(),
            notifier: None,
            next_task_id: 0,
        }
    }

    /// Attach a notification dispatcher.
    pub fn with_notifier(&mut self, dispatcher: NotificationDispatcher) {
        self.notifier = Some(Arc::new(Mutex::new(dispatcher)));
    }

    /// Register an agent type.
    pub fn register_agent(&mut self, agent: AgentDefinition) {
        let id = agent.id.clone();
        self.agents.insert(id.clone(), agent);
        self.queues.entry(id).or_default();
    }

    /// Get an agent definition by id.
    #[must_use]
    pub fn get_agent(&self, id: &str) -> Option<&AgentDefinition> {
        self.agents.get(id)
    }

    /// Submit a task to a specific agent.
    pub fn submit(&mut self, agent_id: &str, mut task: AgentTask) -> Result<String, String> {
        let _agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| format!("agent '{agent_id}' not registered"))?;

        self.next_task_id += 1;
        let task_id = format!("orch-task-{}", self.next_task_id);
        task.id = task_id.clone();

        let orchestrated = OrchestratedTask {
            task,
            state: TaskState::Queued,
            assigned_agent: agent_id.to_string(),
            queued_at_ms: now_ms(),
            started_at_ms: None,
            finished_at_ms: None,
            result: None,
            error: None,
            locked_resources: Vec::new(),
        };

        self.tasks.insert(task_id.clone(), orchestrated);
        self.queues
            .entry(agent_id.to_string())
            .or_default()
            .push_back(task_id.clone());

        self.emit(
            EventType::SessionStarted,
            Severity::Info,
            format!("task {task_id} queued for agent '{agent_id}'"),
        );

        Ok(task_id)
    }

    /// Pick the next queued task for an agent and mark it running.
    /// Returns the task id if one was started, or None if queue is empty or agent is at capacity.
    pub fn start_next(&mut self, agent_id: &str) -> Option<String> {
        let agent = self.agents.get(agent_id)?;
        if !agent.can_accept() {
            return None;
        }

        let queue = self.queues.get_mut(agent_id)?;
        let next_id = queue.pop_front()?;

        let task = self.tasks.get_mut(&next_id)?;
        task.state = TaskState::Running;
        task.started_at_ms = Some(now_ms());

        let agent = self.agents.get_mut(agent_id)?;
        agent.running.push(next_id.clone());

        self.emit(
            EventType::Custom("task_started".to_string()),
            Severity::Info,
            format!("task {next_id} started on agent '{agent_id}'"),
        );

        Some(next_id)
    }

    /// Mark a task as completed with a result.
    pub fn complete(&mut self, task_id: &str, result: &str) -> Result<(), String> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task '{task_id}' not found"))?;

        if task.state != TaskState::Running {
            return Err(format!("task '{task_id}' is not running"));
        }

        task.state = TaskState::Completed;
        task.finished_at_ms = Some(now_ms());
        task.result = Some(result.to_string());
        task.locked_resources = self.resources.held_by(task_id);
        self.resources.release_by_task(task_id);

        let agent_id = &task.assigned_agent;
        if let Some(agent) = self.agents.get_mut(agent_id) {
            agent.running.retain(|id| id != task_id);
        }

        self.emit(
            EventType::Custom("task_completed".to_string()),
            Severity::Info,
            format!("task {task_id} completed"),
        );

        Ok(())
    }

    /// Mark a task as failed.
    pub fn fail(&mut self, task_id: &str, error: &str) -> Result<(), String> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task '{task_id}' not found"))?;

        if task.state != TaskState::Running {
            return Err(format!("task '{task_id}' is not running"));
        }

        task.state = TaskState::Failed;
        task.finished_at_ms = Some(now_ms());
        task.error = Some(error.to_string());
        task.locked_resources = self.resources.held_by(task_id);
        self.resources.release_by_task(task_id);

        let agent_id = &task.assigned_agent;
        if let Some(agent) = self.agents.get_mut(agent_id) {
            agent.running.retain(|id| id != task_id);
        }

        self.emit(
            EventType::Custom("task_failed".to_string()),
            Severity::Error,
            format!("task {task_id} failed: {error}"),
        );

        Ok(())
    }

    /// Cancel a queued task.
    pub fn cancel(&mut self, task_id: &str) -> Result<(), String> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task '{task_id}' not found"))?;

        if task.state != TaskState::Queued {
            return Err(format!("task '{task_id}' is not queued"));
        }

        task.state = TaskState::Cancelled;
        task.finished_at_ms = Some(now_ms());
        task.locked_resources.clear();
        self.resources.release_by_task(task_id);

        let agent_id = task.assigned_agent.clone();
        if let Some(queue) = self.queues.get_mut(&agent_id) {
            queue.retain(|id| id != task_id);
        }

        self.emit(
            EventType::Custom("task_cancelled".to_string()),
            Severity::Warning,
            format!("task {task_id} cancelled"),
        );

        Ok(())
    }

    /// Try to acquire resources for a running task.
    pub fn acquire_resources(
        &mut self,
        task_id: &str,
        resources: &[String],
    ) -> Result<(), HashSet<String>> {
        let task = self
            .tasks
            .get(task_id)
            .ok_or_else(|| HashSet::from_iter(std::iter::once(task_id.to_string())))?;
        if task.state != TaskState::Running {
            let mut msg = HashSet::new();
            msg.insert(format!(
                "task '{task_id}' is not running (state: {:?})",
                task.state
            ));
            return Err(msg);
        }
        match self.resources.acquire(task_id, resources) {
            Ok(()) => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.locked_resources
                        .extend(resources.iter().cloned());
                    task.locked_resources.sort();
                    task.locked_resources.dedup();
                }
                Ok(())
            }
            Err(conflicts) => Err(conflicts),
        }
    }

    /// List all tasks (completed, running, queued, failed, cancelled).
    #[must_use]
    pub fn all_tasks(&self) -> Vec<&OrchestratedTask> {
        self.tasks.values().collect()
    }

    /// List tasks by state.
    #[must_use]
    pub fn tasks_by_state(&self, state: TaskState) -> Vec<&OrchestratedTask> {
        self.tasks.values().filter(|t| t.state == state).collect()
    }

    /// Get a specific task.
    #[must_use]
    pub fn get_task(&self, task_id: &str) -> Option<&OrchestratedTask> {
        self.tasks.get(task_id)
    }

    /// Get global shared context.
    #[must_use]
    pub fn global_context(&self) -> &AgentContext {
        &self.global_context
    }

    /// Number of queued tasks for an agent.
    #[must_use]
    pub fn queue_len(&self, agent_id: &str) -> usize {
        self.queues.get(agent_id).map_or(0, |q| q.len())
    }

    /// Number of registered agents.
    #[must_use]
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Total number of tasks.
    #[must_use]
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    // Emit a notification if a dispatcher is attached.
    fn emit(&self, event: EventType, severity: Severity, message: String) {
        if let Some(notifier) = &self.notifier {
            if let Ok(dispatcher) = notifier.lock() {
                let _ = dispatcher.dispatch(&Notification::new(event, message, severity));
            }
        }
    }
}

impl Default for AgentOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(prompt: &str) -> AgentTask {
        AgentTask::new("", "explore", prompt)
    }

    #[test]
    fn orchestrator_new_is_empty() {
        let orch = AgentOrchestrator::new();
        assert_eq!(orch.agent_count(), 0);
        assert_eq!(orch.task_count(), 0);
    }

    #[test]
    fn register_and_find_agent() {
        let mut orch = AgentOrchestrator::new();
        let agent = AgentDefinition::new("explore", "Explorer");
        orch.register_agent(agent);
        assert_eq!(orch.agent_count(), 1);
        assert!(orch.get_agent("explore").is_some());
    }

    #[test]
    fn submit_task_queues_it() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer"));

        let id = orch
            .submit("explore", make_task("find files"))
            .expect("submit");
        let task = orch.get_task(&id).expect("task exists");
        assert_eq!(task.state, TaskState::Queued);
        assert_eq!(orch.queue_len("explore"), 1);
    }

    #[test]
    fn submit_to_unknown_agent_fails() {
        let mut orch = AgentOrchestrator::new();
        let result = orch.submit("missing", make_task("do something"));
        assert!(result.is_err());
    }

    #[test]
    fn start_next_dequeues_and_runs() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer"));
        let id = orch.submit("explore", make_task("x")).expect("submit");

        let started = orch.start_next("explore");
        assert_eq!(started, Some(id.clone()));
        assert_eq!(orch.get_task(&id).unwrap().state, TaskState::Running);
        assert_eq!(orch.queue_len("explore"), 0);
    }

    #[test]
    fn start_next_respects_max_concurrent() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer").with_max_concurrent(1));

        let id1 = orch.submit("explore", make_task("a")).expect("submit");
        let _id2 = orch.submit("explore", make_task("b")).expect("submit");

        let started = orch.start_next("explore");
        assert_eq!(started, Some(id1));

        let blocked = orch.start_next("explore");
        assert!(blocked.is_none());
        assert_eq!(orch.queue_len("explore"), 1);
    }

    #[test]
    fn complete_task_releases_agent_slot() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer").with_max_concurrent(1));

        let id1 = orch.submit("explore", make_task("a")).expect("submit");
        let id2 = orch.submit("explore", make_task("b")).expect("submit");

        orch.start_next("explore");
        orch.complete(&id1, "done").expect("complete");

        let next = orch.start_next("explore").expect("should start next");
        assert_eq!(next, id2);
    }

    #[test]
    fn fail_task_releases_resources() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer"));

        let id = orch.submit("explore", make_task("x")).expect("submit");
        orch.start_next("explore");
        orch.acquire_resources(&id, &["src/main.rs".to_string()])
            .expect("lock");

        orch.fail(&id, "crashed").expect("fail");

        let task = orch.get_task(&id).unwrap();
        assert_eq!(task.state, TaskState::Failed);
        assert!(task.error.is_some());
        assert_eq!(orch.resources.lock_count(), 0);
    }

    #[test]
    fn cancel_queued_task() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer"));

        let id = orch.submit("explore", make_task("x")).expect("submit");
        orch.cancel(&id).expect("cancel");
        assert_eq!(orch.get_task(&id).unwrap().state, TaskState::Cancelled);
        assert_eq!(orch.queue_len("explore"), 0);
    }

    #[test]
    fn cancel_running_task_fails() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer"));
        let id = orch.submit("explore", make_task("x")).expect("submit");
        orch.start_next("explore");
        let result = orch.cancel(&id);
        assert!(result.is_err());
    }

    #[test]
    fn resource_lock_conflict() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("a1", "Agent1"));
        orch.register_agent(AgentDefinition::new("a2", "Agent2"));

        let id1 = orch.submit("a1", make_task("t1")).expect("submit");
        let id2 = orch.submit("a2", make_task("t2")).expect("submit");
        orch.start_next("a1");
        orch.start_next("a2");

        orch.acquire_resources(&id1, &["file.rs".to_string()])
            .expect("lock1");
        let conflict = orch.acquire_resources(&id2, &["file.rs".to_string()]);
        assert!(conflict.is_err());

        let conflicts = conflict.unwrap_err();
        assert!(conflicts.contains("file.rs"));
    }

    #[test]
    fn resource_lock_release_by_task() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer"));

        let id = orch.submit("explore", make_task("x")).expect("submit");
        orch.start_next("explore");
        orch.acquire_resources(&id, &["a.rs".to_string(), "b.rs".to_string()])
            .expect("lock");
        assert_eq!(orch.resources.lock_count(), 2);

        orch.complete(&id, "done").expect("complete");
        assert_eq!(orch.resources.lock_count(), 0);
    }

    #[test]
    fn list_tasks_by_state() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer").with_max_concurrent(2));

        let id1 = orch.submit("explore", make_task("a")).expect("submit");
        let id2 = orch.submit("explore", make_task("b")).expect("submit");
        let id3 = orch.submit("explore", make_task("c")).expect("submit");

        orch.start_next("explore"); // id1 running
        orch.start_next("explore"); // id2 running
        orch.complete(&id2, "done").expect("complete");

        let running = orch.tasks_by_state(TaskState::Running);
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].task.id, id1);

        let completed = orch.tasks_by_state(TaskState::Completed);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].task.id, id2);

        assert_eq!(orch.tasks_by_state(TaskState::Queued).len(), 1);
        assert_eq!(orch.tasks_by_state(TaskState::Failed).len(), 0);

        orch.fail(&id1, "err").expect("fail");
        assert!(orch.get_task(&id3).unwrap().state == TaskState::Queued);
    }

    #[test]
    fn serde_round_trip_task_state() {
        for state in [
            TaskState::Queued,
            TaskState::Running,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Cancelled,
        ] {
            let json = serde_json::to_string(&state).expect("serialize");
            let parsed: TaskState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn global_context_is_shared() {
        let mut orch = AgentOrchestrator::new();
        orch.global_context().set("key", "value");
        assert_eq!(orch.global_context().get("key"), Some("value".to_string()));
    }

    #[test]
    fn acquire_resources_rejects_non_running_task() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer"));
        let id = orch.submit("explore", make_task("x")).expect("submit");
        let result = orch.acquire_resources(&id, &["f.rs".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn multiple_acquire_appends_resources() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("explore", "Explorer"));
        let id = orch.submit("explore", make_task("x")).expect("submit");
        orch.start_next("explore");
        orch.acquire_resources(&id, &["a.rs".to_string()]).expect("1");
        orch.acquire_resources(&id, &["b.rs".to_string()]).expect("2");
        let task = orch.get_task(&id).unwrap();
        assert!(task.locked_resources.contains(&"a.rs".to_string()));
        assert!(task.locked_resources.contains(&"b.rs".to_string()));
    }

    #[test]
    fn resource_lock_rolling_acquire_atomicity() {
        let mut lock = ResourceLock::default();
        lock.acquire("t1", &["a.rs".to_string(), "b.rs".to_string()])
            .expect("lock");
        // Partial-acquire: t2 wants both b and c. "b" is taken, so NO locks should apply.
        let conflict = lock.acquire("t2", &["b.rs".to_string(), "c.rs".to_string()]);
        assert!(conflict.is_err());
        assert!(conflict.unwrap_err().contains("b.rs"));
        // Verify t2 did NOT acquire c.rs either.
        assert_eq!(lock.held_by("t2").len(), 0);
    }

    #[test]
    fn resource_lock_idempotent_reacquire() {
        let mut lock = ResourceLock::default();
        lock.acquire("t1", &["a.rs".to_string()]).expect("1");
        lock.acquire("t1", &["a.rs".to_string()]).expect("2");
        assert_eq!(lock.held_by("t1"), vec!["a.rs"]);
    }

    #[test]
    fn queue_len_unregistered_agent_returns_zero() {
        let orch = AgentOrchestrator::new();
        assert_eq!(orch.queue_len("nonexistent"), 0);
    }

    #[test]
    fn get_task_missing_returns_none() {
        let orch = AgentOrchestrator::new();
        assert!(orch.get_task("no-such-task").is_none());
    }

    #[test]
    fn task_and_agent_count_after_lifecycle() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("a", "A"));
        orch.register_agent(AgentDefinition::new("b", "B"));
        assert_eq!(orch.agent_count(), 2);

        let id1 = orch.submit("a", make_task("x")).expect("1");
        let _id2 = orch.submit("a", make_task("y")).expect("2");
        assert_eq!(orch.task_count(), 2);

        orch.start_next("a");
        orch.complete(&id1, "ok").expect("complete");

        // task stays in map even after completion
        assert_eq!(orch.task_count(), 2);
    }

    #[test]
    fn cancel_on_completed_task_fails() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("a", "A"));
        let id = orch.submit("a", make_task("x")).expect("1");
        orch.start_next("a");
        orch.complete(&id, "ok").expect("complete");
        assert!(orch.cancel(&id).is_err());
        assert!(orch.cancel(&id).unwrap_err().contains("not queued"));
    }

    #[test]
    fn fail_captures_locked_resources() {
        let mut orch = AgentOrchestrator::new();
        orch.register_agent(AgentDefinition::new("a", "A"));
        let id = orch.submit("a", make_task("x")).expect("submit");
        orch.start_next("a");
        orch.acquire_resources(&id, &["f.rs".to_string()]).expect("lock");
        orch.fail(&id, "boom").expect("fail");
        let task = orch.get_task(&id).unwrap();
        assert!(task.locked_resources.contains(&"f.rs".to_string()));
        assert_eq!(orch.resources.lock_count(), 0);
    }
}
