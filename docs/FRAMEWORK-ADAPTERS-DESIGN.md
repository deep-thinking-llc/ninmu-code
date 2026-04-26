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

### Unit Tests (ninmu-py core)

| Test | Description |
|------|-------------|
| `test_client_create_session` | Mock subprocess, assert correct JSON-RPC sent and session ID parsed |
| `test_client_run_turn` | Send turn, mock response, verify output returned |
| `test_client_fork` | Send fork request, verify new session ID |
| `test_client_subscribe` | Mock event stream, verify generator yields events |
| `test_client_shutdown` | Assert process terminated and stdin closed |
| `test_protocol_error` | Mock error response, verify exception raised |
| `test_parse_error_retry` | Mock parse error, verify retry logic |
| `test_process_crash` | Kill subprocess, verify connection error raised |
| `test_concurrent_sessions` | Create multiple clients, verify isolation |
| `test_event_types` | Mock all 8 event types, verify correct deserialization |

### Integration Tests

| Test | Description |
|------|-------------|
| `test_real_rpc_startup` | Launch `ninmu rpc`, ping, verify version response |
| `test_real_session_create_turn` | Create session, run turn, verify non-empty result |
| `test_langchain_tool_roundtrip` | Wire NinmuTool into LangChain agent, invoke |
| `test_autogen_agent_roundtrip` | Wire NinmuAgent into AutoGen, send message |
| `test_crewai_tool_roundtrip` | Wire into CrewAI, execute task |
| `test_concurrent_framework_agents` | LangChain + AutoGen simultaneously |

## 6. Implementation Phases

### Phase 1: `ninmu-py` Core (3 days)
- [ ] `NinmuClient` class with process lifecycle
- [ ] JSON-RPC request/response parsing
- [ ] Event subscription generator
- [ ] Error handling and retry
- [ ] Unit tests with mocked subprocess

### Phase 2: Framework Adapters (2 days)
- [ ] LangChain `NinmuTool`
- [ ] AutoGen `NinmuAgent`
- [ ] CrewAI `NinmuTool`
- [ ] Integration tests

### Phase 3: Packaging & CI (1 day)
- [ ] `pyproject.toml` with dependencies
- [ ] PyPI publishing workflow
- [ ] Example notebook
- [ ] README with quick-start

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
    ├── test_client.py        # Unit tests with mocked subprocess
    ├── test_langchain.py     # LangChain integration
    ├── test_autogen.py       # AutoGen integration
    ├── test_crewai.py        # CrewAI integration
    └── conftest.py           # Shared fixtures
```
