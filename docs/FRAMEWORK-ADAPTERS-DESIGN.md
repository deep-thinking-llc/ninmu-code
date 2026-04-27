# Framework Adapters — Design & Implementation Plan

## 1. Overview

Python adapter packages that consume Ninmu Code's existing JSON-RPC server (`ninmu rpc`) for use in popular AI agent frameworks. Each adapter wraps the RPC protocol into the framework's native tool/agent conventions.

## 2. Architecture

```
┌──────────────────────────────────────────┐
│  Python Agent Framework                  │
│  │                                      │
│  ├── LangChain Agent                     │
│  ├── AutoGen Agent                       │
│  └── CrewAI Agent                        │
├──────────────────────────────────────────┤
│  ninmu-py (Python adapter package)       │
│  ├── NinmuClient()                       │
│  │   ├── create_session()                │
│  │   ├── run_turn()                      │
│  │   ├── fork_session()                  │
│  │   └── close_session()                 │
│  ├── LangChainNinmuTool()                │
│  ├── AutoGenNinmuAgent()                 │
│  └── CrewAINinmuTool()                   │
├──────────────────────────────────────────┤
│  stdin/stdout JSON-RPC                   │
├──────────────────────────────────────────┤
│  ninmu rpc (Rust process)                │
└──────────────────────────────────────────┘
```

## 3. `ninmu-py` Core Library

### NinmuClient

```python
class NinmuClient:
    """JSON-RPC client over stdin/stdout of a `ninmu rpc` subprocess.

    Wires the ninmu binary's RPC protocol: session.create, session.turn,
    session.destroy, session.tree.fork, session.tree.navigate,
    session.tree.path, session.list, events.subscribe, ping, shutdown.
    """

    def __init__(self, model: str = "claude-sonnet-4-6",
                 system_prompt: list[str] | None = None,
                 binary: str = "ninmu"):
        self.proc = subprocess.Popen(
            [binary, "rpc"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self._req_id = 0
        self._stderr_thread = threading.Thread(
            target=_drain_stderr, args=(self.proc.stderr,), daemon=True
        )
        self._stderr_thread.start()
        ...

    def create_session(self, model: str | None = None,
                       system_prompt: list[str] | None = None) -> str: ...
    def run_turn(self, session_id: str, input: str) -> str: ...
    def list_sessions(self) -> list[dict]: ...
    def destroy_session(self, session_id: str) -> None: ...
    def fork_session(self, session_id: str, node_id: str,
                     new_branch_id: str) -> str: ...
    def navigate(self, session_id: str, node_id: str) -> str: ...
    def path(self, session_id: str) -> list[dict]: ...
    def subscribe(self, session_id: str | None = None
                  ) -> Generator[dict, None, None]: ...
    def ping(self) -> dict: ...
    def shutdown(self) -> None: ...
```

### Wire Protocol (JSON-RPC 2.0)

```python
# Request
{"jsonrpc": "2.0", "method": "session.create",
 "params": {"model": "claude-sonnet-4-6"},
 "id": 1}

# Response
{"jsonrpc": "2.0", "result": {"sessionId": "abc123"}, "id": 1}

# Event notification (from subscribe)
{"jsonrpc": "2.0", "method": "events.stream",
 "params": {"event": "TurnCompleted(...)", "data": {...}}}
```

### Error Handling

- Parse errors (`-32700`): retry with backoff
- Method not found (`-32601`): raise `NinmuProtocolError`
- Generic errors (`-32000`): raise `NinmuRuntimeError` with message
- Process crash: raise `NinmuConnectionError`

## 4. Framework Adapters

### LangChain Adapter

```python
from langchain.tools import BaseTool

class NinmuTool(BaseTool):
    """LangChain tool that spawns a ninmu RPC session per invocation."""

    name: str = "ninmu_coding_agent"
    description: str = "Delegate a coding subtask to Ninmu Code agent"
    client: NinmuClient = Field(default_factory=NinmuClient)
    session_id: str = ""

    def _run(self, prompt: str) -> str:
        if not self.session_id:
            self.session_id = self.client.create_session()
        return self.client.run_turn(self.session_id, prompt)
```

**Integration example:**
```python
from langchain.agents import AgentExecutor, create_react_agent
from langchain_openai import ChatOpenAI

tools = [NinmuTool()]
agent = create_react_agent( llm=ChatOpenAI(), tools=tools, prompt=... )
```

### AutoGen Adapter

```python
from autogen import ConversableAgent

class NinmuAgent(ConversableAgent):
    """AutoGen agent backed by a ninmu RPC session."""

    def __init__(self, name: str, model: str = "claude-sonnet-4-6",
                 system_prompt: list[str] | None = None, **kwargs):
        super().__init__(name, **kwargs)
        self._client = NinmuClient(model=model, system_prompt=system_prompt)
        self._session_id = self._client.create_session()

    def generate_reply(self, messages: list[dict] | None = None,
                       sender=None, **kwargs) -> tuple[str, dict]:
        prompt = messages[-1]["content"] if messages else ""
        result = self._client.run_turn(self._session_id, prompt)
        return True, result
```

### CrewAI Adapter

```python
from crewai import Tool as CrewAITool

def ninmu_coding_tool(model: str = "claude-sonnet-4-6") -> CrewAITool:
    """CrewAI tool wrapping a ninmu RPC session."""

    def _execute(prompt: str) -> str:
        client = NinmuClient(model=model)
        session_id = client.create_session()
        try:
            return client.run_turn(session_id, prompt)
        finally:
            client.close_session(session_id)

    return CrewAITool(
        name="ninmu_coding_agent",
        description="Delegate a coding task to a Ninmu Code agent",
        func=_execute,
    )
```

## 5. Test Plan

### Testing Strategy (TDD First)

All adapter code is written test-first: define the test surface, implement to pass, then refactor. Three layers:

1. **Unit tests** — pure Python, mock subprocess/stdin/stdout, no real ninmu binary needed
2. **Integration tests** — require a real `ninmu` binary in PATH, test RPC protocol end-to-end
3. **E2E tests** — full framework integration with mock LLM to avoid API costs

### Unit Tests (ninmu-py core)

Run with `pytest python/ninmu_py/tests/ -v` (no external dependencies beyond pytest).

| Test | Layer | Coverage |
|------|-------|----------|
| `test_client_create_session` | Unit | Mock subprocess, assert correct JSON-RPC sent, session ID parsed |
| `test_client_create_session_sends_correct_json` | Unit | Verify exact JSON payload matches spec |
| `test_client_run_turn` | Unit | Send turn text, mock response, verify output |
| `test_client_run_turn_handles_empty_result` | Unit | Empty string from model → empty output |
| `test_client_fork_session` | Unit | Send fork with `node_id`, verify parent-child link |
| `test_client_subscribe_yields_events` | Unit | Mock 3 events on stream, verify generator yields all 3 |
| `test_client_subscribe_stops_on_session_end` | Unit | Session closed mid-stream → generator stops cleanly |
| `test_client_subscribe_timeout` | Unit | No events for 30s → raises `NinmuTimeoutError` |
| `test_client_shutdown_closes_process` | Unit | Assert `proc.stdin` closed, `proc.poll()` not None |
| `test_client_shutdown_graceful_then_force` | Unit | Send shutdown, wait 5s, then terminate |
| `test_client_context_manager` | Unit | `with NinmuClient() as c:` — verify cleanup on exit |
| `test_protocol_parse_error` | Unit | `{"error": {"code": -32700}}` → `NinmuProtocolError` |
| `test_protocol_method_not_found` | Unit | `{"error": {"code": -32601}}` → `NinmuProtocolError` |
| `test_protocol_generic_error` | Unit | `{"error": {"code": -32000, "message": "..."}}` → `NinmuRuntimeError` |
| `test_protocol_parse_error_retries` | Unit | First response parse error, second succeeds → works |
| `test_protocol_retry_exhausted` | Unit | 3 parse errors in a row → raises after max retries |
| `test_process_crash_before_response` | Unit | Subprocess dies mid-request → `NinmuConnectionError` |
| `test_process_crash_during_stream` | Unit | Subprocess dies mid-subscribe → generator raises |
| `test_concurrent_sessions_isolation` | Unit | 2 clients, sessions don't interfere |
| `test_event_types_all_8_deserialize` | Unit | Mock all 8 event types, verify correct fields |
| `test_drain_stderr_logs_warning` | Unit | stderr lines → logged via `logging.warning` |
| `test_ping_returns_version` | Unit | `{"result": {"version": "0.1.0"}}` → parsed correctly |
| `test_multiple_sessions_same_client` | Unit | 5 sessions from one client, all return unique IDs |
| `test_large_turn_result` | Unit | 1MB string response → handled without buffering issues |
| `test_binary_not_found` | Unit | Invalid binary path → `NinmuBinaryError` on construction |
| `test_timeout_on_slow_turn` | Unit | Turn takes >30s → `NinmuTimeoutError` |

**Total: 28 unit tests**

### Integration Tests (ninmu-py -> real binary)

Run with `pytest python/ninmu_py/tests/integration/ -v` — requires `ninmu` in PATH and
`ANTHROPIC_API_KEY` (or other provider key) set.

| Test | Layer | Setup | Teardown |
|------|-------|-------|----------|
| `test_real_rpc_startup_ping` | Integration | Launch `ninmu rpc` | Kill process |
| `test_real_session_create_turn_basic` | Integration | Create session, send "hello" | Clean session |
| `test_real_session_create_turn_nonempty` | Integration | Assert response has content | Clean session |
| `test_real_session_multiple_turns` | Integration | 3 turns in same session, verify history | Clean session |
| `test_real_session_list` | Integration | Create 2 sessions, list them | Clean both |
| `test_real_session_destroy` | Integration | Create + destroy, verify gone | — |
| `test_real_subscribe_events_stream` | Integration | Subscribe, run turn, verify event received | Clean session |
| `test_real_fork_branch` | Integration | Create, fork, verify independent session | Clean both |
| `test_real_navigate_tree` | Integration | Fork parent → branch A → navigate back | Clean all |
| `test_real_prompt_cache_events` | Integration | 2 identical turns, verify cache hit event | Clean session |
| `test_real_compaction_event` | Integration | Many turns to trigger auto-compaction | Clean session |
| `test_real_session_persistence` | Integration | Create turn, restart RPC, resume session | Clean |
| `test_real_rpc_shutdown` | Integration | Send shutdown, verify process exits 0 | — |

**Total: 13 integration tests**

### E2E Tests (Full Framework Integration)

Run with `pytest python/ninmu_py/tests/e2e/ -v` — requires frameworks installed:
`pip install langchain-core langchain-openai pyautogen crewai`

| Test | Layer | Notes |
|------|-------|-------|
| `test_langchain_tool_roundtrip` | E2E | Wire `NinmuTool` into LangChain agent, invoke, verify result |
| `test_langchain_multiple_tool_calls` | E2E | 3 sequential tool calls, verify independent sessions |
| `test_langchain_tool_error_handling` | E2E | Tool fails → LangChain agent handles gracefully |
| `test_autogen_agent_roundtrip` | E2E | Wire `NinmuAgent`, send message, get reply |
| `test_autogen_two_agent_conversation` | E2E | UserAgent + NinmuAgent have 3-turn conversation |
| `test_autogen_agent_terminates` | E2E | Agent stops when condition met |
| `test_crewai_tool_roundtrip` | E2E | Wire into CrewAI, execute task with single agent |
| `test_crewai_multi_agent` | E2E | 2 CrewAI agents with Ninmu tool, coordinated task |
| `test_all_three_frameworks_sequentially` | E2E | LangChain → AutoGen → CrewAI in one test run |
| `test_concurrent_framework_agents` | E2E | LangChain + AutoGen run simultaneously |
| `test_invalid_api_key_reports_clearly` | E2E | Bad key → clear error, not crash |

**Total: 11 e2e tests**

### Testing Gaps Checklist

- [ ] All 8 event types have at least one test (unit deserialization + integration capture)
- [ ] Error paths: protocol errors, process crash, timeout, invalid binary, bad API key
- [ ] Concurrency: multiple sessions, multiple clients, concurrent framework agents
- [ ] Edge cases: empty input, 1MB output, 100 concurrent sessions, rapid create/destroy
- [ ] Framework-specific: tool name collision, missing optional deps, version compatibility
- [ ] **Cross-platform**: `ninmu binary` not found on Windows, stderr encoding on non-UTF8 systems, process spawn paths with spaces
- [ ] **Event stream**: interleaved JSON-RPC responses with event stream (valid JSON-LD but can interleave), partial JSON chunks on stdin buffer
- [ ] **Session lifecycle**: `destroy_session` on already-destroyed session, `run_turn` on destroyed session, fork then destroy parent
- [ ] **Error escalation**: framework adapter errors propagate correctly (tool raises → framework catches → agent handles gracefully)
- [ ] **Backpressure**: 1000 events/second on subscribe → buffer management, slow consumer handling

## 6. Implementation Phases

### Phase 1: `ninmu-py` Core (3 days)
- [ ] `NinmuClient` class with process lifecycle (TDD: 7 unit tests first)
- [ ] JSON-RPC request/response parsing (TDD: 5 unit tests first)
- [ ] Event subscription generator (TDD: 3 unit tests first)
- [ ] Error handling and retry (TDD: 5 unit tests first)
- [ ] Integration tests with real binary (13 tests)
- [ ] `pyproject.toml` and package structure

### Phase 2: Framework Adapters (2 days)
- [ ] LangChain `NinmuTool` (TDD: 3 unit + 3 e2e tests)
- [ ] AutoGen `NinmuAgent` (TDD: 3 unit + 3 e2e tests)
- [ ] CrewAI `NinmuTool` (TDD: 3 unit + 3 e2e tests)
- [ ] Concurrency tests (TDD: 1 unit + 1 e2e)
- [ ] Integration test suite (13 tests)

### Phase 3: Packaging & CI (1 day)
- [ ] `pyproject.toml` with optional framework extras
- [ ] PyPI publishing workflow
- [ ] Example Jupyter notebook
- [ ] README with quick-start
- [ ] CI: run unit tests on every PR, integration on merge to main

## 7. Project Structure

```
python/ninmu_py/
├── pyproject.toml
├── README.md
├── ninmu/
│   ├── __init__.py           # Public API exports
│   ├── client.py             # NinmuClient (core RPC client)
│   ├── errors.py             # Exception classes
│   ├── langchain_adapter.py  # LangChain NinmuTool
│   ├── autogen_adapter.py    # AutoGen NinmuAgent
│   └── crewai_adapter.py     # CrewAI ninmu_coding_tool
└── tests/
    ├── conftest.py           # Shared fixtures (mock subprocess, etc.)
    ├── test_client.py        # 28 unit tests for core client
    ├── integration/
    │   ├── conftest.py       # Fixtures for real binary
    │   └── test_rpc.py       # 13 integration tests
    └── e2e/
        ├── conftest.py       # Fixtures for framework setup
        ├── test_langchain.py # 3 e2e tests
        ├── test_autogen.py   # 3 e2e tests
        ├── test_crewai.py    # 2 e2e tests
        └── test_all.py       # 3 cross-framework e2e tests
```

## 8. CI Integration

```yaml
# .github/workflows/ninmu-py.yml
name: ninmu-py

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.11"
      - name: Install ninmu binary
        run: cargo build --release -p ninmu-cli && cp target/release/ninmu /usr/local/bin/
        working-directory: rust
      - name: Install python deps
        run: pip install pytest pytest-asyncio
      - name: Run unit tests
        run: pytest python/ninmu_py/tests/ -v --ignore=python/ninmu_py/tests/integration --ignore=python/ninmu_py/tests/e2e
      - name: Run integration tests
        run: pytest python/ninmu_py/tests/integration/ -v
        env:
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
      - name: Install framework extras
        run: pip install langchain-core langchain-openai pyautogen crewai
      - name: Run e2e tests
        run: pytest python/ninmu_py/tests/e2e/ -v
        env:
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
  publish:
    if: startsWith(github.ref, 'refs/tags/')
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
      - name: Build and publish
        run: |
          pip install build twine
          python -m build python/ninmu_py/
          twine upload python/ninmu_py/dist/*
        env:
          TWINE_USERNAME: __token__
          TWINE_PASSWORD: ${{ secrets.PYPI_TOKEN }}
