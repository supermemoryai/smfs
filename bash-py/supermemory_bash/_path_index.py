from __future__ import annotations


class PathIndex:
    def __init__(self) -> None:
        self._files: dict[str, str] = {}
        self._by_doc_id: dict[str, str] = {}
        self._synthetic_dirs: set[str] = set()

    def insert(self, path: str, doc_id: str) -> None:
        existing = self._files.get(path)
        if existing and existing != doc_id:
            self._by_doc_id.pop(existing, None)
        self._files[path] = doc_id
        self._by_doc_id[doc_id] = path

    def resolve(self, path: str) -> str | None:
        return self._files.get(path)

    def find_path(self, doc_id: str) -> str | None:
        return self._by_doc_id.get(doc_id)

    def remove(self, path: str) -> None:
        doc_id = self._files.pop(path, None)
        if doc_id is not None:
            self._by_doc_id.pop(doc_id, None)

    def mark_synthetic_dir(self, path: str) -> None:
        if path in ("", "/"):
            return
        self._synthetic_dirs.add(path)

    def remove_synthetic_dir(self, path: str) -> None:
        self._synthetic_dirs.discard(path)

    def is_file(self, path: str) -> bool:
        return path in self._files

    def is_directory(self, path: str) -> bool:
        if path in ("", "/"):
            return True
        if path in self._synthetic_dirs:
            return True
        prefix = path if path.endswith("/") else f"{path}/"
        return any(f.startswith(prefix) for f in self._files)

    def find_ancestor_file(self, path: str) -> str | None:
        segments = [s for s in path.split("/") if s != ""]
        for i in range(len(segments) - 1, 0, -1):
            ancestor = "/" + "/".join(segments[:i])
            if ancestor in self._files:
                return ancestor
        return None

    def has_descendant(self, path: str) -> bool:
        if path in ("", "/"):
            return False
        prefix = path if path.endswith("/") else f"{path}/"
        return any(f.startswith(prefix) for f in self._files)

    def paths(self) -> list[str]:
        return sorted(self._files)

    def synthetic_dir_paths(self) -> list[str]:
        return sorted(self._synthetic_dirs)

    def size(self) -> int:
        return len(self._files)
