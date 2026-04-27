"""Shared test fixtures for ninmu-py."""

from __future__ import annotations

import io
import json
import queue
import subprocess
import threading
from typing import Any

import pytest


_DEFAULT_METHOD_RESPONSES: dict[str, dict[str, Any]] = {
    "ping": {"status": "ok", "version": "0.1.0"},
    "session.create": {"sessionId": "session-abc"},
    "session.turn": {"sessionId": "session-abc", "status": "ok", "summary": "Hello back"},
    "session.list": {"sessions": [{"sessionId": "s1"}]},
    "session.destroy": {"status": "destroyed", "sessionId": "session-abc"},
    "session.tree.fork": {"sessionId": "session-def", "activeId": "session-def"},
    "session.tree.navigate": {"sessionId": "session-abc", "activeId": "node-42"},
    "session.tree.path": {"sessionId": "session-abc", "path": [{"id": "root", "role": "user"}]},
}


class MockStdout:
    """Acts like a pipe: readline() blocks until data is enqueued via write()."""

    def __init__(self) -> None:
        self._queue: queue.Queue[str | None] = queue.Queue()
        self._closed = False

    def readline(self, size: int = -1) -> str:
        data = self._queue.get()
        if data is None:
            return ""
        return data

    def write(self, data: str) -> None:
        self._queue.put(data)

    def close(self) -> None:
        self._closed = True

    @property
    def closed(self) -> bool:
        return self._closed


class MockProcess:
    """Duck-typed substitute for subprocess.Popen.

    A background thread polls *stdin*, and for each new JSON-RPC
    request it puts a canned response into *stdout* — so
    ``client._send_request``'s ``readline()`` blocks until the
    response is ready (just like a real pipe).
    """

    def __init__(
        self,
        method_responses: dict[str, dict[str, Any]] | None = None,
    ) -> None:
        self._method_responses = method_responses or dict(_DEFAULT_METHOD_RESPONSES)
        self.stdin = io.StringIO()
        self.stderr = io.StringIO()
        self.stdout = MockStdout()
        self.returncode: int | None = None
        self._done = threading.Event()
        self._last_read = 0

        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def _run(self) -> None:
        """Poll stdin for new JSON-RPC requests and respond to each."""
        while not self._done.is_set():
            self._done.wait(0.02)
            try:
                data = self.stdin.getvalue()
            except ValueError:
                break  # stdin was closed
            if len(data) <= self._last_read:
                continue
            new_text = data[self._last_read:]
            self._last_read = len(data)
            new_text = new_text.strip()
            if not new_text:
                continue
            for line in new_text.split("\n"):
                line = line.strip()
                if not line:
                    continue
                try:
                    req = json.loads(line)
                    method = req.get("method", "")
                    result = self._method_responses.get(method, {"ok": True})
                    resp = json.dumps({
                        "jsonrpc": "2.0",
                        "result": result,
                        "id": req.get("id", 0),
                    })
                    self.stdout.write(resp + "\n")
                except (json.JSONDecodeError, KeyError):
                    pass

    def poll(self) -> int | None:
        return self.returncode

    def wait(self, timeout: float | None = None) -> int:
        self._done.set()
        self._thread.join(timeout=timeout or 5)
        return self.returncode or 0

    def kill(self) -> None:
        self.returncode = -9
        self._done.set()


@pytest.fixture
def mock_process(monkeypatch: pytest.MonkeyPatch) -> MockProcess:
    """Replace ``subprocess.Popen`` with a method-aware ``MockProcess``."""
    proc = MockProcess()
    monkeypatch.setattr(subprocess, "Popen", lambda *a, **kw: proc)
    return proc


class MockProcessEvents(MockProcess):
    """Mock process that pre-seeds event notifications before the subscribe response."""

    def __init__(self) -> None:
        super().__init__({"events.subscribe": {"status": "subscribed"}})
        # Pre-seed events into the stdout queue
        self.stdout._queue.put(json.dumps({
            "jsonrpc": "2.0",
            "method": "events.stream",
            "params": {"event": "connected", "data": {}},
        }) + "\n")
        self.stdout._queue.put(json.dumps({
            "jsonrpc": "2.0",
            "method": "events.stream",
            "params": {"event": "turn_started", "data": {"input": "hello"}},
        }) + "\n")


@pytest.fixture
def mock_process_queue(monkeypatch: pytest.MonkeyPatch) -> MockProcessEvents:
    """Mock process that produces event stream notifications."""
    proc = MockProcessEvents()
    monkeypatch.setattr(subprocess, "Popen", lambda *a, **kw: proc)
    return proc


class MockProcessLarge(MockProcess):
    """Mock process that returns a 5000-char turn summary."""

    def __init__(self) -> None:
        large_summary = "x" * 5000
        responses = dict(_DEFAULT_METHOD_RESPONSES)
        responses["session.turn"] = {
            "sessionId": "session-abc",
            "status": "ok",
            "summary": large_summary,
        }
        super().__init__(responses)


@pytest.fixture
def mock_process_large(monkeypatch: pytest.MonkeyPatch) -> MockProcessLarge:
    """Mock process with a large 5000-char response."""
    proc = MockProcessLarge()
    monkeypatch.setattr(subprocess, "Popen", lambda *a, **kw: proc)
    return proc


class MockProcessCrash(MockProcess):
    """Mock process that exits before responding."""

    def __init__(self) -> None:
        super().__init__()
        self.returncode = 1
        self._done.set()  # Stop background polling thread
        # Put None into the queue so readline() returns "" immediately
        self.stdout._queue.put(None)


@pytest.fixture
def mock_process_crash(monkeypatch: pytest.MonkeyPatch) -> MockProcessCrash:
    """Mock process that crashes immediately."""
    proc = MockProcessCrash()
    monkeypatch.setattr(subprocess, "Popen", lambda *a, **kw: proc)
    return proc


class MockProcessSlowExit(MockProcess):
    """Mock process that needs a kill after timeout."""

    def wait(self, timeout: float | None = None) -> int:
        # Simulate timeout by not exiting
        import time
        time.sleep(0.05)
        self.returncode = 0
        return 0


@pytest.fixture
def mock_process_slow_exit(monkeypatch: pytest.MonkeyPatch) -> MockProcessSlowExit:
    """Mock process that simulates a slow exit (tests kill fallback)."""
    proc = MockProcessSlowExit()
    monkeypatch.setattr(subprocess, "Popen", lambda *a, **kw: proc)
    return proc
