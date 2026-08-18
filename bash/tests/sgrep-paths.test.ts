import { describe, expect, it, vi } from "vitest";
import { resolveSgrepFilepath } from "../src/commands/sgrep.js";
import { PathIndex } from "../src/path-index.js";
import type { DocSummary } from "../src/volume.js";

const doc = (filepath: string): DocSummary => ({
  id: `doc:${filepath}`,
  filepath,
  status: "done",
  size: 0,
  mtime: new Date(0),
});

function makeVolume() {
  return {
    pathIndex: new PathIndex(),
    listByPrefix: vi.fn(async () => [] as DocSummary[]),
  };
}

describe("sgrep filepath resolution", () => {
  it("normalizes a known directory even when the trailing slash is omitted", async () => {
    const volume = makeVolume();
    volume.pathIndex.insert("/trajectories/one.md", "doc-1");

    await expect(resolveSgrepFilepath(volume, "/trajectories", "auto")).resolves.toBe(
      "/trajectories/",
    );
    expect(volume.listByPrefix).not.toHaveBeenCalled();
  });

  it("preserves a known exact file scope", async () => {
    const volume = makeVolume();
    volume.pathIndex.insert("/trajectories", "doc-1");

    await expect(resolveSgrepFilepath(volume, "/trajectories", "auto")).resolves.toBe(
      "/trajectories",
    );
  });

  it("probes a cold index before deciding that a slashless path is a directory", async () => {
    const volume = makeVolume();
    volume.listByPrefix.mockImplementation(async (prefix) =>
      prefix === "/trajectories/" ? [doc("/trajectories/one.md")] : [],
    );

    await expect(resolveSgrepFilepath(volume, "/trajectories", "auto")).resolves.toBe(
      "/trajectories/",
    );
    expect(volume.listByPrefix).toHaveBeenCalledWith("/trajectories", {
      exact: true,
      limit: 1,
    });
    expect(volume.listByPrefix).toHaveBeenCalledWith("/trajectories/", { limit: 1 });
  });

  it("requires an explicit scope when a file and directory share a path", async () => {
    const volume = makeVolume();
    volume.pathIndex.insert("/trajectories", "doc-file");
    volume.pathIndex.insert("/trajectories/one.md", "doc-child");

    await expect(resolveSgrepFilepath(volume, "/trajectories", "auto")).rejects.toThrow(
      "both a file and a directory",
    );
    await expect(resolveSgrepFilepath(volume, "/trajectories", "file")).resolves.toBe(
      "/trajectories",
    );
    await expect(resolveSgrepFilepath(volume, "/trajectories", "directory")).resolves.toBe(
      "/trajectories/",
    );
  });

  it("returns an actionable error for a missing automatic scope", async () => {
    const volume = makeVolume();
    await expect(resolveSgrepFilepath(volume, "/missing", "auto")).rejects.toThrow(
      "use --file or --dir",
    );
  });
});
