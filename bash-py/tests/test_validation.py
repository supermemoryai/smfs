from __future__ import annotations

import pytest

from supermemory_bash._errors import FsError
from supermemory_bash._path_index import PathIndex
from supermemory_bash._validation import (
    ValidationCtx,
    assert_writable,
    rule_ancestor_not_file,
    rule_no_descendants,
    rule_reserved,
    rule_shape,
)


def _ctx(path: str, idx: PathIndex | None = None) -> ValidationCtx:
    return ValidationCtx(path=path, intent="addDoc", path_index=idx or PathIndex())


# ── rule_shape ────────────────────────────────────────────────────────────


def test_rule_shape_returns_none_for_valid_path() -> None:
    assert rule_shape(_ctx("/foo.md")) is None


def test_rule_shape_returns_einval_for_missing_extension() -> None:
    err = rule_shape(_ctx("/memory"))
    assert isinstance(err, FsError)
    assert err.code == "EINVAL"


def test_rule_shape_returns_enametoolong_for_path_overflow() -> None:
    err = rule_shape(_ctx(f"/{'a' * 5000}.md"))
    assert isinstance(err, FsError)
    assert err.code == "ENAMETOOLONG"


def test_rule_shape_returns_enametoolong_for_basename_overflow() -> None:
    err = rule_shape(_ctx(f"/{'a' * 300}.md"))
    assert isinstance(err, FsError)
    assert err.code == "ENAMETOOLONG"


# ── rule_reserved ─────────────────────────────────────────────────────────


def test_rule_reserved_allows_non_reserved() -> None:
    assert rule_reserved(_ctx("/notes/foo.md")) is None


def test_rule_reserved_blocks_profile_md() -> None:
    err = rule_reserved(_ctx("/profile.md"))
    assert isinstance(err, FsError)
    assert err.code == "EPERM"


def test_rule_reserved_does_not_block_subpath() -> None:
    assert rule_reserved(_ctx("/sub/profile.md")) is None


# ── rule_ancestor_not_file ────────────────────────────────────────────────


def test_rule_ancestor_returns_none_when_no_ancestor_is_file() -> None:
    assert rule_ancestor_not_file(_ctx("/notes/foo.md")) is None


def test_rule_ancestor_returns_enotdir_when_ancestor_is_file() -> None:
    idx = PathIndex()
    idx.insert("/foo.md", "doc-1")
    err = rule_ancestor_not_file(_ctx("/foo.md/bar.md", idx))
    assert isinstance(err, FsError)
    assert err.code == "ENOTDIR"
    assert "/foo.md" in str(err)


def test_rule_ancestor_walks_multiple_levels() -> None:
    idx = PathIndex()
    idx.insert("/a/b.md", "doc-1")
    err = rule_ancestor_not_file(_ctx("/a/b.md/c/d.md", idx))
    assert isinstance(err, FsError)
    assert err.code == "ENOTDIR"


# ── rule_no_descendants ───────────────────────────────────────────────────


def test_rule_no_descendants_returns_none_when_nothing_under_path() -> None:
    assert rule_no_descendants(_ctx("/foo.md")) is None


def test_rule_no_descendants_returns_eisdir_when_descendant_exists() -> None:
    idx = PathIndex()
    idx.insert("/foo.md/bar.md", "doc-1")
    err = rule_no_descendants(_ctx("/foo.md", idx))
    assert isinstance(err, FsError)
    assert err.code == "EISDIR"


def test_rule_no_descendants_does_not_match_unrelated_prefix() -> None:
    idx = PathIndex()
    idx.insert("/foo.markdown", "doc-1")
    assert rule_no_descendants(_ctx("/foo.md", idx)) is None


# ── assert_writable pipeline ──────────────────────────────────────────────


def test_assert_writable_passes_for_valid_path() -> None:
    assert_writable(_ctx("/notes/foo.md"))  # no exception


def test_assert_writable_raises_einval_on_shape_failure() -> None:
    with pytest.raises(FsError) as exc:
        assert_writable(_ctx("/memory"))
    assert exc.value.code == "EINVAL"


def test_assert_writable_raises_eperm_on_reserved() -> None:
    with pytest.raises(FsError) as exc:
        assert_writable(_ctx("/profile.md"))
    assert exc.value.code == "EPERM"


def test_assert_writable_raises_enotdir_on_ancestor_collision() -> None:
    idx = PathIndex()
    idx.insert("/foo.md", "doc-1")
    with pytest.raises(FsError) as exc:
        assert_writable(_ctx("/foo.md/bar.md", idx))
    assert exc.value.code == "ENOTDIR"


def test_assert_writable_raises_eisdir_on_descendant_collision() -> None:
    idx = PathIndex()
    idx.insert("/foo.md/bar.md", "doc-1")
    with pytest.raises(FsError) as exc:
        assert_writable(_ctx("/foo.md", idx))
    assert exc.value.code == "EISDIR"


def test_assert_writable_first_failure_wins_shape_before_reserved() -> None:
    # empty path should fail at shape (rule 1) before reserved (rule 2)
    with pytest.raises(FsError) as exc:
        assert_writable(_ctx(""))
    assert exc.value.code == "EINVAL"
