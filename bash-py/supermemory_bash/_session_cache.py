from __future__ import annotations

import time
from dataclasses import dataclass
from typing import Literal

DocStatus = Literal["done", "failed", "processing"]

DEFAULT_TTL_MS = 150_000


@dataclass
class CachedEntry:
    content: str | bytes
    status: DocStatus


class SessionCache:
    def __init__(
        self,
        ttl_ms: int | None = DEFAULT_TTL_MS,
        max_bytes: int = 50 * 1024 * 1024,
    ) -> None:
        self._entries: dict[str, tuple[str | bytes, DocStatus, float, int]] = {}
        self._current_bytes = 0
        self._ttl_ms = ttl_ms
        self._max_bytes = max_bytes

    def _now(self) -> float:
        return time.monotonic() * 1000

    def get(self, path: str) -> CachedEntry | None:
        entry = self._entries.get(path)
        if entry is None:
            return None
        content, status, expires_at, nbytes = entry
        if self._now() >= expires_at:
            del self._entries[path]
            self._current_bytes -= nbytes
            return None
        # LRU: reinsert at end
        del self._entries[path]
        self._entries[path] = entry
        return CachedEntry(content=content, status=status)

    def set(self, path: str, content: str | bytes, status: DocStatus) -> None:
        existing = self._entries.pop(path, None)
        if existing is not None:
            self._current_bytes -= existing[3]

        nbytes = len(content.encode()) if isinstance(content, str) else len(content)
        if self._ttl_ms is None:
            expires_at = float("inf")
        elif self._ttl_ms == 0:
            expires_at = self._now()
        else:
            expires_at = self._now() + self._ttl_ms

        self._entries[path] = (content, status, expires_at, nbytes)
        self._current_bytes += nbytes

        while self._current_bytes > self._max_bytes and len(self._entries) > 1:
            oldest_key = next(iter(self._entries))
            evicted = self._entries.pop(oldest_key)
            self._current_bytes -= evicted[3]

    def delete(self, path: str) -> None:
        entry = self._entries.pop(path, None)
        if entry is not None:
            self._current_bytes -= entry[3]

    def clear(self) -> None:
        self._entries.clear()
        self._current_bytes = 0

    def size(self) -> int:
        return len(self._entries)

    def total_bytes(self) -> int:
        return self._current_bytes
