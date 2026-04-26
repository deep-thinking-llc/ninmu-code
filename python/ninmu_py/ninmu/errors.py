"""Error types for ninmu-py."""


class NinmuError(Exception):
    """Base error for all ninmu-py exceptions."""


class NinmuConnectionError(NinmuError):
    """The ninmu RPC subprocess died or cannot be started."""


class NinmuProtocolError(NinmuError):
    """The server returned a JSON-RPC protocol error (method not found, etc.)."""

    def __init__(self, code: int, message: str) -> None:
        self.code = code
        self.message = message
        super().__init__(f"[{code}] {message}")


class NinmuRuntimeError(NinmuError):
    """The server returned a generic runtime error."""
