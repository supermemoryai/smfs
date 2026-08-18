import { describe, expect, it, vi } from "vitest";
import { createBash } from "../src/create-bash.js";

// We can't easily mock the Supermemory SDK constructor (it's imported as
// a default class), so these tests run against a stub apiKey and rely on
// the eagerLoad:false path to avoid any HTTP. The eagerLoad:true path is
// exercised against production in .scratch/validate-b6.ts.

describe("createBash factory", () => {
  it("eagerLoad:false skips the initial listByPrefix (no HTTP at construction)", async () => {
    // Spy on the global fetch so we'd see any wire activity.
    const fetchSpy = vi.spyOn(globalThis, "fetch").mockImplementation(() => {
      throw new Error("createBash should not have called fetch with eagerLoad:false");
    });
    const { bash, volume } = await createBash({
      apiKey: "stub",
      containerTag: "test_b6_stub",
      eagerLoad: false,
    });
    expect(bash).toBeDefined();
    expect(volume).toBeDefined();
    expect(fetchSpy).not.toHaveBeenCalled();
    fetchSpy.mockRestore();
  });

  it("does not pre-populate any synthetic dirs at boot", async () => {
    const { volume } = await createBash({
      apiKey: "stub",
      containerTag: "test_b6_layout",
      eagerLoad: false,
    });
    expect(volume.pathIndex.syntheticDirPaths()).toEqual([]);
  });

  it("registers sgrep as a custom command (sgrep --help works)", async () => {
    const { bash } = await createBash({
      apiKey: "stub",
      containerTag: "test_b6_sgrep",
      eagerLoad: false,
    });
    const r = await bash.exec("sgrep --help");
    expect(r.exitCode).toBe(0);
    expect(r.stdout).toContain("Usage");
    expect(r.stdout).toContain("sgrep");
  });

  it("refresh() calls listByPrefix again", async () => {
    // Use eagerLoad:false to avoid initial HTTP, then spy on volume.listByPrefix
    // to confirm refresh invokes it.
    const { volume, refresh } = await createBash({
      apiKey: "stub",
      containerTag: "test_b6_refresh",
      eagerLoad: false,
    });
    const spy = vi.spyOn(volume, "listByPrefix").mockResolvedValue([]);
    await refresh();
    expect(spy).toHaveBeenCalledTimes(1);
    expect(spy.mock.calls[0]?.[0]).toBe("/");
    spy.mockRestore();
  });
  it("treats a known slashless sgrep scope as a directory", async () => {
    const { bash, volume } = await createBash({
      apiKey: "stub",
      containerTag: "test_b6_sgrep_directory",
      eagerLoad: false,
    });
    volume.pathIndex.insert("/trajectories/one.md", "doc-1");
    const search = vi.spyOn(volume, "search").mockResolvedValue({ results: [] });

    const result = await bash.exec("sgrep authentication /trajectories");

    expect(result.exitCode).toBe(0);
    expect(search).toHaveBeenCalledWith({ q: "authentication", filepath: "/trajectories/" });
  });
});
