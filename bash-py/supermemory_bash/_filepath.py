from __future__ import annotations

import re
from typing import Literal

RESERVED_FILEPATHS: frozenset[str] = frozenset({"/profile.md"})

FILEPATH_MAX_BYTES = 4096
BASENAME_MAX_BYTES = 255

_CONTROL_CHARS = re.compile(r"[\x00-\x1F\x7F]")
_VALID_BASENAME = re.compile(r"^\.?[^/]*\.[^/.]+$")

FilepathRejection = Literal[
    "not_string",
    "empty",
    "too_long",
    "not_absolute",
    "control_char",
    "double_slash",
    "empty_leaf",
    "basename_too_long",
    "missing_extension",
]


def check_filepath(value: str) -> FilepathRejection | None:
    if not isinstance(value, str):
        return "not_string"
    if len(value) == 0:
        return "empty"
    if len(value) > FILEPATH_MAX_BYTES:
        return "too_long"
    if not value.startswith("/"):
        return "not_absolute"
    if _CONTROL_CHARS.search(value):
        return "control_char"
    if "//" in value:
        return "double_slash"
    segments = value.split("/")[1:]
    if len(segments) == 0:
        return "empty_leaf"
    basename = segments[-1]
    if not basename:
        return "empty_leaf"
    if len(basename) > BASENAME_MAX_BYTES:
        return "basename_too_long"
    if not _VALID_BASENAME.match(basename):
        return "missing_extension"
    return None


def is_valid_filepath(value: str) -> bool:
    return check_filepath(value) is None


def is_reserved_filepath(value: str) -> bool:
    return value in RESERVED_FILEPATHS
