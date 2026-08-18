import { defineCommand } from "just-bash";
import type { SupermemoryFs } from "../supermemory-fs.js";
import type { SearchResult } from "../volume.js";

interface SgrepArgs {
  query: string;
  filepath?: string;
  help: boolean;
  lineNumber: boolean;
}

const HELP =
  "Usage: sgrep QUERY [PATH]\n" +
  "       sgrep [-p PATH] QUERY\n" +
  "  Semantic search across the Supermemory container. Output mirrors\n" +
  "  grep: 'filepath:passage' for every matching semantic passage,\n" +
  "  ranked by semantic similarity descending.\n" +
  "\n" +
  "  PATH        Restrict to files whose path equals PATH (or starts\n" +
  "              with PATH if it ends with '/'). Positional or via -p.\n" +
  "  -n, --line-number\n" +
  "              Accepted for grep compatibility. Semantic passages do\n" +
  "              not have exact line numbers, so output is unchanged.\n" +
  "  --help      Show this help.\n";

export function parseSgrepArgs(argv: string[]): SgrepArgs | { error: string } {
  const out: SgrepArgs = { query: "", filepath: undefined, help: false, lineNumber: false };
  const positional: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === undefined) continue;
    if (a === "--help" || a === "-h") {
      out.help = true;
    } else if (a === "-n" || a === "--line-number") {
      // Recorded for future exact-line sources; passage output remains unchanged.
      out.lineNumber = true;
    } else if (a === "-p") {
      const next = argv[i + 1];
      if (next === undefined) return { error: "sgrep: -p requires an argument" };
      out.filepath = next;
      i++;
    } else if (a.startsWith("-")) {
      return { error: `sgrep: unknown flag '${a}'` };
    } else {
      positional.push(a);
    }
  }
  if (out.help) return out;
  if (positional.length === 0) return { error: "sgrep: missing QUERY (try --help)" };

  // `sgrep "term" /notes/` — last absolute-path positional is the scope.
  if (out.filepath === undefined && positional.length >= 2) {
    const last = positional[positional.length - 1];
    if (last?.startsWith("/")) {
      out.filepath = last;
      positional.pop();
    }
  }
  out.query = positional.join(" ");
  return out;
}

function escapeForOneLine(s: string): string {
  return s.replace(/\\/g, "\\\\").replace(/\r/g, "\\r").replace(/\n/g, "\\n");
}

export function formatSgrepOutput(results: SearchResult[], includeDocIds = false): string {
  if (results.length === 0) return "";
  const lines: string[] = [];
  for (const r of results) {
    const fp = r.filepath ?? "(unknown)";
    let content = "";
    if (typeof r.memory === "string" && r.memory.length > 0) content = r.memory;
    else if (typeof r.chunk === "string") content = r.chunk;
    if (content.length === 0) continue;
    const prefix = includeDocIds && r.id ? `${fp} [doc:${r.id}]` : fp;
    lines.push(`${prefix}:${escapeForOneLine(content)}`);
  }
  return lines.length === 0 ? "" : `${lines.join("\n\n")}\n`;
}

export interface SgrepCommandOptions {
  includeDocIds?: boolean;
}

export function createSgrepCommand(opts: SgrepCommandOptions = {}) {
  return defineCommand("sgrep", async (argv, ctx) => {
    const parsed = parseSgrepArgs(argv);
    if ("error" in parsed) {
      return { stdout: "", stderr: `${parsed.error}\n`, exitCode: 2 };
    }
    if (parsed.help) {
      return { stdout: HELP, stderr: "", exitCode: 0 };
    }

    const fs = ctx.fs as Partial<SupermemoryFs>;
    if (!fs.volume) {
      return {
        stdout: "",
        stderr: "sgrep: not a SupermemoryFs (missing volume reference)\n",
        exitCode: 1,
      };
    }

    try {
      const resp = await fs.volume.search({
        q: parsed.query,
        ...(parsed.filepath ? { filepath: parsed.filepath } : {}),
      });
      return {
        stdout: formatSgrepOutput(resp.results, opts.includeDocIds),
        stderr: "",
        exitCode: 0,
      };
    } catch (err) {
      return {
        stdout: "",
        stderr: `sgrep: ${(err as Error).message}\n`,
        exitCode: 1,
      };
    }
  });
}

export const sgrepCommand = createSgrepCommand();
