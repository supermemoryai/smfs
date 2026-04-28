from __future__ import annotations


class FsError(OSError):
    def __init__(self, code: str, errno: int, message: str) -> None:
        self.code = code
        super().__init__(errno, f"{code}: {message}")


def _make(code: str, errno: int, suffix: str) -> FsError:
    return FsError(code, errno, suffix)


def enoent(path: str) -> FsError:
    return _make("ENOENT", -2, f"no such file or directory, '{path}'")


def eperm(path: str, op: str | None = None) -> FsError:
    extra = f", {op}" if op else ""
    return _make("EPERM", -1, f"operation not permitted{extra} '{path}'")


def eio(reason: str) -> FsError:
    return _make("EIO", -5, f"I/O error, {reason}")


def eisdir(path: str) -> FsError:
    return _make("EISDIR", -21, f"is a directory, '{path}'")


def enotdir(path: str) -> FsError:
    return _make("ENOTDIR", -20, f"not a directory, '{path}'")


def enotempty(path: str) -> FsError:
    return _make("ENOTEMPTY", -39, f"directory not empty, '{path}'")


def eexist(path: str) -> FsError:
    return _make("EEXIST", -17, f"file already exists, '{path}'")


def enosys(op: str) -> FsError:
    return _make("ENOSYS", -38, f"function not supported, {op}")


def einval(reason: str) -> FsError:
    return _make("EINVAL", -22, f"invalid argument, {reason}")


def efbig(path: str) -> FsError:
    return _make("EFBIG", -27, f"file too large, '{path}'")


def ebusy(path: str) -> FsError:
    return _make("EBUSY", -16, f"resource busy or locked, '{path}'")


def enametoolong(path: str) -> FsError:
    return _make("ENAMETOOLONG", -36, f"file name too long, '{path}'")
