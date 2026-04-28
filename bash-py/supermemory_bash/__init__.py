from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from ._client import SupermemoryClient
from ._errors import FsError
from ._path_index import PathIndex
from ._session_cache import SessionCache
from ._shell import ExecResult, Shell
from ._tool_description import TOOL_DESCRIPTION
from ._volume import (
    DocResult,
    DocStat,
    DocSummary,
    RemoveByPrefixResult,
    SearchResp,
    SearchResult,
    SupermemoryVolume,
)

__all__ = [
    "create_bash",
    "CreateBashOptions",
    "CreateBashResult",
    "ExecResult",
    "Shell",
    "SupermemoryVolume",
    "DocResult",
    "DocStat",
    "DocSummary",
    "SearchResult",
    "SearchResp",
    "RemoveByPrefixResult",
    "FsError",
    "PathIndex",
    "SessionCache",
    "TOOL_DESCRIPTION",
]

@dataclass
class CreateBashOptions:
    api_key: str
    container_tag: str
    base_url: str | None = None
    eager_load: bool = True
    eager_content: bool = True
    cwd: str = "/"
    env: dict[str, str] | None = None
    cache_ttl_ms: int | None = 150_000


@dataclass
class CreateBashResult:
    bash: Shell
    volume: SupermemoryVolume
    tool_description: str
    configure_memory_paths: Any  # async callable
    refresh: Any  # async callable


async def create_bash(
    api_key: str,
    container_tag: str,
    *,
    base_url: str | None = None,
    eager_load: bool = True,
    eager_content: bool = True,
    cwd: str = "/",
    env: dict[str, str] | None = None,
    cache_ttl_ms: int | None = 150_000,
) -> CreateBashResult:
    client = SupermemoryClient(api_key, base_url=base_url)
    volume = SupermemoryVolume(client, container_tag, cache_ttl_ms=cache_ttl_ms)

    async def warm() -> None:
        await volume.list_by_prefix("/", with_content=eager_content)

    if eager_load:
        await warm()

    shell = Shell(volume, cwd=cwd, env=env)

    return CreateBashResult(
        bash=shell,
        volume=volume,
        tool_description=TOOL_DESCRIPTION,
        configure_memory_paths=volume.configure_memory_paths,
        refresh=warm,
    )
