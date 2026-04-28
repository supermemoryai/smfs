from __future__ import annotations

import json
import time
from dataclasses import dataclass, field
from typing import Any, AsyncIterator

from ._client import SupermemoryClient
from ._errors import ebusy, eexist, efbig, eio, enoent
from ._path_index import PathIndex
from ._session_cache import CachedEntry, DocStatus, SessionCache
from ._validation import ValidationCtx, assert_writable


def _normalize_status(s: str) -> DocStatus:
    if s == "done":
        return "done"
    if s == "failed":
        return "failed"
    return "processing"


@dataclass
class DocResult:
    id: str
    content: str | bytes
    status: DocStatus
    error_reason: str | None = None
    virtual: bool = False


@dataclass
class DocSummary:
    id: str
    filepath: str
    status: DocStatus
    size: int
    mtime: float
    content: str | None = None


@dataclass
class DocStat:
    is_file: bool
    is_directory: bool
    size: int
    mtime: float
    id: str | None = None
    status: DocStatus | None = None


@dataclass
class SearchResult:
    id: str
    filepath: str | None = None
    memory: str | None = None
    chunk: str | None = None
    similarity: float = 0.0


@dataclass
class SearchResp:
    results: list[SearchResult] = field(default_factory=list)
    total: int | None = None
    timing: float | None = None


@dataclass
class RemoveByPrefixResult:
    deleted: int = 0
    errors: list[Exception] = field(default_factory=list)


PROFILE_HEADER = """# Memory Profile

This file is auto-generated from your memories. To change what appears
here, modify the source files in your folder.
"""


def format_profile(resp: dict[str, Any]) -> str:
    profile = resp.get("profile") or {}
    static_items = profile.get("static") or []
    dynamic_items = profile.get("dynamic") or []
    if not static_items and not dynamic_items:
        return f"{PROFILE_HEADER}\n(no memories extracted yet — write some files and check back in a few minutes)\n"
    parts: list[str] = [PROFILE_HEADER]
    if static_items:
        parts.append("\n## Core Knowledge\n")
        for item in static_items:
            parts.append(f"- {item}\n")
    if dynamic_items:
        parts.append("\n## Recent Context\n")
        for item in dynamic_items:
            parts.append(f"- {item}\n")
    return "".join(parts)


class SupermemoryVolume:
    PROFILE_PATH = "/profile.md"
    ALL_PATHS_TTL_MS = 60_000
    ALL_PATHS_HARD_CAP = 5000

    def __init__(
        self,
        client: SupermemoryClient,
        container_tag: str,
        *,
        path_index: PathIndex | None = None,
        cache: SessionCache | None = None,
        cache_ttl_ms: int | None = 150_000,
    ) -> None:
        self.client = client
        self.container_tag = container_tag
        self.path_index = path_index or PathIndex()
        self.cache = cache or SessionCache(ttl_ms=cache_ttl_ms)
        self._all_paths_cache: tuple[list[str], float] | None = None
        self._last_configured_paths: str | None = None

    async def _iter_container(
        self,
        filepath: str | None = None,
        include_content: bool = False,
    ) -> AsyncIterator[dict[str, Any]]:
        page = 1
        while True:
            params: dict[str, Any] = {
                "containerTags": [self.container_tag],
                "limit": 100,
                "page": page,
                "includeContent": include_content,
            }
            if filepath is not None:
                params["filepath"] = filepath
            resp = await self.client.documents_list(params)
            for m in resp.get("memories", []):
                yield m
            total_pages = resp.get("pagination", {}).get("totalPages", 1)
            if page >= total_pages:
                break
            page += 1

    async def _lookup_doc_id(self, path: str) -> str | None:
        cached = self.path_index.resolve(path)
        if cached:
            return cached
        try:
            resp = await self.client.documents_list({
                "containerTags": [self.container_tag],
                "limit": 1,
                "page": 1,
                "filepath": path,
            })
            m = (resp.get("memories") or [None])[0]
            if not m:
                return None
            if m.get("filepath") == path:
                doc_id: str = m["id"]
                self.path_index.insert(path, doc_id)
                return doc_id
            return None
        except Exception as err:
            raise eio(f"lookup_doc_id({path}): {err}") from err

    def _filter_arg_for(self, prefix: str, exact: bool) -> str | None:
        if not prefix:
            return None
        if exact:
            return prefix
        return prefix if prefix.endswith("/") else f"{prefix}/"

    async def add_doc(self, path: str, content: str | bytes) -> tuple[str, DocStatus]:
        if isinstance(content, (bytes, bytearray)):
            raise efbig(path)
        assert_writable(ValidationCtx(path=path, intent="addDoc", path_index=self.path_index))

        existing = self.path_index.resolve(path)
        try:
            if existing:
                resp = await self.client.documents_update(existing, {
                    "content": content,
                    "containerTag": self.container_tag,
                    "filepath": path,
                })
                doc_id = resp.get("id", existing)
                server_status = resp.get("status", "unknown")
            else:
                resp = await self.client.documents_add({
                    "content": content,
                    "containerTag": self.container_tag,
                    "filepath": path,
                })
                doc_id = resp["id"]
                server_status = resp.get("status", "unknown")
        except Exception as err:
            raise eio(f"add_doc({path}): {err}") from err

        status = _normalize_status(server_status)
        self.path_index.insert(path, doc_id)
        self.cache.set(path, content, status)
        return doc_id, status

    async def update_doc(self, path: str, content: str | bytes) -> tuple[str, DocStatus]:
        if not await self._lookup_doc_id(path):
            raise enoent(path)
        return await self.add_doc(path, content)

    def is_reserved_path(self, path: str) -> bool:
        return path == self.PROFILE_PATH

    async def fetch_profile(self) -> str:
        cached = self.cache.get(self.PROFILE_PATH)
        if cached and isinstance(cached.content, str):
            return cached.content
        try:
            resp = await self.client.profile({"containerTag": self.container_tag})
        except Exception as err:
            raise eio(f"profile: {err}") from err
        body = format_profile(resp)
        self.cache.set(self.PROFILE_PATH, body, "done")
        return body

    async def get_doc(self, path: str) -> DocResult | None:
        if self.is_reserved_path(path):
            body = await self.fetch_profile()
            return DocResult(id="virtual:profile", content=body, status="done", virtual=True)

        cached = self.cache.get(path)
        if cached:
            doc_id = self.path_index.resolve(path)
            if doc_id:
                return DocResult(id=doc_id, content=cached.content, status=cached.status)

        try:
            resp = await self.client.documents_list({
                "containerTags": [self.container_tag],
                "limit": 1,
                "page": 1,
                "includeContent": True,
                "filepath": path,
            })
        except Exception as err:
            raise eio(f"get_doc({path}): {err}") from err

        m = (resp.get("memories") or [None])[0]
        if not m or (m.get("filepath") is not None and m.get("filepath") != path):
            self.path_index.remove(path)
            self.cache.delete(path)
            return None

        status = _normalize_status(m.get("status", "unknown"))
        raw_content: str = m.get("content", "")
        content = raw_content
        error_reason: str | None = None

        if status == "failed":
            error_reason = (
                m.get("errorMessage")
                or m.get("errorReason")
                or m.get("error")
                or m.get("failureReason")
                or "(unknown)"
            )
            content = (
                f"[supermemory.error: processing-failed]\n\n"
                f"This document could not be processed.\nReason: {error_reason}"
            )

        if m.get("id"):
            self.path_index.insert(path, m["id"])
        self.cache.set(path, content, status)
        return DocResult(id=m["id"], content=content, status=status, error_reason=error_reason)

    async def remove_doc(self, path: str) -> None:
        doc_id = await self._lookup_doc_id(path)
        if not doc_id:
            return

        try:
            await self.client.documents_delete(doc_id)
        except Exception as err:
            status = getattr(err, "response", None)
            status_code = getattr(status, "status_code", None) if status else None
            if status_code == 409:
                raise ebusy(path) from err
            if status_code == 404:
                self.path_index.remove(path)
                self.cache.delete(path)
                return
            raise eio(f"remove_doc({path}): {err}") from err

        self.path_index.remove(path)
        self.cache.delete(path)

    async def remove_by_prefix(self, prefix: str) -> RemoveByPrefixResult:
        filter_arg = self._filter_arg_for(prefix, False)
        if filter_arg is None:
            return await self._remove_by_prefix_via_list(prefix)

        deleted = 0
        errors: list[Exception] = []
        failed_ids: set[str] = set()
        any_unattributed = False
        try:
            resp = await self.client.documents_delete_bulk({
                "containerTags": [self.container_tag],
                "filepath": filter_arg,
            })
            deleted = resp.get("deletedCount", 0)
            for e in resp.get("errors", []):
                errors.append(Exception(f"{e.get('id')}: {e.get('error')}"))
                if e.get("id"):
                    failed_ids.add(e["id"])
                else:
                    any_unattributed = True
        except Exception as err:
            errors.append(Exception(f"remove_by_prefix({prefix}): {err}"))
            return RemoveByPrefixResult(deleted=deleted, errors=errors)

        if not any_unattributed:
            for p in list(self.path_index.paths()):
                if p.startswith(prefix):
                    doc_id = self.path_index.resolve(p)
                    if doc_id not in failed_ids:
                        self.path_index.remove(p)
                        self.cache.delete(p)
            self._evict_synthetic_under(prefix)

        return RemoveByPrefixResult(deleted=deleted, errors=errors)

    def _evict_synthetic_under(self, prefix: str) -> None:
        dir_self = prefix[:-1] if prefix.endswith("/") else prefix
        for d in list(self.path_index.synthetic_dir_paths()):
            if d == dir_self or d.startswith(prefix):
                self.path_index.remove_synthetic_dir(d)

    async def _remove_by_prefix_via_list(self, prefix: str) -> RemoveByPrefixResult:
        matches: list[tuple[str, str]] = []
        async for m in self._iter_container(include_content=False):
            fp = m.get("filepath")
            if isinstance(fp, str) and fp.startswith(prefix):
                matches.append((m["id"], fp))
        if not matches:
            return RemoveByPrefixResult()

        deleted = 0
        errors: list[Exception] = []
        for i in range(0, len(matches), 100):
            batch = matches[i : i + 100]
            try:
                resp = await self.client.documents_delete_bulk({"ids": [item[0] for item in batch]})
                deleted += resp.get("deletedCount", 0)
                for e in resp.get("errors", []):
                    errors.append(Exception(f"{e.get('id')}: {e.get('error')}"))
            except Exception as err:
                for item in batch:
                    errors.append(Exception(f"{item[0]}: {err}"))

        erred_ids = set()
        for e in errors:
            eid = str(e).split(":")[0].strip()
            if eid:
                erred_ids.add(eid)
        for doc_id, fp in matches:
            if doc_id not in erred_ids:
                self.path_index.remove(fp)
                self.cache.delete(fp)
        if not errors:
            self._evict_synthetic_under(prefix)
        return RemoveByPrefixResult(deleted=deleted, errors=errors)

    async def move_doc(self, from_path: str, to_path: str) -> None:
        assert_writable(ValidationCtx(path=to_path, intent="moveDoc", path_index=self.path_index))
        doc_id = await self._lookup_doc_id(from_path)
        if not doc_id:
            raise enoent(from_path)
        if await self._lookup_doc_id(to_path):
            raise eexist(to_path)

        try:
            await self.client.documents_update(doc_id, {
                "containerTag": self.container_tag,
                "filepath": to_path,
            })
        except Exception as err:
            status = getattr(err, "response", None)
            status_code = getattr(status, "status_code", None) if status else None
            if status_code == 404:
                self.path_index.remove(from_path)
                self.cache.delete(from_path)
                raise enoent(from_path) from err
            if status_code == 409:
                raise ebusy(from_path) from err
            raise eio(f"move_doc({from_path} -> {to_path}): {err}") from err

        cached = self.cache.get(from_path)
        self.path_index.remove(from_path)
        self.path_index.insert(to_path, doc_id)
        if cached:
            self.cache.set(to_path, cached.content, cached.status)
            self.cache.delete(from_path)

    async def list_by_prefix(
        self,
        prefix: str,
        *,
        with_content: bool = False,
        exact: bool = False,
        limit: int | None = None,
    ) -> list[DocSummary]:
        out: list[DocSummary] = []
        max_items = limit or float("inf")
        filter_arg = self._filter_arg_for(prefix, exact)
        async for m in self._iter_container(filepath=filter_arg, include_content=with_content):
            fp = m.get("filepath")
            if not isinstance(fp, str):
                continue
            matches = (fp == prefix) if exact else fp.startswith(prefix)
            if not matches:
                continue
            status = _normalize_status(m.get("status", "unknown"))
            content = m.get("content") if isinstance(m.get("content"), str) else None
            updated = m.get("updatedAt", "")
            mtime = 0.0
            if updated:
                try:
                    from datetime import datetime, timezone
                    mtime = datetime.fromisoformat(updated.replace("Z", "+00:00")).timestamp()
                except Exception:
                    pass
            summary = DocSummary(
                id=m["id"],
                filepath=fp,
                status=status,
                size=len(content) if content else 0,
                mtime=mtime,
                content=content,
            )
            out.append(summary)
            self.path_index.insert(fp, m["id"])
            if with_content and content is not None:
                self.cache.set(fp, content, status)
            if len(out) >= max_items:
                break

        # Inject /profile.md at root listings so `ls /` surfaces the virtual file.
        if prefix in ("", "/") and not exact:
            already = any(s.filepath == self.PROFILE_PATH for s in out)
            if not already:
                out.append(
                    DocSummary(
                        id="virtual:profile",
                        filepath=self.PROFILE_PATH,
                        status="done",
                        size=0,
                        mtime=time.time(),
                    )
                )
        return out

    async def list_all_paths(self) -> list[str]:
        paths: list[str] = []
        async for m in self._iter_container(include_content=False):
            fp = m.get("filepath")
            if not isinstance(fp, str):
                continue
            paths.append(fp)
            self.path_index.insert(fp, m["id"])
            if len(paths) > self.ALL_PATHS_HARD_CAP:
                raise eio(
                    f"list_all_paths exceeded {self.ALL_PATHS_HARD_CAP} docs "
                    f"in container '{self.container_tag}'"
                )
        paths.sort()
        self._all_paths_cache = (paths, time.monotonic() * 1000)
        return paths

    def cached_all_paths(self) -> list[str]:
        if not self._all_paths_cache:
            return []
        paths, at = self._all_paths_cache
        if time.monotonic() * 1000 - at > self.ALL_PATHS_TTL_MS:
            return []
        return paths

    async def stat_doc(self, path: str) -> DocStat | None:
        if self.is_reserved_path(path):
            body = await self.fetch_profile()
            return DocStat(
                is_file=True,
                is_directory=False,
                size=len(body.encode()),
                mtime=time.time(),
                status="done",
            )

        if self.path_index.is_directory(path) and not self.path_index.is_file(path):
            return DocStat(is_file=False, is_directory=True, size=0, mtime=0.0)

        cached = self.cache.get(path)
        if cached:
            doc_id = self.path_index.resolve(path)
            if doc_id:
                size = len(cached.content.encode() if isinstance(cached.content, str) else cached.content)
                return DocStat(
                    id=doc_id, is_file=True, is_directory=False,
                    size=size, mtime=0.0, status=cached.status,
                )

        try:
            resp = await self.client.documents_list({
                "containerTags": [self.container_tag],
                "limit": 1,
                "page": 1,
                "includeContent": True,
                "filepath": path,
            })
        except Exception as err:
            raise eio(f"stat_doc({path}): {err}") from err

        m = (resp.get("memories") or [None])[0]
        if not m or (m.get("filepath") is not None and m.get("filepath") != path):
            self.path_index.remove(path)
            self.cache.delete(path)
            return None

        status = _normalize_status(m.get("status", "unknown"))
        raw_content: str = m.get("content", "")
        if m.get("id"):
            self.path_index.insert(path, m["id"])
        self.cache.set(path, raw_content, status)

        updated = m.get("updatedAt", "")
        mtime = 0.0
        if updated:
            try:
                from datetime import datetime
                mtime = datetime.fromisoformat(updated.replace("Z", "+00:00")).timestamp()
            except Exception:
                pass

        return DocStat(
            id=m.get("id"), is_file=True, is_directory=False,
            size=len(raw_content), mtime=mtime, status=status,
        )

    def mark_synthetic_dir(self, path: str) -> None:
        self.path_index.mark_synthetic_dir(path)

    async def is_dir_empty(self, path: str) -> bool:
        prefix = path if path == "/" else f"{path}/"
        probe = await self.list_by_prefix(prefix, limit=1)
        if probe:
            return False
        for d in self.path_index.synthetic_dir_paths():
            if d != path and d.startswith(prefix):
                return False
        return True

    async def move_tree(self, src: str, dest: str) -> RemoveByPrefixResult:
        src_prefix = src if src.endswith("/") else f"{src}/"
        dest_prefix = dest if dest.endswith("/") else f"{dest}/"
        entries = await self.list_by_prefix(src_prefix)
        errors: list[Exception] = []
        for e in entries:
            new_path = dest_prefix + e.filepath[len(src_prefix):]
            try:
                await self.move_doc(e.filepath, new_path)
            except Exception as err:
                errors.append(err)
        for d in list(self.path_index.synthetic_dir_paths()):
            if d == src:
                self.path_index.remove_synthetic_dir(d)
            elif d.startswith(src_prefix):
                self.path_index.remove_synthetic_dir(d)
                self.path_index.mark_synthetic_dir(dest_prefix + d[len(src_prefix):])
        self.path_index.mark_synthetic_dir(dest)
        return RemoveByPrefixResult(deleted=len(entries) - len(errors), errors=errors)

    async def copy_tree(self, src: str, dest: str) -> RemoveByPrefixResult:
        src_prefix = src if src.endswith("/") else f"{src}/"
        dest_prefix = dest if dest.endswith("/") else f"{dest}/"
        entries = await self.list_by_prefix(src_prefix, with_content=True)
        errors: list[Exception] = []
        for e in entries:
            new_path = dest_prefix + e.filepath[len(src_prefix):]
            try:
                await self.add_doc(new_path, e.content or "")
            except Exception as err:
                errors.append(err)
        for d in self.path_index.synthetic_dir_paths():
            if d.startswith(src_prefix):
                self.path_index.mark_synthetic_dir(dest_prefix + d[len(src_prefix):])
        self.path_index.mark_synthetic_dir(dest)
        return RemoveByPrefixResult(deleted=len(entries) - len(errors), errors=errors)

    async def search(self, q: str, filepath: str | None = None) -> SearchResp:
        try:
            body: dict[str, Any] = {
                "q": q,
                "containerTag": self.container_tag,
                "searchMode": "hybrid",
                "include": {"documents": True},
                "limit": 50,
            }
            if filepath is not None:
                body["filepath"] = filepath
            resp = await self.client.search_memories(body)
        except Exception as err:
            raise eio(f"search({q}): {err}") from err

        out: list[SearchResult] = []
        for r in resp.get("results", []):
            docs = r.get("documents", [])
            doc_id = (
                (docs[0].get("id") or docs[0].get("documentId")) if docs else r.get("id", "")
            )
            fp = r.get("filepath")
            if not isinstance(fp, str):
                fp = self.path_index.find_path(doc_id) if doc_id else None

            if filepath:
                wants_prefix = filepath.endswith("/")
                if not fp:
                    continue
                if wants_prefix:
                    if not fp.startswith(filepath):
                        continue
                elif fp != filepath:
                    continue

            out.append(SearchResult(
                id=doc_id,
                filepath=fp,
                memory=r.get("memory"),
                chunk=r.get("chunk"),
                similarity=r.get("similarity", 0.0),
            ))
        return SearchResp(results=out)

    async def configure_memory_paths(self, paths: list[str]) -> None:
        key = json.dumps(paths)
        if self._last_configured_paths == key:
            return
        try:
            await self.client.update_container_tag(
                self.container_tag, {"memoryFilesystemPaths": paths}
            )
        except Exception as err:
            raise eio(f"configure_memory_paths: {err}") from err
        self._last_configured_paths = key
