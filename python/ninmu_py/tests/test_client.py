"""Tests for ninmu-py NinmuClient."""

from __future__ import annotations

import json
from typing import Any

import pytest

from ninmu.client import NinmuClient
from ninmu.errors import NinmuConnectionError, NinmuProtocolError, NinmuRuntimeError


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
        with pytest.raises(NinmuConnectionError, match="binary not found"):
            NinmuClient(binary="nonexistent-binary")


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
        # subscribe sends events.subscribe, gets back inline event notifications
        # followed by the response line. The mock will respond with the
        # default "events.subscribe" response, which returns {"status": "subscribed"}.
        # Our queue-based mock will return that as the only response line.
        events = list(client.subscribe())
        # No events in default mock (only basic result returned) -- just verify no error
        assert isinstance(events, list)
