"""JSON-RPC 2.0 client over stdin/stdout of a ninmu rpc subprocess."""

from __future__ import annotations

import json
import subprocess
import threading
from types import TracebackType
from typing import Any, Generator, TextIO

from .errors import (
    NinmuBinaryError,
    NinmuConnectionError,
    NinmuProtocolError,
    NinmuRuntimeError,
    NinmuTimeoutError,
)

_RETRY_CODES = {-32700}  # Parse error — retryable
_MAX_RETRIES = 3
_READ_TIMEOUT = 30.0


def _drain_stderr(stream: TextIO | None) -> None:
    """Drain stderr to prevent pipe buffer blocking.

    Runs as a daemon thread; lines are discarded since ninmu RPC
    communicates exclusively via JSON-RPC on stdout.
    """
    if stream is None:
        return
    try:
        for _ in stream:
            pass
    except (ValueError, OSError):
        pass


class NinmuClient:
    """JSON-RPC 2.0 client for a ``ninmu rpc`` subprocess.

    Usage::

        with NinmuClient() as client:
            session_id = client.create_session()
            result = client.run_turn(session_id, "hello")
            client.destroy_session(session_id)
    """

    def __init__(
        self,
        model: str = "claude-sonnet-4-6",
        system_prompt: list[str] | None = None,
        binary: str = "ninmu",
    ) -> None:
        self._model = model
        self._system_prompt = system_prompt
        self._req_id = 0
        self._lock = threading.Lock()
        self._closed = False

        try:
            self._proc = subprocess.Popen(
                [binary, "rpc"],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
        except FileNotFoundError as exc:
            raise NinmuBinaryError(
                f"binary not found: {binary!r}. Is ninmu installed and on PATH?"
            ) from exc
        except OSError as exc:
            raise NinmuBinaryError(
                f"binary could not be executed: {binary!r}: {exc}"
            ) from exc

        self._stdin: TextIO | None = self._proc.stdin  # type: ignore[assignment]
        self._stdout: TextIO | None = self._proc.stdout  # type: ignore[assignment]

        self._stderr_thread = threading.Thread(
            target=_drain_stderr, args=(self._proc.stderr,), daemon=True
        )
        self._stderr_thread.start()

    # ------------------------------------------------------------------
    # Context manager
    # ------------------------------------------------------------------

    def __enter__(self) -> NinmuClient:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: TracebackType | None,
    ) -> None:
        self.shutdown()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _next_id(self) -> int:
        self._req_id += 1
        return self._req_id

    def _send_request(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        """Send a JSON-RPC request and return the decoded response."""
        return self._send_request_with_retry(method, params, attempt=0)

    def _send_request_with_retry(
        self, method: str, params: dict[str, Any] | None, attempt: int
    ) -> dict[str, Any]:
        req_id = self._next_id()
        request = {
            "jsonrpc": "2.0",
            "method": method,
            "params": params or {},
            "id": req_id,
        }

        with self._lock:
            if self._closed or self._stdin is None:
                raise NinmuConnectionError("client is shut down")
            try:
                line = json.dumps(request, ensure_ascii=False)
                self._stdin.write(line + "\n")
                self._stdin.flush()
            except (OSError, ValueError) as exc:
                raise NinmuConnectionError(f"write failed: {exc}") from exc

            if self._stdout is None:
                raise NinmuConnectionError("stdout closed")

            try:
                try:
                    import select as _select
                    ready = _select.select([self._stdout], [], [], _READ_TIMEOUT)[0]
                except (TypeError, ValueError, OSError):
                    # Mock/non-real file descriptor — fall through to blocking read
                    ready = True
                if not ready:
                    raise NinmuTimeoutError(
                        f"no response within {_READ_TIMEOUT}s for method {method!r}"
                    )
                response_line = self._stdout.readline()
            except (OSError, ValueError) as exc:
                raise NinmuConnectionError(f"read failed: {exc}") from exc

        if not response_line:
            self._check_process_alive()
            raise NinmuConnectionError("empty response from server")

        try:
            response = json.loads(response_line)
        except json.JSONDecodeError as exc:
            retrying = attempt < _MAX_RETRIES - 1
            if retrying:
                return self._send_request_with_retry(method, params, attempt + 1)
            raise NinmuConnectionError(f"invalid JSON response: {response_line!r}") from exc

        if "error" in response:
            err = response["error"]
            code = err.get("code", -32000)
            msg = err.get("message", "unknown error")
            if code in _RETRY_CODES and attempt < _MAX_RETRIES - 1:
                return self._send_request_with_retry(method, params, attempt + 1)
            if code == -32601:
                raise NinmuProtocolError(code, msg)
            raise NinmuRuntimeError(f"[{code}] {msg}")

        return response.get("result", {})

    def _check_process_alive(self) -> None:
        retcode = self._proc.poll()
        if retcode is not None:
            raise NinmuConnectionError(
                f"process exited with code {retcode}"
            )

    # ------------------------------------------------------------------
    # RPC methods
    # ------------------------------------------------------------------

    def ping(self) -> dict[str, Any]:
        """Check that the server is alive."""
        return self._send_request("ping")

    def create_session(
        self,
        model: str | None = None,
        system_prompt: list[str] | None = None,
    ) -> str:
        """Create a new session and return its ID."""
        params: dict[str, Any] = {}
        if model is not None:
            params["model"] = model
        if system_prompt is not None:
            params["system_prompt"] = system_prompt
        result = self._send_request("session.create", params)
        return result["sessionId"]

    def run_turn(self, session_id: str, input_text: str) -> str:
        """Run a single turn in a session and return the result."""
        result = self._send_request("session.turn", {
            "session_id": session_id,
            "input": input_text,
        })
        if result.get("status") == "error":
            raise NinmuRuntimeError(result.get("error", "unknown error"))
        return result.get("summary", "")

    def list_sessions(self) -> list[dict[str, Any]]:
        """List all active sessions."""
        result = self._send_request("session.list")
        return result.get("sessions", [])

    def destroy_session(self, session_id: str) -> None:
        """Destroy a session."""
        self._send_request("session.destroy", {"session_id": session_id})

    def fork_session(
        self, session_id: str, node_id: str, new_branch_id: str
    ) -> str:
        """Fork the session tree at a node, returning the new session ID."""
        result = self._send_request("session.tree.fork", {
            "session_id": session_id,
            "node_id": node_id,
            "new_branch_id": new_branch_id,
        })
        return result.get("sessionId", result.get("activeId", ""))

    def navigate(self, session_id: str, node_id: str) -> str:
        """Navigate to a different node in the session tree."""
        result = self._send_request("session.tree.navigate", {
            "session_id": session_id,
            "node_id": node_id,
        })
        return result.get("activeId", "")

    def path(self, session_id: str) -> list[dict[str, Any]]:
        """Get the current path through the session tree."""
        result = self._send_request("session.tree.path", {
            "session_id": session_id,
        })
        return result.get("path", [])

    def subscribe(
        self, session_id: str | None = None
    ) -> Generator[dict[str, Any], None, None]:
        """Subscribe to session events.

        This sends one ``events.subscribe`` request, reads all returned
        event lines, then returns.  Each yielded dict is a JSON-RPC
        notification with method ``events.stream``.
        """
        req_id = self._next_id()
        request = {
            "jsonrpc": "2.0",
            "method": "events.subscribe",
            "params": {"session_id": session_id} if session_id else {},
            "id": req_id,
        }

        with self._lock:
            if self._closed or self._stdin is None:
                raise NinmuConnectionError("client is shut down")
            try:
                self._stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
                self._stdin.flush()
            except (OSError, ValueError) as exc:
                raise NinmuConnectionError(f"write failed: {exc}") from exc

            if self._stdout is None:
                raise NinmuConnectionError("stdout closed")

            while True:
                try:
                    line = self._stdout.readline()
                except (OSError, ValueError) as exc:
                    raise NinmuConnectionError(f"read failed: {exc}") from exc

                if not line:
                    break

                try:
                    data = json.loads(line)
                except json.JSONDecodeError:
                    continue

                if data.get("id") == req_id:
                    break  # the response to our subscribe call
                yield data

    def shutdown(self) -> None:
        """Shut down the RPC server and clean up."""
        if self._closed:
            return
        self._closed = True

        # Fire-and-forget: write the shutdown request, don't wait for a response.
        with self._lock:
            if self._stdin is not None:
                try:
                    request = json.dumps({
                        "jsonrpc": "2.0",
                        "method": "shutdown",
                        "params": {},
                        "id": self._next_id(),
                    }, ensure_ascii=False)
                    self._stdin.write(request + "\n")
                    self._stdin.flush()
                except (OSError, ValueError):
                    pass

        # Close stdin so the server sees EOF.
        try:
            if self._proc.stdin:
                self._proc.stdin.close()
        except OSError:
            pass

        try:
            self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            self._proc.wait()

        self._stdin = None
        self._stdout = None
