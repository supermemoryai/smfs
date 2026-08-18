import { describe, expect, it } from "vitest";
import { createToolDescription, TOOL_DESCRIPTION } from "../src/tool-description.js";

describe("tool description", () => {
  it("keeps the exported default description writable and documents rg", () => {
    expect(TOOL_DESCRIPTION).toContain("Files you write here persist");
    expect(TOOL_DESCRIPTION).toContain("rg PATTERN PATH");
    expect(TOOL_DESCRIPTION).not.toContain("This filesystem is read-only");
  });

  it("describes the actual read-only command policy", () => {
    const description = createToolDescription({
      readOnly: true,
      commands: ["cat", "grep", "rg"],
    });

    expect(description).toContain("This filesystem is read-only");
    expect(description).toContain("rejected with EPERM");
    expect(description).toContain("Configured built-in commands: cat, grep, rg");
    expect(description).not.toContain("organize it however you want");
  });
});
