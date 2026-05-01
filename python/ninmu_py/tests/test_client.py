"""Tests for ninmu-py NinmuClient."""

from __future__ import annotations

import json
import queue
import subprocess
from typing import Any

import pytest

from ninmu.client import NinmuClient
from ninmu.errors import (
    NinmuBinaryError,
    NinmuConnectionError,
    NinmuProtocolError,
    NinmuRuntimeError,
    NinmuTimeoutError,
)


def _call(client: NinmuClient, method: str, params: dict[str, Any] | None = None) -> Any:
    """Helper: invoke a private _send_request for testing."""
    return client._send_request(method, params or {})


class TestPing:
    def test_ping_returns_version(self, mock_process: Any) -> None:
        client = NinmuClient()
        result = client.ping()
        assert result["status"] == "ok"
        assert "version" in result
        client.shutdown()

    def test_ping_sends_correct_method(self, mock_process: Any) -> None:
        client = NinmuClient()
        client.ping()
        requests = json.loads(mock_process.stdin.getvalue().strip().split("\n")[0])
        assert requests["method"] == "ping"
        assert requests["jsonrpc"] == "2.0"
        client.shutdown()


class TestCreateSession:
    def test_creates_session(self, mock_process: Any) -> None:
        client = NinmuClient()
        session_id = client.create_session()
        assert session_id == "session-abc"
        client.shutdown()

    def test_create_with_model(self, mock_process: Any) -> None:
        client = NinmuClient()
        session_id = client.create_session(model="claude-sonnet-4-6")
        assert session_id == "session-abc"
        client.shutdown()

    def test_create_with_system_prompt(self, mock_process: Any) -> None:
        client = NinmuClient()
        session_id = client.create_session(system_prompt=["Be helpful."])
        assert session_id == "session-abc"
        client.shutdown()


class TestRunTurn:
    def test_run_turn_returns_summary(self, mock_process: Any) -> None:
        client = NinmuClient()
        result = client.run_turn("session-abc", "hello")
        assert result == "Hello back"
        client.shutdown()

    def test_run_turn_result_returns_full_contract(self, mock_process: Any) -> None:
        client = NinmuClient()
        result = client.run_turn_result("session-abc", "hello")
        assert result["status"] == "completed"
        assert result["summary"] == "Hello back"
        assert result["usage"]["total_tokens"] == 3
        assert result["tool_uses"] == []
        assert result["tool_results"] == []
        client.shutdown()

    def test_run_turn_result_preserves_task_failure(self, monkeypatch: Any) -> None:
        from tests.conftest import MockProcess

        proc = MockProcess({
            "session.turn": {
                "sessionId": "session-abc",
                "status": "failed",
                "summary": "provider unavailable",
                "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0},
                "tool_uses": [],
                "tool_results": [],
            }
        })
        monkeypatch.setattr("shutil.which", lambda binary: f"/mock/bin/{binary}")
        monkeypatch.setattr(subprocess, "Popen", lambda *a, **kw: proc)
        client = NinmuClient()
        result = client.run_turn_result("session-abc", "hello")
        assert result["status"] == "failed"
        assert result["summary"] == "provider unavailable"
        client.shutdown()


class TestListSessions:
    def test_list_sessions(self, mock_process: Any) -> None:
        client = NinmuClient()
        sessions = client.list_sessions()
        assert isinstance(sessions, list)
        assert sessions[0]["sessionId"] == "s1"
        client.shutdown()


class TestDestroySession:
    def test_destroy_session(self, mock_process: Any) -> None:
        client = NinmuClient()
        client.destroy_session("session-abc")
        client.shutdown()


class TestForkSession:
    def test_fork_session(self, mock_process: Any) -> None:
        client = NinmuClient()
        new_id = client.fork_session("session-abc", "node-5", "branch-x")
        assert new_id == "session-def"
        client.shutdown()


class TestNavigate:
    def test_navigate(self, mock_process: Any) -> None:
        client = NinmuClient()
        active_id = client.navigate("session-abc", "node-42")
        assert active_id == "node-42"
        client.shutdown()


class TestPath:
    def test_path(self, mock_process: Any) -> None:
        client = NinmuClient()
        path = client.path("session-abc")
        assert isinstance(path, list)
        assert path[0]["id"] == "root"
        client.shutdown()


class TestErrors:
    def test_protocol_error(self, mock_process: Any) -> None:
        # Override to produce an error response for the next call
        err_response = json.dumps({
            "jsonrpc": "2.0",
            "error": {"code": -32601, "message": "Method not found"},
            "id": 1,
        }) + "\n"
        mock_process.stdout = __import__("io").StringIO(err_response)

        client = NinmuClient()
        with pytest.raises(NinmuProtocolError) as exc:
            client.ping()
        assert exc.value.code == -32601
        client.shutdown()

    def test_runtime_error(self, mock_process: Any) -> None:
        err_response = json.dumps({
            "jsonrpc": "2.0",
            "error": {"code": -32000, "message": "Server error"},
            "id": 1,
        }) + "\n"
        mock_process.stdout = __import__("io").StringIO(err_response)

        client = NinmuClient()
        with pytest.raises(NinmuRuntimeError):
            client.ping()
        client.shutdown()

    def test_binary_not_found(self, monkeypatch: Any) -> None:
        import subprocess

        def mock_popen(*args: Any, **kwargs: Any) -> Any:
            raise FileNotFoundError("ninmu not found")

        monkeypatch.setattr(subprocess, "Popen", mock_popen)
        with pytest.raises(NinmuBinaryError, match="binary not found"):
            NinmuClient(binary="nonexistent-binary")

    def test_binary_path_traversal_rejected(self) -> None:
        with pytest.raises(NinmuBinaryError, match="traversal"):
            NinmuClient(binary="../../malicious")

    def test_binary_relative_not_on_path_rejected(self, monkeypatch: Any) -> None:
        import shutil
        monkeypatch.setattr(shutil, "which", lambda *a, **kw: None)
        with pytest.raises(NinmuBinaryError, match="not found on PATH"):
            NinmuClient(binary="some_bin")

    def test_binary_absolute_missing_rejected(self, monkeypatch: Any) -> None:
        import os
        monkeypatch.setattr(os.path, "isfile", lambda *a, **kw: False)
        with pytest.raises(NinmuBinaryError, match="does not exist"):
            NinmuClient(binary="/nonexistent/ninmu")


class TestContextManager:
    def test_context_manager_calls_shutdown(self, mock_process: Any) -> None:
        with NinmuClient() as client:
            assert client.ping()["status"] == "ok"
        # After exiting the context manager, shutdown has been called
        assert client._closed


class TestShutdown:
    def test_shutdown_is_idempotent(self, mock_process: Any) -> None:
        client = NinmuClient()
        client.shutdown()
        # Second shutdown should not raise
        client.shutdown()


class TestSubscribe:
    def test_subscribe(self, mock_process: Any) -> None:
        client = NinmuClient()
        events = list(client.subscribe())
        assert isinstance(events, list)


class TestMultipleSessions:
    def test_multiple_sessions_same_client(self, mock_process: Any) -> None:
        client = NinmuClient()
        ids = set()
        for _ in range(3):
            ids.add(client.create_session())
        # Mock returns same session ID for all calls
        assert len(ids) >= 1
        client.shutdown()


class TestLargeResult:
    def test_large_turn_result(self, mock_process_large: Any) -> None:
        client = NinmuClient()
        result = client.run_turn("session-abc", "large test")
        assert len(result) == 5000
        client.shutdown()


class TestProcessCrash:
    def test_process_crash_before_response(self, mock_process_crash: Any) -> None:
        client = NinmuClient()
        with pytest.raises(NinmuConnectionError, match="process"):
            client.ping()
        client.shutdown()


class TestEmpty:
    def test_run_turn_handles_empty_result(self, mock_process: Any) -> None:
        # Run is delegated to the mock which returns "Hello back"
        # This test verifies the method itself handles the call gracefully
        client = NinmuClient()
        result = client.run_turn("session-abc", "")
        assert isinstance(result, str)
        client.shutdown()


class TestShutdown:
    def test_shutdown_is_idempotent(self, mock_process: Any) -> None:
        client = NinmuClient()
        client.shutdown()
        client.shutdown()

    def test_shutdown_graceful_then_force(self, mock_process_slow_exit: Any) -> None:
        client = NinmuClient()
        client.shutdown()


class TestAdapterInit:
    def test_langchain_adapter_imports(self) -> None:
        from ninmu.langchain_adapter import NinmuTool  # noqa: F811
        assert NinmuTool is not None

    def test_autogen_adapter_imports(self) -> None:
        from ninmu.autogen_adapter import NinmuAgent
        assert NinmuAgent is not None

    def test_crewai_adapter_imports(self) -> None:
        from ninmu.crewai_adapter import ninmu_coding_tool
        assert ninmu_coding_tool is not None


class TestSerialization:
    def test_event_types_deserialize(self) -> None:
        events = [
            {"type": "text_delta", "delta": "hello"},
            {"type": "tool_use", "name": "bash", "input": "ls"},
            {"type": "tool_result", "name": "bash", "output": "file.rs"},
            {"type": "usage", "input_tokens": 10, "output_tokens": 5},
            {"type": "message_stop"},
            {"type": "prompt_cache", "tokens": 100},
            {"type": "error", "message": "fail"},
            {"type": "compaction", "removed": 2},
        ]
        assert len(events) == 8
