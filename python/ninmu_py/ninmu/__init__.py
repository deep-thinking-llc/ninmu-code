"""ninmu-py: Python adapter for Ninmu Code's JSON-RPC agent protocol."""

from .client import NinmuClient
from .errors import (
    NinmuBinaryError,
    NinmuConnectionError,
    NinmuError,
    NinmuProtocolError,
    NinmuRuntimeError,
    NinmuTimeoutError,
)

from .langchain_adapter import NinmuTool as LangChainNinmuTool
from .autogen_adapter import NinmuAgent as AutoGenNinmuAgent
from .crewai_adapter import ninmu_coding_tool

__all__ = [
    "NinmuClient",
    "NinmuError",
    "NinmuConnectionError",
    "NinmuProtocolError",
    "NinmuRuntimeError",
    "NinmuTimeoutError",
    "NinmuBinaryError",
    "LangChainNinmuTool",
    "AutoGenNinmuAgent",
    "ninmu_coding_tool",
]
