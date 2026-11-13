import type Supermemory from "supermemory";
import { describe, expect, it, vi } from "vitest";
import { FsError } from "../src/errors.js";
import { PathIndex } from "../src/path-index.js";
import { SessionCache } from "../src/session-cache.js";
import { formatProfile, SupermemoryVolume } from "../src/volume.js";

const fakeClient = {} as unknown as Supermemory;

describe("SupermemoryVolume constructor", () => {
  it("constructs with (client, containerTag) using default options", () => {
    const v = new SupermemoryVolume(fakeClient, "test-tag");
    expect(v).toBeInstanceOf(SupermemoryVolume);
  });

  it("stores containerTag and exposes it as a readable property", () => {
    const v = new SupermemoryVolume(fakeClient, "my-container");
    expect(v.containerTag).toBe("my-container");
  });

  it("stores the SDK client reference", () => {
    const v = new SupermemoryVolume(fakeClient, "tag");
    expect(v.client).toBe(fakeClient);
  });

  it("creates a default PathIndex when options.pathIndex is omitted", () => {
    const v = new SupermemoryVolume(fakeClient, "tag");
    expect(v.pathIndex).toBeInstanceOf(PathIndex);
    expect(v.pathIndex.size()).toBe(0);
  });

  it("uses the provided pathIndex when options.pathIndex is passed", () => {
    const customIndex = new PathIndex();
    customIndex.insert("/seeded.md", "doc-seed");
    const v = new SupermemoryVolume(fakeClient, "tag", { pathIndex: customIndex });
    expect(v.pathIndex).toBe(customIndex);
    expect(v.pathIndex.resolve("/seeded.md")).toBe("doc-seed");
  });

  it("creates a default SessionCache when options.cache is omitted", () => {
    const v = new SupermemoryVolume(fakeClient, "tag");
    expect(v.cache).toBeInstanceOf(SessionCache);
    expect(v.cache.size()).toBe(0);
  });

  it("uses the provided cache when options.cache is passed", () => {
    const customCache = new SessionCache();
    customCache.set("/seeded.md", "x", "done");
    const v = new SupermemoryVolume(fakeClient, "tag", { cache: customCache });
    expect(v.cache).toBe(customCache);
    expect(v.cache.size()).toBe(1);
  });

  it("propagates cacheOptions to the default SessionCache", () => {
    const v = new SupermemoryVolume(fakeClient, "tag", {
      cacheOptions: { maxBytes: 256, ttlMs: 1 },
    });
    // Confirm by overflowing the small byte cap
    v.cache.set("/a", "a".repeat(200), "done");
    v.cache.set("/b", "b".repeat(200), "done");
    expect(v.cache.size()).toBe(1); // /a evicted by tiny maxBytes
  });

  it("does not call any SDK method during construction", () => {
    const trackedClient = {
      add: vi.fn(),
      list: vi.fn(),
      documents: { get: vi.fn(), update: vi.fn(), delete: vi.fn() },
      search: vi.fn(),
      profile: vi.fn(),
      patch: vi.fn(),
    } as unknown as Supermemory;

    new SupermemoryVolume(trackedClient, "tag");

    const called = [
      (trackedClient.add as ReturnType<typeof vi.fn>).mock.calls.length,
      (trackedClient.documents.get as ReturnType<typeof vi.fn>).mock.calls.length,
      (trackedClient.list as ReturnType<typeof vi.fn>).mock.calls.length,
      (trackedClient.search as ReturnType<typeof vi.fn>).mock.calls.length,
      (trackedClient.profile as ReturnType<typeof vi.fn>).mock.calls.length,
    ];
    expect(called.every((n) => n === 0)).toBe(true);
  });
});

// Empty list response is the default for lookupDocId fallback in tests where the
// path isn't pre-inserted into PathIndex. Tests that need a hit can override.
const emptyListResp = {
  documents: [],
  pagination: { currentPage: 1, totalPages: 1, totalItems: 0 },
};

function makeVolumeWithMocks(
  addResp: { id: string; status: string } = { id: "doc-1", status: "queued" },
  updateResp: { id: string; status: string } = { id: "doc-1", status: "done" },
) {
  const add = vi.fn().mockResolvedValue(addResp);
  const update = vi.fn().mockResolvedValue(updateResp);
  const list = vi.fn().mockResolvedValue(emptyListResp);
  const client = {
    add,
    list,
    documents: { update },
  } as unknown as Supermemory;
  const volume = new SupermemoryVolume(client, "test-tag");
  return { volume, add, update, list };
}

describe("SupermemoryVolume.addDoc / updateDoc", () => {
  it("addDoc with new path calls client.documents.add with { content, containerTag, filepath }", async () => {
    const { volume, add } = makeVolumeWithMocks();
    await volume.addDoc("/notes/a.md", "hello");
    expect(add).toHaveBeenCalledTimes(1);
    expect(add).toHaveBeenCalledWith(
      "test-tag",
      { content: "hello", id: expect.any(String) },
      {
        additionalBodyParameters: { filepath: "/notes/a.md" },
      },
    );
  });

  it("addDoc with new path inserts into pathIndex", async () => {
    const { volume } = makeVolumeWithMocks({ id: "doc-xyz", status: "queued" });
    await volume.addDoc("/notes/a.md", "hello");
    expect(volume.pathIndex.resolve("/notes/a.md")).toBe("doc-xyz");
  });

  it("addDoc populates cache with content + normalized status (self-write visibility)", async () => {
    const { volume } = makeVolumeWithMocks({ id: "doc-1", status: "queued" });
    await volume.addDoc("/notes/a.md", "hello");
    const cached = volume.cache.get("/notes/a.md");
    expect(cached?.content).toBe("hello");
    expect(cached?.status).toBe("processing");
  });

  it("addDoc returns { id, status } with status normalized", async () => {
    const { volume } = makeVolumeWithMocks({ id: "doc-1", status: "queued" });
    const result = await volume.addDoc("/notes/a.md", "hello");
    expect(result).toEqual({ id: "doc-1", status: "processing" });
  });

  it("addDoc with existing path calls client.documents.update(docId, ...)", async () => {
    const { volume, add, update } = makeVolumeWithMocks();
    volume.pathIndex.insert("/notes/a.md", "doc-existing");
    await volume.addDoc("/notes/a.md", "updated content");
    expect(add).not.toHaveBeenCalled();
    expect(update).toHaveBeenCalledTimes(1);
    expect(update).toHaveBeenCalledWith(
      "test-tag",
      "doc-existing",
      { content: "updated content" },
      {
        additionalBodyParameters: { filepath: "/notes/a.md" },
      },
    );
  });

  it("addDoc returns 'done' when server returns 'done'", async () => {
    const { volume } = makeVolumeWithMocks({ id: "doc-1", status: "done" });
    const result = await volume.addDoc("/a.md", "x");
    expect(result.status).toBe("done");
  });

  it("addDoc returns 'failed' when server returns 'failed'", async () => {
    const { volume } = makeVolumeWithMocks({ id: "doc-1", status: "failed" });
    const result = await volume.addDoc("/a.md", "x");
    expect(result.status).toBe("failed");
  });

  it.each([
    ["queued"],
    ["extracting"],
    ["chunking"],
    ["embedding"],
    ["indexing"],
    ["unknown"],
    ["something-new-from-server"],
  ])("addDoc maps server status %s → 'processing'", async (serverStatus) => {
    const { volume } = makeVolumeWithMocks({ id: "doc-1", status: serverStatus });
    const result = await volume.addDoc("/a.md", "x");
    expect(result.status).toBe("processing");
  });

  it("addDoc with Uint8Array content throws EFBIG (binary deferred)", async () => {
    const { volume } = makeVolumeWithMocks();
    const bytes = new Uint8Array([1, 2, 3]);
    await expect(volume.addDoc("/binary.bin", bytes)).rejects.toThrow(/EFBIG/);
  });

  it("addDoc throws eio when SDK call fails", async () => {
    const add = vi.fn().mockRejectedValue(new Error("network down"));
    const client = { add, documents: { update: vi.fn() } } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    await expect(volume.addDoc("/a.md", "x")).rejects.toThrow(/EIO/);
  });

  it("updateDoc on path not in index throws enoent", async () => {
    const { volume } = makeVolumeWithMocks();
    await expect(volume.updateDoc("/never.md", "x")).rejects.toThrow(/ENOENT/);
  });

  it("updateDoc on known path calls update and returns { id, status }", async () => {
    const { volume, update } = makeVolumeWithMocks(
      { id: "x", status: "queued" },
      { id: "doc-known", status: "done" },
    );
    volume.pathIndex.insert("/known.md", "doc-known");
    const result = await volume.updateDoc("/known.md", "new content");
    expect(update).toHaveBeenCalledWith(
      "test-tag",
      "doc-known",
      expect.any(Object),
      expect.any(Object),
    );
    expect(result).toEqual({ id: "doc-known", status: "done" });
  });
});

function makeVolumeWithGetMock(
  doc: Record<string, unknown> | null = {
    id: "doc-1",
    content: "hello",
    system: { status: "done" },
  },
) {
  // After the cache refactor, getDoc on cache miss uses documents.list with a
  // filepath filter rather than documents.get(id). The mock list returns a single
  // memory matching the doc fixture (or empty when null).
  const list = vi.fn().mockResolvedValue(
    doc
      ? {
          documents: [
            {
              ...doc,
              system: doc.system ?? {
                status: doc.status,
                filepath: doc.filepath,
                updatedAt: doc.updatedAt,
              },
            },
          ],
          pagination: { currentPage: 1, totalPages: 1, totalItems: 1 },
        }
      : emptyListResp,
  );
  const client = {
    add: vi.fn(),
    list,
    documents: { update: vi.fn(), get: vi.fn() },
  } as unknown as Supermemory;
  const volume = new SupermemoryVolume(client, "test-tag");
  return { volume, list };
}

describe("SupermemoryVolume.getDoc", () => {
  it("returns null when wire confirms the path doesn't exist", async () => {
    const { volume, list } = makeVolumeWithGetMock(null); // empty list response
    const result = await volume.getDoc("/never-added.md");
    expect(result).toBeNull();
    expect(list).toHaveBeenCalledTimes(1);
  });

  it("returns from cache without calling SDK when cache is populated", async () => {
    const { volume, list } = makeVolumeWithGetMock();
    volume.pathIndex.insert("/cached.md", "doc-c");
    volume.cache.set("/cached.md", "cached-content", "done");
    const result = await volume.getDoc("/cached.md");
    expect(list).not.toHaveBeenCalled();
    expect(result).toEqual({ id: "doc-c", content: "cached-content", status: "done" });
  });

  it("on cache miss calls documents.list with filepath filter and returns content", async () => {
    const { volume, list } = makeVolumeWithGetMock({
      id: "doc-x",
      content: "fetched",
      system: { status: "done" },
    });
    const result = await volume.getDoc("/a.md");
    expect(list).toHaveBeenCalledTimes(1);
    expect(list).toHaveBeenCalledWith(
      "test-tag",
      "documents",
      {
        limit: 1,
        page: 1,
        include: { content: true },
      },
      { additionalBodyParameters: { filepath: "/a.md" } },
    );
    expect(result).toEqual({ id: "doc-x", content: "fetched", status: "done" });
  });

  it("status 'done' passes through", async () => {
    const { volume } = makeVolumeWithGetMock({
      id: "d",
      content: "ok",
      system: { status: "done" },
    });
    const result = await volume.getDoc("/a.md");
    expect(result?.status).toBe("done");
  });

  it.each([
    ["queued"],
    ["extracting"],
    ["chunking"],
    ["embedding"],
    ["indexing"],
    ["unknown"],
    ["something-new-from-server"],
  ])("normalizes server status %s → 'processing'", async (serverStatus) => {
    const { volume } = makeVolumeWithGetMock({
      id: "d",
      content: "x",
      system: { status: serverStatus },
    });
    const result = await volume.getDoc("/a.md");
    expect(result?.status).toBe("processing");
  });

  it("status 'failed' with errorMessage rewrites content and populates errorReason", async () => {
    const { volume } = makeVolumeWithGetMock({
      id: "d",
      content: "partial",
      errorMessage: "extraction timeout",
      system: { status: "failed" },
    });
    const result = await volume.getDoc("/a.md");
    expect(result?.status).toBe("failed");
    expect(result?.errorReason).toBe("extraction timeout");
    expect(result?.content).toBe(
      "[supermemory.error: processing-failed]\n\nThis document could not be processed.\nReason: extraction timeout",
    );
  });

  it("status 'failed' with no error fields uses '(unknown)' as reason", async () => {
    const { volume } = makeVolumeWithGetMock({
      id: "d",
      content: "",
      system: { status: "failed" },
    });
    const result = await volume.getDoc("/a.md");
    expect(result?.errorReason).toBe("(unknown)");
    expect(result?.content).toMatch(/Reason: \(unknown\)$/);
  });

  it("populates cache after successful fetch with normalized status", async () => {
    const { volume } = makeVolumeWithGetMock({
      id: "d",
      content: "fresh",
      system: { status: "queued" },
    });
    await volume.getDoc("/a.md");
    const cached = volume.cache.get("/a.md");
    expect(cached?.content).toBe("fresh");
    expect(cached?.status).toBe("processing");
  });

  it("caches the formatted blurb for failed docs (subsequent reads stay structured)", async () => {
    const { volume, list } = makeVolumeWithGetMock({
      id: "d",
      content: "raw",
      errorMessage: "bad mime",
      system: { status: "failed" },
    });
    await volume.getDoc("/a.md");
    const second = await volume.getDoc("/a.md");
    expect(list).toHaveBeenCalledTimes(1); // second call is cache hit
    expect(second?.content).toContain("[supermemory.error: processing-failed]");
    expect(second?.content).toContain("Reason: bad mime");
  });

  it("returns null and evicts pathIndex when wire reports the doc missing", async () => {
    const { volume } = makeVolumeWithGetMock(null); // empty list response
    volume.pathIndex.insert("/stale.md", "doc-gone");
    const result = await volume.getDoc("/stale.md");
    expect(result).toBeNull();
    expect(volume.pathIndex.resolve("/stale.md")).toBeNull();
  });

  it("throws eio when SDK list throws", async () => {
    const list = vi
      .fn()
      .mockRejectedValue(Object.assign(new Error("network down"), { statusCode: 500 }));
    const client = {
      add: vi.fn(),
      list,
      documents: { update: vi.fn(), get: vi.fn() },
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    await expect(volume.getDoc("/a.md")).rejects.toMatchObject({ code: "EIO" });
    await expect(volume.getDoc("/a.md")).rejects.toBeInstanceOf(FsError);
  });

  it("treats null SDK content as empty string", async () => {
    const { volume } = makeVolumeWithGetMock({
      id: "d",
      content: null,
      system: { status: "done" },
    });
    const result = await volume.getDoc("/a.md");
    expect(result?.content).toBe("");
  });
});

function makeVolumeWithDeleteMock(opts: { rejectStatus?: number } = {}) {
  const del = opts.rejectStatus
    ? vi
        .fn()
        .mockRejectedValue(
          Object.assign(new Error(`http ${opts.rejectStatus}`), { statusCode: opts.rejectStatus }),
        )
    : vi.fn().mockResolvedValue({ count: 1, errors: [] });
  const list = vi.fn().mockResolvedValue(emptyListResp);
  const client = {
    add: vi.fn(),
    list,
    documents: { update: vi.fn(), get: vi.fn(), delete: del },
  } as unknown as Supermemory;
  const volume = new SupermemoryVolume(client, "tag");
  return { volume, del };
}

describe("SupermemoryVolume.removeDoc", () => {
  it("is a no-op when path is not in pathIndex (no SDK call)", async () => {
    const { volume, del } = makeVolumeWithDeleteMock();
    await volume.removeDoc("/never-here.md");
    expect(del).not.toHaveBeenCalled();
  });

  it("calls delete(docId) and evicts pathIndex + cache on success", async () => {
    const { volume, del } = makeVolumeWithDeleteMock();
    volume.pathIndex.insert("/a.md", "doc-a");
    volume.cache.set("/a.md", "cached", "done");
    await volume.removeDoc("/a.md");
    expect(del).toHaveBeenCalledWith("tag", { ids: ["doc-a"] });
    expect(volume.pathIndex.resolve("/a.md")).toBeNull();
    expect(volume.cache.get("/a.md")).toBeNull();
  });

  it("throws ebusy when SDK returns 409", async () => {
    const { volume } = makeVolumeWithDeleteMock({ rejectStatus: 409 });
    volume.pathIndex.insert("/a.md", "doc-a");
    await expect(volume.removeDoc("/a.md")).rejects.toMatchObject({ code: "EBUSY" });
    await expect(volume.removeDoc("/a.md")).rejects.toBeInstanceOf(FsError);
  });

  it("treats 404 as soft success and evicts local state", async () => {
    const { volume } = makeVolumeWithDeleteMock({ rejectStatus: 404 });
    volume.pathIndex.insert("/a.md", "doc-a");
    volume.cache.set("/a.md", "x", "done");
    await expect(volume.removeDoc("/a.md")).resolves.toBeUndefined();
    expect(volume.pathIndex.resolve("/a.md")).toBeNull();
    expect(volume.cache.get("/a.md")).toBeNull();
  });

  it("throws eio for non-409, non-404 errors", async () => {
    const { volume } = makeVolumeWithDeleteMock({ rejectStatus: 500 });
    volume.pathIndex.insert("/a.md", "doc-a");
    await expect(volume.removeDoc("/a.md")).rejects.toMatchObject({ code: "EIO" });
  });
});

function makeVolumeForBulk(
  listResponses: Array<{
    documents: Array<{ id: string; filepath?: string }>;
    pagination: { currentPage: number; totalPages: number; totalItems: number };
  }>,
  bulkResponse: {
    count: number;
    success: boolean;
    errors?: Array<{ id: string; error: string }>;
  } = { count: 0, success: true },
) {
  const list = vi.fn();
  for (const r of listResponses)
    list.mockResolvedValueOnce({
      ...r,
      documents: r.documents.map((d) => ({
        ...d,
        system: { filepath: d.filepath, status: "done", updatedAt: "2026-01-01" },
      })),
    });
  const deleteBulk = vi
    .fn()
    .mockResolvedValue({ ...bulkResponse, errors: bulkResponse.errors ?? [] });
  const client = {
    add: vi.fn(),
    list,
    documents: { update: vi.fn(), get: vi.fn(), delete: deleteBulk },
  } as unknown as Supermemory;
  const volume = new SupermemoryVolume(client, "tag");
  return { volume, list, deleteBulk };
}

describe("SupermemoryVolume.removeByPrefix", () => {
  it("calls deleteBulk once with {containerTags, filepath} and evicts matching paths", async () => {
    const { volume, deleteBulk } = makeVolumeForBulk(
      [
        {
          documents: [
            { id: "id1", filepath: "/notes/a.md" },
            { id: "id2", filepath: "/notes/b.md" },
          ],
          pagination: { currentPage: 1, totalPages: 1, totalItems: 2 },
        },
      ],
      { count: 2, success: true },
    );
    volume.pathIndex.insert("/notes/a.md", "id1");
    volume.pathIndex.insert("/notes/b.md", "id2");
    volume.pathIndex.insert("/other.md", "id3");
    const result = await volume.removeByPrefix("/notes/");
    expect(deleteBulk).toHaveBeenCalledTimes(1);
    expect(deleteBulk).toHaveBeenCalledWith("tag", { ids: ["id1", "id2"] });
    expect(result.deleted).toBe(2);
    expect(volume.pathIndex.resolve("/notes/a.md")).toBeNull();
    expect(volume.pathIndex.resolve("/notes/b.md")).toBeNull();
    expect(volume.pathIndex.resolve("/other.md")).toBe("id3");
  });

  it("empty prefix falls back to list+deleteBulk-by-ids (preserves filepath-NULL behavior)", async () => {
    const { volume, list, deleteBulk } = makeVolumeForBulk(
      [
        {
          documents: [{ id: "id1", filepath: "/a.md" }],
          pagination: { currentPage: 1, totalPages: 1, totalItems: 1 },
        },
      ],
      { count: 1, success: true },
    );
    await volume.removeByPrefix("");
    expect(list).toHaveBeenCalled();
    expect(deleteBulk).toHaveBeenCalledWith("tag", { ids: ["id1"] });
  });
});

// moveDoc is a single PATCH with filepath only (no content). Verified by B4.0
// wire probe + matches smfs's rename mechanism. docId stays stable.
describe("SupermemoryVolume.moveDoc", () => {
  function makeMoveClient() {
    const update = vi.fn().mockResolvedValue({ id: "doc-x", status: "done" });
    const client = {
      add: vi.fn(),
      list: vi.fn().mockResolvedValue(emptyListResp),
      documents: { update, get: vi.fn(), delete: vi.fn() },
    } as unknown as Supermemory;
    return { client, update };
  }

  it("throws ENOENT when source path is not in pathIndex", async () => {
    const { client } = makeMoveClient();
    const volume = new SupermemoryVolume(client, "tag");
    await expect(volume.moveDoc("/missing.md", "/dst.md")).rejects.toMatchObject({
      code: "ENOENT",
    });
  });

  it("throws EEXIST when destination is already in pathIndex", async () => {
    const { client } = makeMoveClient();
    const volume = new SupermemoryVolume(client, "tag");
    volume.pathIndex.insert("/src.md", "doc-src");
    volume.pathIndex.insert("/dst.md", "doc-dst");
    await expect(volume.moveDoc("/src.md", "/dst.md")).rejects.toMatchObject({
      code: "EEXIST",
    });
  });

  it("on success: PATCH with filepath only (no content), docId stable, cache moves", async () => {
    const { client, update } = makeMoveClient();
    const volume = new SupermemoryVolume(client, "tag");
    volume.pathIndex.insert("/old.md", "doc-x");
    volume.cache.set("/old.md", "body", "done");
    await volume.moveDoc("/old.md", "/new.md");
    expect(update).toHaveBeenCalledTimes(1);
    expect(update.mock.calls[0]?.[0]).toBe("tag");
    expect(update.mock.calls[0]?.[1]).toBe("doc-x");
    const body = update.mock.calls[0]?.[2] as Record<string, unknown>;
    expect(update.mock.calls[0]?.[3]).toEqual({
      additionalBodyParameters: { filepath: "/new.md" },
    });
    expect(body.content).toBeUndefined(); // critical: no content on PATCH
    expect(volume.pathIndex.resolve("/old.md")).toBeNull();
    expect(volume.pathIndex.resolve("/new.md")).toBe("doc-x"); // SAME docId
    expect(volume.cache.get("/old.md")).toBeNull();
    expect(volume.cache.get("/new.md")?.content).toBe("body");
  });
});

function makeListClient(
  pages: Array<{
    documents: Array<Record<string, unknown>>;
    pagination: { currentPage: number; totalPages: number; totalItems: number };
  }>,
) {
  const list = vi.fn().mockResolvedValue(emptyListResp);
  for (const p of pages)
    list.mockResolvedValueOnce({
      ...p,
      documents: p.documents.map((d) => ({
        ...d,
        system: d.system ?? { filepath: d.filepath, status: d.status, updatedAt: d.updatedAt },
      })),
    });
  const client = {
    add: vi.fn(),
    list,
    documents: { update: vi.fn(), get: vi.fn(), delete: vi.fn() },
  } as unknown as Supermemory;
  return { client, list };
}

describe("SupermemoryVolume.listByPrefix / listAllPaths / statDoc", () => {
  it("listByPrefix filters out memories without filepath AND outside prefix; honors limit", async () => {
    const { client } = makeListClient([
      {
        documents: [
          { id: "1", system: { filepath: "/notes/a.md", status: "done", updatedAt: "2026-01-01" } },
          { id: "2" }, // no filepath
          { id: "3", system: { filepath: "/other/b.md", status: "done", updatedAt: "2026-01-01" } },
          {
            id: "4",
            system: { filepath: "/notes/c.md", status: "queued", updatedAt: "2026-01-01" },
          },
          { id: "5", system: { filepath: "/notes/d.md", status: "done", updatedAt: "2026-01-01" } },
        ],
        pagination: { currentPage: 1, totalPages: 1, totalItems: 5 },
      },
    ]);
    const volume = new SupermemoryVolume(client, "tag");
    const result = await volume.listByPrefix("/notes/", { limit: 2 });
    expect(result.length).toBe(2);
    expect(result.map((s) => s.filepath)).toEqual(["/notes/a.md", "/notes/c.md"]);
    expect(result[1]?.status).toBe("processing");
  });

  it("listAllPaths throws EIO when container exceeds 5000 docs", async () => {
    // Simulate >5000 results across pages.
    const pages = [];
    for (let p = 1; p <= 51; p++) {
      const memories = Array.from({ length: 100 }, (_, i) => ({
        id: `id-${p}-${i}`,
        filepath: `/p${p}/${i}.md`,
      }));
      pages.push({
        documents: memories,
        pagination: { currentPage: p, totalPages: 51, totalItems: 5100 },
      });
    }
    const { client } = makeListClient(pages);
    const volume = new SupermemoryVolume(client, "tag");
    await expect(volume.listAllPaths()).rejects.toMatchObject({ code: "EIO" });
  });

  it("cachedAllPaths is empty before listAllPaths and populated after", async () => {
    const { client } = makeListClient([
      {
        documents: [
          { id: "1", filepath: "/a.md" },
          { id: "2", filepath: "/b.md" },
        ],
        pagination: { currentPage: 1, totalPages: 1, totalItems: 2 },
      },
    ]);
    const volume = new SupermemoryVolume(client, "tag");
    expect(volume.cachedAllPaths()).toEqual([]);
    await volume.listAllPaths();
    expect(volume.cachedAllPaths()).toEqual(["/a.md", "/b.md"]);
  });

  it("statDoc returns isDirectory:true for synthetic dirs and null for unknown paths", async () => {
    const { client } = makeListClient([]);
    const volume = new SupermemoryVolume(client, "tag");
    volume.markSyntheticDir("/empty");
    const dir = await volume.statDoc("/empty");
    expect(dir?.isDirectory).toBe(true);
    expect(dir?.isFile).toBe(false);
    const missing = await volume.statDoc("/never.md");
    expect(missing).toBeNull();
  });
});

describe("SupermemoryVolume.search (hybrid mode)", () => {
  it("maps /v4/search hybrid response to SearchResult with memory + chunk + filepath", async () => {
    const memories = vi.fn().mockResolvedValue({
      results: [
        {
          id: "mem-1",
          memory: "fact about plants",
          similarity: 0.9,
          system: { filepath: "/a.md" },
          included: { document: { id: "doc-1" } },
        },
        {
          id: "chunk-1",
          chunk: "raw chunk from doc-2",
          similarity: 0.7,
          system: { filepath: null },
          included: { document: { id: "doc-2" } },
        },
      ],
      total: 2,
      timing: 100,
    });
    const client = {
      add: vi.fn(),
      list: vi.fn(),
      documents: { update: vi.fn(), get: vi.fn(), delete: vi.fn() },
      search: memories,
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    // PathIndex reverse-lookup to fill filepath when wire returns null.
    volume.pathIndex.insert("/b.md", "doc-2");

    const all = await volume.search({ q: "anything" });
    expect(memories).toHaveBeenCalledTimes(1);
    expect(memories.mock.calls[0]?.[0]).toBe("tag");
    expect(memories.mock.calls[0]?.[1]).toMatchObject({
      query: "anything",
      searchMode: "hybrid",
      include: { documents: true },
    });
    expect(all.results.length).toBe(2);
    expect(all.results[0]).toMatchObject({
      memory: "fact about plants",
      similarity: 0.9,
      filepath: "/a.md",
    });
    expect(all.results[1]).toMatchObject({
      chunk: "raw chunk from doc-2",
      filepath: "/b.md", // fell back to PathIndex reverse-lookup
    });
  });

  it("filters by filepath: prefix match when trailing slash, exact otherwise", async () => {
    const memories = vi.fn().mockResolvedValue({
      results: [
        {
          id: "1",
          memory: "a",
          similarity: 0.9,
          system: { filepath: "/notes/a.md" },
          included: {},
        },
        {
          id: "2",
          memory: "b",
          similarity: 0.8,
          system: { filepath: "/other/b.md" },
          included: {},
        },
      ],
      total: 2,
      timing: 50,
    });
    const client = {
      add: vi.fn(),
      list: vi.fn(),
      documents: { update: vi.fn(), get: vi.fn(), delete: vi.fn() },
      search: memories,
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");

    const prefixed = await volume.search({ q: "x", filepath: "/notes/" });
    expect(prefixed.results.length).toBe(1);
    expect(prefixed.results[0]?.filepath).toBe("/notes/a.md");

    const exact = await volume.search({ q: "x", filepath: "/other/b.md" });
    expect(exact.results.length).toBe(1);
    expect(exact.results[0]?.filepath).toBe("/other/b.md");
  });
});

describe("SupermemoryVolume.configureMemoryPaths", () => {
  it("skips PATCH when called twice with identical paths; re-issues for different paths", async () => {
    const patch = vi.fn().mockResolvedValue({ count: 1, errors: [] });
    const client = {
      add: vi.fn(),
      list: vi.fn(),
      documents: { update: vi.fn(), get: vi.fn(), delete: vi.fn() },
      namespaces: { update: patch },
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "my-tag");
    await volume.configureMemoryPaths(["/notes/", "/journal/"]);
    await volume.configureMemoryPaths(["/notes/", "/journal/"]);
    expect(patch).toHaveBeenCalledTimes(1);
    await volume.configureMemoryPaths(["/different/"]);
    expect(patch).toHaveBeenCalledTimes(2);
  });
});

// B2.13: PathIndex is a cache, not authoritative. lookupDocId is the new
// "single way" read methods learn a docId, with a wire fallback on cache miss.
describe("SupermemoryVolume — wire fallback on PathIndex miss (B2.13)", () => {
  it("getDoc on cold PathIndex falls back to documents.list and finds the doc", async () => {
    // List response now carries content directly (includeContent:true is part
    // of the new cache-miss request shape).
    const list = vi.fn().mockResolvedValue({
      documents: [
        { id: "doc-z", content: "found", system: { filepath: "/cold.md", status: "done" } },
      ],
      pagination: { currentPage: 1, totalPages: 1, totalItems: 1 },
    });
    const client = {
      add: vi.fn(),
      list,
      documents: { update: vi.fn(), get: vi.fn(), delete: vi.fn() },
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    const result = await volume.getDoc("/cold.md");
    expect(list).toHaveBeenCalledTimes(1);
    expect(list).toHaveBeenCalledWith(
      "tag",
      "documents",
      {
        page: 1,
        limit: 1,
        include: { content: true },
      },
      { additionalBodyParameters: { filepath: "/cold.md" } },
    );
    expect(result?.content).toBe("found");
    expect(volume.pathIndex.resolve("/cold.md")).toBe("doc-z");
  });

  it("getDoc returns null when both PathIndex and the wire come up empty", async () => {
    const list = vi.fn().mockResolvedValue(emptyListResp);
    const get = vi.fn();
    const client = {
      add: vi.fn(),
      list,
      documents: { update: vi.fn(), get, delete: vi.fn() },
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    const result = await volume.getDoc("/never.md");
    expect(result).toBeNull();
    expect(get).not.toHaveBeenCalled();
  });

  it("listByPrefix ships server-side filepath filter for prefix queries (not '/' or empty)", async () => {
    const list = vi.fn().mockResolvedValue({
      documents: [
        { id: "1", system: { filepath: "/notes/a.md", status: "done", updatedAt: "2026-01-01" } },
      ],
      pagination: { currentPage: 1, totalPages: 1, totalItems: 1 },
    });
    const client = {
      add: vi.fn(),
      list,
      documents: { update: vi.fn(), get: vi.fn(), delete: vi.fn() },
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    await volume.listByPrefix("/notes/");
    expect(list.mock.calls[0]?.[3]).toEqual({ additionalBodyParameters: { filepath: "/notes/" } });
  });

  it("listByPrefix omits filepath when prefix is empty (full container)", async () => {
    const list = vi.fn().mockResolvedValue(emptyListResp);
    const client = {
      add: vi.fn(),
      list,
      documents: { update: vi.fn(), get: vi.fn(), delete: vi.fn() },
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    await volume.listByPrefix("");
    const body = (list.mock.calls[0]?.[3] as { additionalBodyParameters: Record<string, unknown> })
      .additionalBodyParameters;
    expect(body.filepath).toBeUndefined();
  });
});

describe("formatProfile", () => {
  it("renders empty profile with placeholder line", () => {
    const out = formatProfile({ profile: { static: [], dynamic: [] } });
    expect(out).toContain("# Memory Profile");
    expect(out).toContain("auto-generated");
    expect(out).toContain("(no memories extracted yet");
    expect(out).not.toContain("## Core Knowledge");
    expect(out).not.toContain("## Recent Context");
  });

  it("renders static-only profile with Core Knowledge section", () => {
    const out = formatProfile({
      profile: {
        static: [
          { id: "profile-0", memory: "likes coffee" },
          { id: "profile-1", memory: "lives in Austin" },
        ],
        dynamic: [],
      },
    });
    expect(out).toContain("## Core Knowledge");
    expect(out).toContain("- likes coffee");
    expect(out).toContain("- lives in Austin");
    expect(out).not.toContain("## Recent Context");
  });

  it("renders dynamic-only profile with Recent Context section", () => {
    const out = formatProfile({
      profile: { static: [], dynamic: [{ id: "profile-0", memory: "bought a drill press" }] },
    });
    expect(out).toContain("## Recent Context");
    expect(out).toContain("- bought a drill press");
    expect(out).not.toContain("## Core Knowledge");
  });

  it("renders both sections when both populated", () => {
    const out = formatProfile({
      profile: {
        static: [{ id: "profile-0", memory: "s1" }],
        dynamic: [
          { id: "profile-0", memory: "d1" },
          { id: "profile-1", memory: "d2" },
        ],
      },
    });
    expect(out.indexOf("## Core Knowledge")).toBeLessThan(out.indexOf("## Recent Context"));
    expect(out).toContain("- s1");
    expect(out).toContain("- d1");
    expect(out).toContain("- d2");
  });
});

describe("SupermemoryVolume.fetchProfile", () => {
  it("calls client.profile with containerTag and caches the rendered body", async () => {
    const profile = vi.fn().mockResolvedValue({
      profile: {
        static: [{ id: "profile-0", memory: "s" }],
        dynamic: [{ id: "profile-0", memory: "d" }],
      },
    });
    const client = { profile } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag-x");

    const body = await volume.fetchProfile();
    expect(profile).toHaveBeenCalledTimes(1);
    expect(profile).toHaveBeenCalledWith("tag-x");
    expect(body).toContain("- s");
    expect(body).toContain("- d");

    const cached = volume.cache.get(SupermemoryVolume.PROFILE_PATH);
    expect(cached?.content).toBe(body);
    expect(cached?.status).toBe("done");
  });

  it("returns cached body without calling the SDK on second read", async () => {
    const profile = vi
      .fn()
      .mockResolvedValue({ profile: { static: [{ id: "profile-0", memory: "s" }], dynamic: [] } });
    const client = { profile } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag-x");

    const a = await volume.fetchProfile();
    const b = await volume.fetchProfile();
    expect(profile).toHaveBeenCalledTimes(1);
    expect(b).toBe(a);
  });

  it("wraps SDK errors in FsError(EIO)", async () => {
    const profile = vi.fn().mockRejectedValue(new Error("upstream down"));
    const client = { profile } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag-x");

    await expect(volume.fetchProfile()).rejects.toThrowError(FsError);
    await expect(volume.fetchProfile()).rejects.toThrow(/EIO.*upstream down/);
  });
});

describe("SupermemoryVolume.isReservedPath", () => {
  it("returns true for /profile.md", () => {
    const v = new SupermemoryVolume(fakeClient, "tag");
    expect(v.isReservedPath("/profile.md")).toBe(true);
  });

  it("returns false for any other path", () => {
    const v = new SupermemoryVolume(fakeClient, "tag");
    expect(v.isReservedPath("/")).toBe(false);
    expect(v.isReservedPath("/profile")).toBe(false);
    expect(v.isReservedPath("/profile.md/")).toBe(false);
    expect(v.isReservedPath("/notes/profile.md")).toBe(false);
  });
});

describe("SupermemoryVolume.addDoc validation pipeline", () => {
  function makeVolume() {
    const add = vi.fn().mockResolvedValue({ id: "new-id", status: "done" });
    const client = {
      add,
      list: vi.fn().mockResolvedValue(emptyListResp),
      documents: { update: vi.fn(), get: vi.fn(), delete: vi.fn() },
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    return { volume, add };
  }

  it("rejects extensionless path with EINVAL before any wire call", async () => {
    const { volume, add } = makeVolume();
    await expect(volume.addDoc("/memory", "x")).rejects.toThrow(/EINVAL/);
    expect(add).not.toHaveBeenCalled();
  });

  it("rejects /profile.md with EPERM", async () => {
    const { volume, add } = makeVolume();
    await expect(volume.addDoc("/profile.md", "x")).rejects.toThrow(/EPERM/);
    expect(add).not.toHaveBeenCalled();
  });

  it("rejects writing under a known file with ENOTDIR", async () => {
    const { volume, add } = makeVolume();
    volume.pathIndex.insert("/foo.md", "doc-1");
    await expect(volume.addDoc("/foo.md/bar.md", "x")).rejects.toThrow(/ENOTDIR/);
    expect(add).not.toHaveBeenCalled();
  });

  it("rejects writing to a path that has descendants with EISDIR", async () => {
    const { volume, add } = makeVolume();
    volume.pathIndex.insert("/foo.md/bar.md", "doc-1");
    await expect(volume.addDoc("/foo.md", "x")).rejects.toThrow(/EISDIR/);
    expect(add).not.toHaveBeenCalled();
  });

  it("accepts a valid, non-colliding path", async () => {
    const { volume, add } = makeVolume();
    await volume.addDoc("/notes/clean.md", "x");
    expect(add).toHaveBeenCalledTimes(1);
  });
});

describe("SupermemoryVolume.moveDoc validation on dest", () => {
  it("rejects rename to a colliding dest before any wire call", async () => {
    const update = vi.fn().mockResolvedValue({ id: "new-id", status: "done" });
    const list = vi.fn().mockResolvedValue(emptyListResp);
    const client = {
      add: vi.fn(),
      list,
      documents: { update, get: vi.fn(), delete: vi.fn() },
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    volume.pathIndex.insert("/parent.md", "doc-parent");
    volume.pathIndex.insert("/somewhere.md", "doc-other");
    await expect(volume.moveDoc("/somewhere.md", "/parent.md/inner.md")).rejects.toThrow(/ENOTDIR/);
    expect(update).not.toHaveBeenCalled();
  });

  it("rejects rename to /profile.md", async () => {
    const update = vi.fn();
    const list = vi.fn().mockResolvedValue(emptyListResp);
    const client = {
      add: vi.fn(),
      list,
      documents: { update, get: vi.fn(), delete: vi.fn() },
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    volume.pathIndex.insert("/foo.md", "doc-foo");
    await expect(volume.moveDoc("/foo.md", "/profile.md")).rejects.toThrow(/EPERM/);
    expect(update).not.toHaveBeenCalled();
  });
});
