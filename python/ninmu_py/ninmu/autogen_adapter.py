"""AutoGen adapter — NinmuAgent for use with AutoGen multi-agent conversations."""

from __future__ import annotations

from typing import Any

from .client import NinmuClient

try:
    from autogen import ConversableAgent
    _AUTOGEN_AVAILABLE = True
except ImportError as _e:
    _AUTOGEN_AVAILABLE = False


class NinmuAgent:
    """AutoGen-style agent backed by a Ninmu Code RPC session."""

    def __init__(
        self,
        name: str,
        model: str = "claude-sonnet-4-6",
        system_prompt: list[str] | None = None,
        binary: str = "ninmu",
    ) -> None:
        if not _AUTOGEN_AVAILABLE:
            raise ImportError(
                "pyautogen is required for NinmuAgent. "
                "Install it: pip install pyautogen"
            )
        self.name = name
        self._client = NinmuClient(model=model, system_prompt=system_prompt, binary=binary)
        self._session_id = self._client.create_session()

    def generate_reply(
        self,
        messages: list[dict[str, Any]] | None = None,
        sender: Any = None,
        **kwargs: Any,
    ) -> tuple[bool, str]:
        """Generate a reply to the last message in the conversation."""
        if not messages:
            return True, ""
        prompt = messages[-1]["content"] if messages else ""
        if not isinstance(prompt, str):
            prompt = str(prompt)
        result = self._client.run_turn(self._session_id, prompt)
        return True, result

    def close(self) -> None:
        """Clean up the RPC session."""
        try:
            self._client.destroy_session(self._session_id)
        except Exception:
            pass
        self._client.shutdown()
