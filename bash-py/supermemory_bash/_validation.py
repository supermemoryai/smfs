from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Literal

from ._errors import FsError, einval, eisdir, enametoolong, enotdir, eperm
from ._filepath import check_filepath, is_reserved_filepath
from ._path_index import PathIndex

WriteIntent = Literal["addDoc", "moveDoc"]


@dataclass
class ValidationCtx:
    path: str
    intent: WriteIntent
    path_index: PathIndex


ValidationRule = Callable[[ValidationCtx], "FsError | None"]


def rule_shape(ctx: ValidationCtx) -> FsError | None:
    reason = check_filepath(ctx.path)
    if reason is None:
        return None
    if reason in ("too_long", "basename_too_long"):
        return enametoolong(ctx.path)
    return einval(f"'{ctx.path}': {reason}")


def rule_reserved(ctx: ValidationCtx) -> FsError | None:
    if is_reserved_filepath(ctx.path):
        return eperm(ctx.path, ctx.intent)
    return None


def rule_ancestor_not_file(ctx: ValidationCtx) -> FsError | None:
    ancestor = ctx.path_index.find_ancestor_file(ctx.path)
    if ancestor:
        return enotdir(ancestor)
    return None


def rule_no_descendants(ctx: ValidationCtx) -> FsError | None:
    if ctx.path_index.has_descendant(ctx.path):
        return eisdir(ctx.path)
    return None


WRITE_RULES: list[ValidationRule] = [
    rule_shape,
    rule_reserved,
    rule_ancestor_not_file,
    rule_no_descendants,
]


def assert_writable(ctx: ValidationCtx) -> None:
    for rule in WRITE_RULES:
        err = rule(ctx)
        if err is not None:
            raise err
