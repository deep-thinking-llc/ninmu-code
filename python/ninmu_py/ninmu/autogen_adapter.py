"""AutoGen adapter — NinmuAgent for use with AutoGen multi-agent conversations."""

from __future__ import annotations

from typing import Any

from .client import NinmuClient

try:
    from autogen import ConversableAgent

    class NinmuAgent(ConversableAgent):
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
            # Initialize the parent ConversableAgent with minimal defaults
            super().__init__(
                name=name,
                llm_config=False,  # We use NinmuClient, not OpenAI
                human_input_mode="NEVER",
            )
            self._ninmu_client = NinmuClient(model=model, system_prompt=system_prompt, binary=binary)
            self._ninmu_session_id = self._ninmu_client.create_session()
            # Register our custom reply function
            self.register_reply([ConversableAgent, None], NinmuAgent._ninmu_reply)

        def _ninmu_reply(
            self,
            messages: list[dict[str, Any]] | None = None,
            sender: Any = None,
            config: Any = None,
        ) -> tuple[bool, str]:
            """Generate a reply using the Ninmu Code RPC session."""
            if not messages:
                return True, ""
            prompt = messages[-1]["content"] if messages else ""
            if not isinstance(prompt, str):
                prompt = str(prompt)
            result = self._ninmu_client.run_turn(self._ninmu_session_id, prompt)
            return True, result

        def close(self) -> None:
            """Clean up the RPC session."""
            try:
                self._ninmu_client.destroy_session(self._ninmu_session_id)
            except Exception:
                pass
            self._ninmu_client.shutdown()

    _AUTOGEN_AVAILABLE = True
except ImportError as _e:
    _AUTOGEN_AVAILABLE = False

    class NinmuAgent:  # type: ignore[no-redef]
        """Placeholder when pyautogen is not installed."""

        def __init__(self, *args: Any, **kwargs: Any) -> None:
            raise ImportError(
                "pyautogen is required for NinmuAgent. "
                "Install it: pip install pyautogen"
            )
