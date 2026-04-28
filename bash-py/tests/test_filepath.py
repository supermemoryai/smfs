from __future__ import annotations

import pytest

from supermemory_bash._filepath import (
    BASENAME_MAX_BYTES,
    FILEPATH_MAX_BYTES,
    check_filepath,
    is_reserved_filepath,
    is_valid_filepath,
)


@pytest.mark.parametrize(
    "value,expected",
    [
        ("/notes/foo.md", True),
        ("/.gitignore", True),
        ("/.env", True),
        ("/a.b.c.tar.gz", True),
        ("/foo.bar/baz.md", True),
        ("/x/y/z.txt", True),
        ("/profile.md", True),
        ("/memory", False),
        ("/foo", False),
        ("/foo.", False),
        ("/", False),
        ("", False),
        ("foo.md", False),
        ("/foo.md/", False),
        ("//foo.md", False),
        ("/foo//bar.md", False),
        ("/foo\x00bar.md", False),
        ("/foo\x01bar.md", False),
        ("/foo\x7fbar.md", False),
    ],
)
def test_is_valid_filepath_table(value: str, expected: bool) -> None:
    assert is_valid_filepath(value) is expected


def test_rejects_when_total_exceeds_filepath_max_bytes() -> None:
    padding = "a" * FILEPATH_MAX_BYTES
    assert is_valid_filepath(f"/{padding}.md") is False


def test_rejects_when_basename_exceeds_basename_max_bytes() -> None:
    stem = "a" * BASENAME_MAX_BYTES
    assert is_valid_filepath(f"/{stem}.md") is False


def test_accepts_basename_at_basename_max_bytes() -> None:
    stem = "a" * (BASENAME_MAX_BYTES - 3)
    assert is_valid_filepath(f"/{stem}.md") is True


@pytest.mark.parametrize(
    "value,reason",
    [
        ("/memory", "missing_extension"),
        ("/foo.", "missing_extension"),
        ("/foo.md/", "empty_leaf"),
        ("//foo.md", "double_slash"),
        ("foo.md", "not_absolute"),
        ("", "empty"),
        ("/foo\x00.md", "control_char"),
    ],
)
def test_check_filepath_rejection_reasons(value: str, reason: str) -> None:
    assert check_filepath(value) == reason


def test_check_filepath_returns_none_for_valid() -> None:
    assert check_filepath("/notes/foo.md") is None


def test_is_reserved_filepath_profile_md() -> None:
    assert is_reserved_filepath("/profile.md") is True


def test_is_reserved_filepath_profile_md_bak() -> None:
    assert is_reserved_filepath("/profile.md.bak") is False


def test_is_reserved_filepath_subpath_profile_md() -> None:
    assert is_reserved_filepath("/sub/profile.md") is False


def test_is_reserved_filepath_no_prefix_match() -> None:
    assert is_reserved_filepath("/profile.markdown") is False
