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

try:
    from .langchain_adapter import NinmuTool as LangChainNinmuTool  # noqa: F401
except ImportError:  # pragma: no cover
    pass

try:
    from .autogen_adapter import NinmuAgent as AutoGenNinmuAgent  # noqa: F401
except ImportError:  # pragma: no cover
    pass

try:
    from .crewai_adapter import ninmu_coding_tool  # noqa: F401
except ImportError:  # pragma: no cover
    pass

__all__ = [
    "NinmuClient",
    "NinmuError",
    "NinmuConnectionError",
    "NinmuProtocolError",
    "NinmuRuntimeError",
    "NinmuTimeoutError",
    "NinmuBinaryError",
]
