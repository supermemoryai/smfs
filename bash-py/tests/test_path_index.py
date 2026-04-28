from __future__ import annotations

from supermemory_bash._path_index import PathIndex


def test_find_ancestor_file_returns_none_when_no_ancestor_is_file() -> None:
    idx = PathIndex()
    idx.insert("/notes/foo.md", "doc-1")
    assert idx.find_ancestor_file("/work/bar.md") is None


def test_find_ancestor_file_finds_immediate_parent() -> None:
    idx = PathIndex()
    idx.insert("/foo.md", "doc-1")
    assert idx.find_ancestor_file("/foo.md/bar.md") == "/foo.md"


def test_find_ancestor_file_walks_multiple_levels() -> None:
    idx = PathIndex()
    idx.insert("/a/b.md", "doc-1")
    assert idx.find_ancestor_file("/a/b.md/c/d.md") == "/a/b.md"


def test_find_ancestor_file_returns_none_for_top_level_path() -> None:
    idx = PathIndex()
    assert idx.find_ancestor_file("/foo.md") is None


def test_has_descendant_returns_false_when_nothing_under_path() -> None:
    idx = PathIndex()
    idx.insert("/foo.md", "doc-1")
    assert idx.has_descendant("/bar") is False


def test_has_descendant_returns_true_when_descendant_doc_exists() -> None:
    idx = PathIndex()
    idx.insert("/foo.md/bar.md", "doc-1")
    assert idx.has_descendant("/foo.md") is True


def test_has_descendant_does_not_match_unrelated_prefixes() -> None:
    idx = PathIndex()
    idx.insert("/foo.markdown", "doc-1")
    assert idx.has_descendant("/foo.md") is False


def test_has_descendant_false_for_root() -> None:
    idx = PathIndex()
    idx.insert("/foo.md", "doc-1")
    assert idx.has_descendant("/") is False
    assert idx.has_descendant("") is False


def test_has_descendant_handles_trailing_slash() -> None:
    idx = PathIndex()
    idx.insert("/work/notes.md", "doc-1")
    assert idx.has_descendant("/work") is True
    assert idx.has_descendant("/work/") is True
