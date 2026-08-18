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

  it("uses the real shell parser for quoted pipes in read-only mode", async () => {
    const { bash, toolDescription } = await createBash({
      apiKey: "stub",
      containerTag: "test_b6_readonly_pipe",
      eagerLoad: false,
      readOnly: true,
    });

    const piped = await bash.exec("printf '%s\\n' 'A|B' 'C' | grep -F 'A|B'");
    const available = await bash.exec("type rg; type rm");

    expect(piped).toMatchObject({ exitCode: 0, stdout: "A|B\n" });
    expect(available.stdout).toContain("rg is");
    expect(available.stderr).toContain("rm: not found");
    expect(toolDescription).toContain("read-only");
    expect(toolDescription).toContain("rejected with EPERM");
  });

  it("blocks redirect writes at the filesystem boundary", async () => {
    const { bash, volume } = await createBash({
      apiKey: "stub",
      containerTag: "test_b6_readonly_redirect",
      eagerLoad: false,
      readOnly: true,
    });
    const addDoc = vi.spyOn(volume, "addDoc");

    const result = await bash.exec("echo blocked > /blocked.md");

    expect(result.exitCode).toBe(1);
    expect(result.stderr).toContain("EPERM");
    expect(addDoc).not.toHaveBeenCalled();
  });

  it("honors an explicit typed command allowlist", async () => {
    const { bash } = await createBash({
      apiKey: "stub",
      containerTag: "test_b6_command_allowlist",
      eagerLoad: false,
      readOnly: true,
      commands: ["printf", "grep"],
    });

    expect((await bash.exec("printf ok | grep ok")).stdout).toBe("ok\n");
    expect((await bash.exec("rg --help")).exitCode).toBe(127);
  });
});
