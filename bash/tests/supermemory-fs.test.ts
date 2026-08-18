import type Supermemory from "supermemory";
import { describe, expect, it, vi } from "vitest";
import { FsError } from "../src/errors.js";
import { SupermemoryFs } from "../src/supermemory-fs.js";
import { SupermemoryVolume } from "../src/volume.js";

const emptyListResp = {
  memories: [],
  pagination: { currentPage: 1, totalPages: 1, totalItems: 0 },
};

function makeFs(opts: { listResp?: unknown; getResp?: unknown; readOnly?: boolean } = {}) {
  const list = vi.fn().mockResolvedValue(opts.listResp ?? emptyListResp);
  const get = vi.fn().mockResolvedValue(opts.getResp ?? null);
  const deleteBulk = vi.fn().mockResolvedValue({ deletedCount: 1, success: true });
  const add = vi.fn().mockResolvedValue({ id: "new-id", status: "done" });
  const update = vi.fn().mockResolvedValue({ id: "new-id", status: "done" });
  const del = vi.fn().mockResolvedValue(undefined);
  const client = {
    documents: { add, update, get, delete: del, deleteBulk, list },
  } as unknown as Supermemory;
  const volume = new SupermemoryVolume(client, "tag");
  const fs = new SupermemoryFs(volume, { readOnly: opts.readOnly });
  return { fs, volume, list, get, deleteBulk, add, update, del };
}

describe("SupermemoryFs.resolvePath (pure normalization)", () => {
  it.each([
    ["/", "a", "/a"],
    ["/x", "../y", "/y"],
    ["/", ".", "/"],
    ["/x", "/abs", "/abs"],
    ["/", "a/b/../c", "/a/c"],
    ["/", "", "/"],
    ["/x/y", ".", "/x/y"],
    ["/", "a/", "/a"],
  ])("resolvePath(%j, %j) → %j", (base, p, expected) => {
    const { fs } = makeFs();
    expect(fs.resolvePath(base, p)).toBe(expected);
  });
});

describe("SupermemoryFs.readdir grouping", () => {
  it("groups paths under a prefix into unique entries; sorts; classifies file vs dir", async () => {
    const memories = [
      { id: "1", filepath: "/notes/a.md", status: "done", updatedAt: "2026-01-01" },
      { id: "2", filepath: "/notes/sub/b.md", status: "done", updatedAt: "2026-01-01" },
      { id: "3", filepath: "/notes/sub/c.md", status: "done", updatedAt: "2026-01-01" },
    ];
    const { fs } = makeFs({
      listResp: {
        memories,
        pagination: { currentPage: 1, totalPages: 1, totalItems: 3 },
      },
    });
    const entries = await fs.readdirWithFileTypes("/notes");
    expect(entries.map((e) => e.name)).toEqual(["a.md", "sub"]);
    expect(entries[0]).toMatchObject({ name: "a.md", isFile: true, isDirectory: false });
    expect(entries[1]).toMatchObject({ name: "sub", isFile: false, isDirectory: true });
  });
});

describe("SupermemoryFs.readFile error mapping", () => {
  it("throws FsError with code ENOENT when getDoc returns null and not a directory", async () => {
    const { fs } = makeFs(); // empty list, null get
    await expect(fs.readFile("/never.md")).rejects.toMatchObject({ code: "ENOENT" });
    await expect(fs.readFile("/never.md")).rejects.toBeInstanceOf(FsError);
  });
});

describe("SupermemoryFs.stat mapping", () => {
  it("maps a doc to FsStat with mode 0o644 / size / mtime; throws ENOENT on missing", async () => {
    const memories = [
      {
        id: "doc-x",
        filepath: "/a.md",
        status: "done",
        updatedAt: "2026-01-15T10:00:00.000Z",
        content: "hello",
      },
    ];
    const { fs, volume } = makeFs({
      listResp: {
        memories,
        pagination: { currentPage: 1, totalPages: 1, totalItems: 1 },
      },
      getResp: {
        id: "doc-x",
        content: "hello",
        status: "done",
        updatedAt: "2026-01-15T10:00:00.000Z",
      },
    });
    volume.pathIndex.insert("/a.md", "doc-x");
    const s = await fs.stat("/a.md");
    expect(s.isFile).toBe(true);
    expect(s.isDirectory).toBe(false);
    expect(s.isSymbolicLink).toBe(false);
    expect(s.mode).toBe(0o644);
    expect(s.size).toBe(5);

    await expect(fs.stat("/never.md")).rejects.toMatchObject({ code: "ENOENT" });
  });
});

describe("SupermemoryFs read-only mode", () => {
  it("reports read-only file and directory modes", async () => {
    const { fs, volume } = makeFs({
      readOnly: true,
      listResp: {
        memories: [
          {
            id: "doc-x",
            filepath: "/a.md",
            status: "done",
            updatedAt: "2026-01-15T10:00:00.000Z",
            content: "hello",
          },
        ],
        pagination: { currentPage: 1, totalPages: 1, totalItems: 1 },
      },
    });
    volume.pathIndex.insert("/a.md", "doc-x");
    volume.markSyntheticDir("/notes");

    expect((await fs.stat("/a.md")).mode).toBe(0o444);
    expect((await fs.stat("/notes")).mode).toBe(0o555);
  });

  it("rejects every mutation before calling the Supermemory API", async () => {
    const { fs, volume, list, get, deleteBulk, add, update, del } = makeFs({ readOnly: true });
    volume.pathIndex.insert("/existing.md", "doc-existing");
    volume.markSyntheticDir("/existing-dir");

    const mutations = [
      () => fs.writeFile("/new.md", "value"),
      () => fs.appendFile("/existing.md", "value"),
      () => fs.mkdir("/new-dir"),
      () => fs.rm("/existing.md"),
      () => fs.rmdir("/existing-dir"),
      () => fs.mv("/existing.md", "/moved.md"),
      () => fs.cp("/existing.md", "/copied.md"),
    ];

    for (const mutate of mutations) {
      await expect(mutate()).rejects.toMatchObject({ code: "EPERM" });
    }
    expect(add).not.toHaveBeenCalled();
    expect(deleteBulk).not.toHaveBeenCalled();
    expect(list).not.toHaveBeenCalled();
    expect(get).not.toHaveBeenCalled();
    expect(update).not.toHaveBeenCalled();
    expect(del).not.toHaveBeenCalled();
    expect(volume.pathIndex.resolve("/existing.md")).toBe("doc-existing");
  });
});

describe("SupermemoryFs write-path branch decisions", () => {
  it("writeFile on a directory throws EISDIR", async () => {
    const { fs, volume } = makeFs();
    volume.markSyntheticDir("/somedir");
    await expect(fs.writeFile("/somedir", "x")).rejects.toMatchObject({ code: "EISDIR" });
  });

  it("rm on directory without recursive throws EISDIR; with recursive uses bulk delete", async () => {
    const { fs, volume } = makeFs({
      listResp: {
        memories: [{ id: "1", filepath: "/sub/a.md", status: "done", updatedAt: "2026-01-01" }],
        pagination: { currentPage: 1, totalPages: 1, totalItems: 1 },
      },
    });
    volume.pathIndex.insert("/sub/a.md", "1");
    await expect(fs.rm("/sub")).rejects.toMatchObject({ code: "EISDIR" });
    // recursive succeeds (mocked deleteBulk returns 1)
    await fs.rm("/sub", { recursive: true });
    expect(volume.pathIndex.resolve("/sub/a.md")).toBeNull();
  });

  it("rmdir on non-empty throws ENOTEMPTY", async () => {
    const { fs, volume } = makeFs({
      listResp: {
        memories: [{ id: "1", filepath: "/sub/a.md", status: "done", updatedAt: "2026-01-01" }],
        pagination: { currentPage: 1, totalPages: 1, totalItems: 1 },
      },
    });
    volume.pathIndex.insert("/sub/a.md", "1");
    await expect(fs.rmdir("/sub")).rejects.toMatchObject({ code: "ENOTEMPTY" });
  });

  it("mkdir(recursive) marks every ancestor as synthetic", async () => {
    const { fs, volume } = makeFs();
    await fs.mkdir("/a/b/c", { recursive: true });
    expect(volume.pathIndex.isDirectory("/a")).toBe(true);
    expect(volume.pathIndex.isDirectory("/a/b")).toBe(true);
    expect(volume.pathIndex.isDirectory("/a/b/c")).toBe(true);
  });

  it("appendFile on missing path creates it (POSIX >> semantics)", async () => {
    const add = vi.fn().mockResolvedValue({ id: "id1", status: "done" });
    const list = vi.fn().mockResolvedValue(emptyListResp);
    const get = vi.fn().mockResolvedValue(null);
    const client = {
      documents: { add, update: vi.fn(), get, delete: vi.fn(), deleteBulk: vi.fn(), list },
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    const fs = new SupermemoryFs(volume);
    await fs.appendFile("/new.md", "tail");
    expect(add).toHaveBeenCalledTimes(1);
    expect(add.mock.calls[0]?.[0]).toMatchObject({ content: "tail", filepath: "/new.md" });
  });
});

describe("SupermemoryFs reserved file /profile.md", () => {
  function makeFsWithProfile(profileBody: { static: string[]; dynamic: string[] }) {
    const list = vi.fn().mockResolvedValue(emptyListResp);
    const profile = vi.fn().mockResolvedValue({ profile: profileBody });
    const add = vi.fn().mockResolvedValue({ id: "snap-1", status: "done" });
    const client = {
      documents: { add, update: vi.fn(), get: vi.fn(), delete: vi.fn(), deleteBulk: vi.fn(), list },
      profile,
    } as unknown as Supermemory;
    const volume = new SupermemoryVolume(client, "tag");
    const fs = new SupermemoryFs(volume);
    return { fs, volume, profile, add };
  }

  it("readFile('/profile.md') returns the rendered profile body", async () => {
    const { fs, profile } = makeFsWithProfile({ static: ["s1"], dynamic: ["d1"] });
    const body = await fs.readFile("/profile.md");
    expect(body).toContain("# Memory Profile");
    expect(body).toContain("- s1");
    expect(body).toContain("- d1");
    expect(profile).toHaveBeenCalledTimes(1);
  });

  it("readFileBuffer('/profile.md') returns same body as bytes", async () => {
    const { fs } = makeFsWithProfile({ static: ["s1"], dynamic: [] });
    const buf = await fs.readFileBuffer("/profile.md");
    const decoded = new TextDecoder().decode(buf);
    expect(decoded).toContain("- s1");
  });

  it("stat('/profile.md') returns file mode 0o444", async () => {
    const { fs } = makeFsWithProfile({ static: [], dynamic: [] });
    const s = await fs.stat("/profile.md");
    expect(s.isFile).toBe(true);
    expect(s.isDirectory).toBe(false);
    expect(s.mode).toBe(0o444);
  });

  it("stat('/profile.md') size reflects cached body after first read", async () => {
    const { fs } = makeFsWithProfile({ static: ["a"], dynamic: [] });
    expect((await fs.stat("/profile.md")).size).toBe(0);
    const body = await fs.readFile("/profile.md");
    expect((await fs.stat("/profile.md")).size).toBe(new TextEncoder().encode(body).length);
  });

  it("readdir('/') always lists profile.md, even on an empty container", async () => {
    const { fs } = makeFsWithProfile({ static: [], dynamic: [] });
    const entries = await fs.readdirWithFileTypes("/");
    const profileEntry = entries.find((e) => e.name === "profile.md");
    expect(profileEntry).toBeDefined();
    expect(profileEntry?.isFile).toBe(true);
    expect(profileEntry?.isDirectory).toBe(false);
  });

  it("getAllPaths includes /profile.md", () => {
    const { fs } = makeFsWithProfile({ static: [], dynamic: [] });
    expect(fs.getAllPaths()).toContain("/profile.md");
  });

  it("writeFile('/profile.md') throws EPERM", async () => {
    const { fs } = makeFsWithProfile({ static: [], dynamic: [] });
    await expect(fs.writeFile("/profile.md", "x")).rejects.toThrowError(FsError);
    await expect(fs.writeFile("/profile.md", "x")).rejects.toThrow(/EPERM/);
  });

  it("appendFile('/profile.md') throws EPERM", async () => {
    const { fs } = makeFsWithProfile({ static: [], dynamic: [] });
    await expect(fs.appendFile("/profile.md", "x")).rejects.toThrow(/EPERM/);
  });

  it("rm('/profile.md') throws EPERM", async () => {
    const { fs } = makeFsWithProfile({ static: [], dynamic: [] });
    await expect(fs.rm("/profile.md")).rejects.toThrow(/EPERM/);
  });

  it("mkdir('/profile.md') throws EPERM", async () => {
    const { fs } = makeFsWithProfile({ static: [], dynamic: [] });
    await expect(fs.mkdir("/profile.md")).rejects.toThrow(/EPERM/);
  });

  it("mv with src=/profile.md throws EPERM", async () => {
    const { fs } = makeFsWithProfile({ static: [], dynamic: [] });
    await expect(fs.mv("/profile.md", "/foo.md")).rejects.toThrow(/EPERM/);
  });

  it("mv with dest=/profile.md throws EPERM", async () => {
    const { fs } = makeFsWithProfile({ static: [], dynamic: [] });
    await expect(fs.mv("/foo.md", "/profile.md")).rejects.toThrow(/EPERM/);
  });

  it("cp with dest=/profile.md throws EPERM", async () => {
    const { fs } = makeFsWithProfile({ static: [], dynamic: [] });
    await expect(fs.cp("/foo.md", "/profile.md")).rejects.toThrow(/EPERM/);
  });

  it("cp /profile.md /snap.md writes the rendered body to a normal doc", async () => {
    const { fs, add } = makeFsWithProfile({ static: ["s1"], dynamic: [] });
    await fs.cp("/profile.md", "/snap.md");
    expect(add).toHaveBeenCalledTimes(1);
    const args = add.mock.calls[0]?.[0] as { content: string; filepath: string };
    expect(args.filepath).toBe("/snap.md");
    expect(args.content).toContain("- s1");
    expect(args.content).toContain("# Memory Profile");
  });

  it("subpath profile.md is NOT reserved (user can create /memory/profile.md)", async () => {
    const { fs, add } = makeFsWithProfile({ static: [], dynamic: [] });
    await fs.writeFile("/memory/profile.md", "user-owned profile");
    expect(add).toHaveBeenCalledTimes(1);
    expect(add.mock.calls[0]?.[0]).toMatchObject({
      content: "user-owned profile",
      filepath: "/memory/profile.md",
    });
  });
});

describe("SupermemoryFs nested-synthetic-dir handling (v6 PR review)", () => {
  it("rm -r evicts nested synthetic dirs under the prefix", async () => {
    const { fs, volume } = makeFs();
    await fs.mkdir("/a/b/c", { recursive: true });
    expect(volume.pathIndex.isDirectory("/a/b")).toBe(true);
    await fs.rm("/a", { recursive: true });
    expect(volume.pathIndex.isDirectory("/a")).toBe(false);
    expect(volume.pathIndex.isDirectory("/a/b")).toBe(false);
    expect(volume.pathIndex.isDirectory("/a/b/c")).toBe(false);
  });

  it("rmdir refuses to remove a dir that has only synthetic children", async () => {
    const { fs, volume } = makeFs();
    await fs.mkdir("/a/b", { recursive: true });
    await expect(fs.rmdir("/a")).rejects.toMatchObject({ code: "ENOTEMPTY" });
    expect(volume.pathIndex.isDirectory("/a")).toBe(true);
    expect(volume.pathIndex.isDirectory("/a/b")).toBe(true);
  });

  it("mv of a tree migrates synthetic subdirs to dest", async () => {
    const memories = [
      { id: "f1", filepath: "/src/file.md", status: "done", updatedAt: "2026-01-01" },
    ];
    const { fs, volume } = makeFs({
      listResp: { memories, pagination: { currentPage: 1, totalPages: 1, totalItems: 1 } },
    });
    volume.pathIndex.insert("/src/file.md", "f1");
    await fs.mkdir("/src/empty", { recursive: true });
    await fs.mv("/src", "/dst");
    expect(volume.pathIndex.isDirectory("/dst/empty")).toBe(true);
    expect(volume.pathIndex.isDirectory("/src/empty")).toBe(false);
  });

  it("cp -r of a tree replicates synthetic subdirs at dest", async () => {
    const memories = [
      {
        id: "f1",
        filepath: "/src/file.md",
        status: "done",
        updatedAt: "2026-01-01",
        content: "hi",
      },
    ];
    const { fs, volume } = makeFs({
      listResp: { memories, pagination: { currentPage: 1, totalPages: 1, totalItems: 1 } },
    });
    volume.pathIndex.insert("/src/file.md", "f1");
    await fs.mkdir("/src/empty", { recursive: true });
    await fs.cp("/src", "/dst", { recursive: true });
    expect(volume.pathIndex.isDirectory("/dst/empty")).toBe(true);
    expect(volume.pathIndex.isDirectory("/src/empty")).toBe(true);
  });
});
