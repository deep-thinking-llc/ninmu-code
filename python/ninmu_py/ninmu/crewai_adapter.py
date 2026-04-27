"""CrewAI adapter — ninmu_coding_tool for use with CrewAI agents."""

from __future__ import annotations

from typing import Any

from .client import NinmuClient


def ninmu_coding_tool(
    model: str = "claude-sonnet-4-6",
    system_prompt: list[str] | None = None,
    binary: str = "ninmu",
) -> Any:
    """Create a CrewAI-compatible tool wrapping a Ninmu Code RPC session.

    Usage::

        from ninmu.crewai_adapter import ninmu_coding_tool
        from crewai import Agent

        agent = Agent(
            role="coder",
            goal="Write code",
            tools=[ninmu_coding_tool()],
        )
    """
    import warnings
    try:
        from crewai import Tool as CrewAITool
    except ImportError:
        warnings.warn("crewai is not installed. Install with: pip install crewai")
        return None

    def _execute(prompt: str) -> str:
        client = NinmuClient(model=model, system_prompt=system_prompt, binary=binary)
        session_id = client.create_session()
        try:
            return client.run_turn(session_id, prompt)
        finally:
            try:
                client.destroy_session(session_id)
            except Exception:
                pass
            client.shutdown()

    return CrewAITool(
        name="ninmu_coding_agent",
        description="Delegate a coding task to an autonomous Ninmu Code agent. "
        "The agent will plan, write code, test, and return results.",
        func=_execute,
    )
