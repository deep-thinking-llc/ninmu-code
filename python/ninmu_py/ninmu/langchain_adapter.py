"""LangChain adapter — NinmuTool for use with LangChain agents."""

from __future__ import annotations

from typing import Any

from .client import NinmuClient


class NinmuTool:
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
    client: NinmuClient
    session_id: str

    def __init__(
        self,
        model: str = "claude-sonnet-4-6",
        system_prompt: list[str] | None = None,
        binary: str = "ninmu",
    ) -> None:
        self.model = model
        self.system_prompt = system_prompt
        self.binary = binary
        self.client = NinmuClient(model=model, system_prompt=system_prompt, binary=binary)
        self.session_id = ""

    def _run(self, prompt: str) -> str:
        """Execute the tool with the given prompt."""
        if not self.session_id:
            self.session_id = self.client.create_session()
        return self.client.run_turn(self.session_id, prompt)

    def close(self) -> None:
        """Clean up the RPC session."""
        if self.session_id:
            try:
                self.client.destroy_session(self.session_id)
            except Exception:
                pass
        self.client.shutdown()
