"""AutoGen adapter — NinmuAgent for use with AutoGen multi-agent conversations."""

from __future__ import annotations

from typing import Any

from .client import NinmuClient


class NinmuAgent:
    """AutoGen-style agent backed by a Ninmu Code RPC session.

    Usage::

        from ninmu.autogen_adapter import NinmuAgent

        agent = NinmuAgent(name="coder", model="claude-sonnet-4-6")
        reply = agent.generate_reply([{"role": "user", "content": "Write a test"}])
    """

    def __init__(
        self,
        name: str,
        model: str = "claude-sonnet-4-6",
        system_prompt: list[str] | None = None,
        binary: str = "ninmu",
    ) -> None:
        self.name = name
        self._client = NinmuClient(model=model, system_prompt=system_prompt, binary=binary)
        self._session_id = self._client.create_session()

    def generate_reply(
        self,
        messages: list[dict[str, Any]] | None = None,
        sender: Any = None,
        **kwargs: Any,
    ) -> tuple[bool, str]:
        """Generate a reply to the last message in the conversation.

        Returns (True, reply_text) on success, matching AutoGen's signature.
        """
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
