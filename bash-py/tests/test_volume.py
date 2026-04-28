from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest

from supermemory_bash._volume import (
    PROFILE_HEADER,
    SupermemoryVolume,
    format_profile,
)


def _make_volume(client: MagicMock | None = None) -> SupermemoryVolume:
    c = client or MagicMock()
    return SupermemoryVolume(c, "test-tag")


def test_is_reserved_path_true_for_profile_md() -> None:
    v = _make_volume()
    assert v.is_reserved_path("/profile.md") is True


def test_is_reserved_path_false_for_subpath() -> None:
    v = _make_volume()
    assert v.is_reserved_path("/sub/profile.md") is False


def test_is_reserved_path_false_for_other_files() -> None:
    v = _make_volume()
    assert v.is_reserved_path("/notes/foo.md") is False


def test_format_profile_with_static_and_dynamic() -> None:
    body = format_profile(
        {"profile": {"static": ["fact A", "fact B"], "dynamic": ["context X"]}}
    )
    assert PROFILE_HEADER in body
    assert "## Core Knowledge" in body
    assert "- fact A" in body
    assert "- fact B" in body
    assert "## Recent Context" in body
    assert "- context X" in body


def test_format_profile_empty_response() -> None:
    body = format_profile({"profile": {"static": [], "dynamic": []}})
    assert PROFILE_HEADER in body
    assert "no memories extracted yet" in body


def test_format_profile_handles_missing_keys() -> None:
    body = format_profile({})
    assert "no memories extracted yet" in body


@pytest.mark.asyncio
async def test_fetch_profile_calls_client_and_caches() -> None:
    client = MagicMock()
    client.profile = AsyncMock(
        return_value={"profile": {"static": ["s1"], "dynamic": ["d1"]}}
    )
    v = _make_volume(client)
    body = await v.fetch_profile()
    assert "- s1" in body and "- d1" in body
    # Second call should hit cache, not re-call client
    body2 = await v.fetch_profile()
    assert body2 == body
    assert client.profile.await_count == 1


@pytest.mark.asyncio
async def test_get_doc_returns_profile_for_reserved_path() -> None:
    client = MagicMock()
    client.profile = AsyncMock(
        return_value={"profile": {"static": ["fact"], "dynamic": []}}
    )
    v = _make_volume(client)
    result = await v.get_doc("/profile.md")
    assert result is not None
    assert result.id == "virtual:profile"
    assert result.virtual is True
    assert "- fact" in result.content


@pytest.mark.asyncio
async def test_stat_doc_returns_synthetic_stat_for_profile() -> None:
    client = MagicMock()
    client.profile = AsyncMock(
        return_value={"profile": {"static": ["fact"], "dynamic": []}}
    )
    v = _make_volume(client)
    stat = await v.stat_doc("/profile.md")
    assert stat is not None
    assert stat.is_file is True
    assert stat.is_directory is False
    assert stat.size > 0
    assert stat.status == "done"
