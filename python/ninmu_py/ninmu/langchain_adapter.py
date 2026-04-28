"""LangChain adapter — NinmuTool for use with LangChain agents."""

from __future__ import annotations

from typing import Any

from .client import NinmuClient

try:
    from langchain.tools import BaseTool

    class NinmuTool(BaseTool):
        """LangChain tool that delegates a coding subtask to a Ninmu Code agent.

        Usage::

            from ninmu.langchain_adapter import NinmuTool

            tools = [NinmuTool()]
            # Pass tools to your LangChain agent
        """

        name: str = "ninmu_coding_agent"
        description: str = (
            "Delegate a coding subtask to an autonomous AI coding agent. "
            "The agent will plan, write code, test, and return results. "
            "Provide the full task description as input."
        )

        _client: Any = None
        _session_id: str = ""

        def __init__(
            self,
            model: str = "claude-sonnet-4-6",
            system_prompt: list[str] | None = None,
            binary: str = "ninmu",
        ) -> None:
            super().__init__()
            if not model or not isinstance(model, str):
                raise ValueError("model must be a non-empty string")
            self._client = NinmuClient(model=model, system_prompt=system_prompt, binary=binary)
            self._session_id = ""

        def _run(self, prompt: str) -> str:
            """Execute the tool with the given prompt."""
            if not self._session_id:
                self._session_id = self._client.create_session()
            return self._client.run_turn(self._session_id, prompt)

        def close(self) -> None:
            """Clean up the RPC session."""
            if self._session_id:
                try:
                    self._client.destroy_session(self._session_id)
                except Exception:
                    pass
            self._client.shutdown()

    _LANGCHAIN_AVAILABLE = True
except ImportError as _e:
    _LANGCHAIN_AVAILABLE = False

    class NinmuTool:  # type: ignore[no-redef]
        """Placeholder when langchain is not installed."""

        def __init__(self, *args: Any, **kwargs: Any) -> None:
            raise ImportError(
                "langchain is required for NinmuTool. "
                "Install it: pip install langchain"
            )
