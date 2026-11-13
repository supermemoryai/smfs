from __future__ import annotations

import asyncio
from typing import Any
from urllib.parse import quote

import httpx

DEFAULT_BASE_URL = "https://api.supermemory.ai"
MAX_RETRIES = 5
INITIAL_BACKOFF_MS = 100


class SupermemoryClient:
    def __init__(self, api_key: str, base_url: str | None = None) -> None:
        self._api_key = api_key
        self._base_url = (base_url or DEFAULT_BASE_URL).rstrip("/")
        self._http = httpx.AsyncClient(
            base_url=self._base_url,
            headers={"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"},
            timeout=30.0,
        )

    async def close(self) -> None:
        await self._http.aclose()

    async def _request(self, method: str, path: str, **kwargs: Any) -> Any:
        backoff = INITIAL_BACKOFF_MS
        last_err: Exception | None = None
        for attempt in range(MAX_RETRIES):
            try:
                resp = await self._http.request(method, path, **kwargs)
                if resp.status_code >= 500:
                    last_err = httpx.HTTPStatusError(
                        f"{resp.status_code}", request=resp.request, response=resp
                    )
                    await asyncio.sleep(backoff / 1000)
                    backoff = min(backoff * 2, 10_000)
                    continue
                if resp.status_code >= 400:
                    raise httpx.HTTPStatusError(
                        f"{resp.status_code}: {resp.text}", request=resp.request, response=resp
                    )
                return resp.json() if resp.content else {}
            except httpx.HTTPStatusError:
                raise
            except Exception as e:
                last_err = e
                if attempt < MAX_RETRIES - 1:
                    await asyncio.sleep(backoff / 1000)
                    backoff = min(backoff * 2, 10_000)
        raise last_err or RuntimeError("request failed")

    def _path(self, namespace: str, suffix: str = "") -> str:
        if not namespace:
            raise ValueError("namespace must not be empty")
        return f"/ns/{quote(namespace, safe='')}{suffix}"

    async def documents_list(self, namespace: str, params: dict[str, Any]) -> dict[str, Any]:
        query = {key: params[key] for key in ("page", "limit", "sort", "order") if key in params}
        body: dict[str, Any] = {"include": {"content": params.get("includeContent", False)}}
        if "filepath" in params:
            body["filepath"] = params["filepath"]
        result = await self._request(
            "POST", self._path(namespace, "/list/documents"), params=query, json=body
        )
        docs = []
        for document in result["documents"]:
            system = document["system"]
            docs.append({**document, "status": system["status"],
                         "updatedAt": system["updatedAt"], "createdAt": system["createdAt"],
                         "filepath": system.get("filepath")})
        return {"memories": docs, "pagination": result["pagination"]}

    async def documents_add(self, namespace: str, params: dict[str, Any]) -> dict[str, Any]:
        result: dict[str, Any] = await self._request(
            "POST", self._path(namespace, "/document"), json=params
        )
        return result

    async def documents_update(
        self, namespace: str, doc_id: str, params: dict[str, Any]
    ) -> dict[str, Any]:
        result: dict[str, Any] = await self._request(
            "PATCH", self._path(namespace, f"/document/{quote(doc_id, safe='')}"), json=params
        )
        return result

    async def documents_delete(self, namespace: str, doc_id: str) -> dict[str, Any]:
        result = await self.documents_delete_bulk(namespace, {"ids": [doc_id]})
        errors = [error for error in result["errors"] if error["error"] != "Document not found"]
        if errors:
            request = httpx.Request("DELETE", self._base_url + self._path(namespace, "/document"))
            response = httpx.Response(409, request=request)
            raise httpx.HTTPStatusError(
                f"Document deletion failed: {errors}", request=request, response=response
            )
        return result

    async def documents_delete_bulk(
        self, namespace: str, params: dict[str, Any]
    ) -> dict[str, Any]:
        ids = params.get("ids")
        if ids is None:
            filepath = params.get("filepath")
            if not filepath:
                raise ValueError("Deletion requires explicit IDs or a non-empty filepath")
            ids = []
            page = 1
            while True:
                result = await self.documents_list(namespace, {
                    "page": page, "limit": 100, "filepath": filepath,
                })
                ids.extend(document["id"] for document in result["memories"]
                           if isinstance(document.get("filepath"), str)
                           and (document["filepath"].startswith(filepath)
                                if filepath.endswith("/") else document["filepath"] == filepath))
                if page >= result["pagination"]["totalPages"] or not result["memories"]:
                    break
                page += 1
        count = 0
        errors: list[dict[str, Any]] = []
        for offset in range(0, len(ids), 100):
            result = await self._request(
                "DELETE", self._path(namespace, "/document"), json={"ids": ids[offset:offset + 100]}
            )
            count += result["count"]
            errors.extend(
                error for error in result["errors"] if error["error"] != "Document not found"
            )
        return {"deletedCount": count, "errors": errors}

    async def search_memories(self, namespace: str, params: dict[str, Any]) -> dict[str, Any]:
        result: dict[str, Any] = await self._request(
            "POST", self._path(namespace, "/search"), json=params
        )
        for item in result["results"]:
            document = item.get("included", {}).get("document")
            item["documents"] = [document] if document else []
            item["filepath"] = item["system"].get("filepath")
        return result

    async def update_namespace(self, namespace: str, body: dict[str, Any]) -> dict[str, Any]:
        result: dict[str, Any] = await self._request("PATCH", self._path(namespace), json=body)
        return result

    async def profile(self, namespace: str) -> dict[str, Any]:
        result: dict[str, Any] = await self._request(
            "POST", self._path(namespace, "/profile"), json={}
        )
        for key in ("static", "dynamic"):
            result["profile"][key] = [item["memory"] for item in result["profile"][key]]
        return result
