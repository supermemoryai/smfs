from __future__ import annotations

import asyncio
from typing import Any

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

    # --- Documents ---

    async def documents_list(self, params: dict[str, Any]) -> dict[str, Any]:
        result: dict[str, Any] = await self._request("POST", "/v3/documents/list", json=params)
        return result

    async def documents_add(self, params: dict[str, Any]) -> dict[str, Any]:
        result: dict[str, Any] = await self._request("POST", "/v3/documents", json=params)
        return result

    async def documents_update(self, doc_id: str, params: dict[str, Any]) -> dict[str, Any]:
        result: dict[str, Any] = await self._request("PATCH", f"/v3/documents/{doc_id}", json=params)
        return result

    async def documents_delete(self, doc_id: str) -> dict[str, Any]:
        result: dict[str, Any] = await self._request("DELETE", f"/v3/documents/{doc_id}")
        return result

    async def documents_delete_bulk(self, params: dict[str, Any]) -> dict[str, Any]:
        result: dict[str, Any] = await self._request("DELETE", "/v3/documents/bulk", json=params)
        return result

    # --- Search ---

    async def search_memories(self, params: dict[str, Any]) -> dict[str, Any]:
        result: dict[str, Any] = await self._request("POST", "/v4/search", json=params)
        return result

    # --- Container tags ---

    async def update_container_tag(self, tag: str, body: dict[str, Any]) -> dict[str, Any]:
        from urllib.parse import quote

        result: dict[str, Any] = await self._request(
            "PATCH", f"/v3/container-tags/{quote(tag, safe='')}", json=body
        )
        return result

    # --- Profile ---

    async def profile(self, params: dict[str, Any]) -> dict[str, Any]:
        result: dict[str, Any] = await self._request("POST", "/v4/profile", json=params)
        return result
