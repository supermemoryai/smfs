import { describe, expect, it } from "vitest";
import { formatSgrepOutput, parseSgrepArgs } from "../src/commands/sgrep.js";

describe("sgrep arg parsing", () => {
  it.each([
    [["hello"], { query: "hello", filepath: undefined, help: false }],
    [["hello", "world"], { query: "hello world", filepath: undefined, help: false }],
    [["-n", "hello"], { query: "hello", filepath: undefined, help: false }],
    [["--line-number", "hello"], { query: "hello", filepath: undefined, help: false }],
    [["hello", "-n", "/notes/"], { query: "hello", filepath: "/notes/", help: false }],
    // -p flag, either order
    [["-p", "/notes/", "auth"], { query: "auth", filepath: "/notes/", help: false }],
    [["auth", "-p", "/notes/"], { query: "auth", filepath: "/notes/", help: false }],
    [
      ["auth", "--file", "/trajectories"],
      { query: "auth", filepath: "/trajectories", filepathMode: "file" },
    ],
    [
      ["--dir", "/trajectories", "auth"],
      { query: "auth", filepath: "/trajectories", filepathMode: "directory" },
    ],
    // grep-style positional path (last positional starting with /)
    [["compiler", "/notes/"], { query: "compiler", filepath: "/notes/", help: false }],
    [
      ["plants", "and", "sunlight", "/notes/"],
      { query: "plants and sunlight", filepath: "/notes/", help: false },
    ],
    [["--help"], { help: true }],
  ])("parses %j → %j", (argv, expected) => {
    const got = parseSgrepArgs(argv);
    if ("error" in got) throw new Error(`unexpected error: ${got.error}`);
    expect(got).toMatchObject(expected);
  });

  it.each([
    [[], "missing QUERY"],
    [["-p"], "-p requires"],
    [["--unknown"], "unknown flag"],
    [["auth", "-p", "/one", "--dir", "/two"], "only one path scope"],
  ])("rejects %j with %j", (argv, fragment) => {
    const got = parseSgrepArgs(argv);
    expect("error" in got && got.error.includes(fragment)).toBe(true);
  });
});

describe("sgrep output format (matches smfs grep)", () => {
  it("emits 'filepath:content' one line per match with newlines escaped", () => {
    const out = formatSgrepOutput([
      { id: "1", filepath: "/a.md", chunk: "first line\nsecond line", similarity: 0.9 },
      { id: "2", filepath: "/b.md", chunk: "another", similarity: 0.7 },
    ]);
    expect(out).toBe("/a.md:first line\\nsecond line\n\n/b.md:another\n");
  });

  it("prefers memory over chunk when both present (smfs parity)", () => {
    const out = formatSgrepOutput([
      {
        id: "1",
        filepath: "/a.md",
        memory: "fact about plants",
        chunk: "raw chunk text",
        similarity: 0.8,
      },
    ]);
    expect(out).toBe("/a.md:fact about plants\n");
  });

  it("falls back to chunk when memory is missing", () => {
    const out = formatSgrepOutput([
      { id: "1", filepath: "/a.md", chunk: "raw chunk text", similarity: 0.8 },
    ]);
    expect(out).toBe("/a.md:raw chunk text\n");
  });

  it("escapes \\\\, \\n, \\r in content", () => {
    const out = formatSgrepOutput([
      { id: "1", filepath: "/a.md", memory: "a\nb\rc\\d", similarity: 0.5 },
    ]);
    expect(out).toBe("/a.md:a\\nb\\rc\\\\d\n");
  });

  it("returns empty string when no results", () => {
    expect(formatSgrepOutput([])).toBe("");
  });
});
