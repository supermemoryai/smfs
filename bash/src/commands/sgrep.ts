import { defineCommand } from "just-bash";
import type { SupermemoryFs } from "../supermemory-fs.js";
import type { SearchResult, SupermemoryVolume } from "../volume.js";

export type SgrepPathMode = "auto" | "file" | "directory";

interface SgrepArgs {
  query: string;
  filepath?: string;
  filepathMode: SgrepPathMode;
  help: boolean;
  lineNumber: boolean;
}

const HELP =
  "Usage: sgrep QUERY [PATH]\n" +
  "       sgrep [-p PATH] QUERY\n" +
  "       sgrep [--file PATH | --dir PATH] QUERY\n" +
  "  Semantic search across the Supermemory container. Output mirrors\n" +
  "  grep: 'filepath:passage' for every matching semantic passage,\n" +
  "  ranked by semantic similarity descending.\n" +
  "\n" +
  "  PATH        Restrict search to a file or directory. Existing paths\n" +
  "              are detected automatically; a trailing '/' is optional\n" +
  "              for directories. Positional or via -p.\n" +
  "  --file PATH Force an exact-file scope when a file and directory share\n" +
  "              the same path.\n" +
  "  --dir PATH  Force a directory-prefix scope.\n" +
  "  -n, --line-number\n" +
  "              Accepted for grep compatibility. Semantic passages do\n" +
  "              not have exact line numbers, so output is unchanged.\n" +
  "  --help      Show this help.\n";

export function parseSgrepArgs(argv: string[]): SgrepArgs | { error: string } {
  const out: SgrepArgs = {
    query: "",
    filepath: undefined,
    filepathMode: "auto",
    help: false,
    lineNumber: false,
  };
  const positional: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === undefined) continue;
    if (a === "--help" || a === "-h") {
      out.help = true;
    } else if (a === "-n" || a === "--line-number") {
      // Recorded for future exact-line sources; passage output remains unchanged.
      out.lineNumber = true;
    } else if (a === "-p" || a === "--file" || a === "--dir") {
      const next = argv[i + 1];
      if (next === undefined) return { error: `sgrep: ${a} requires an argument` };
      if (out.filepath !== undefined) {
        return { error: "sgrep: only one path scope may be provided" };
      }
      out.filepath = next;
      out.filepathMode = a === "--file" ? "file" : a === "--dir" ? "directory" : "auto";
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
function withTrailingSlash(path: string): string {
  return path === "/" || path.endsWith("/") ? path : `${path}/`;
}

type SgrepVolume = Pick<SupermemoryVolume, "pathIndex" | "listByPrefix">;

/** Resolve an optional sgrep scope without assuming every slashless path is a file. */
export async function resolveSgrepFilepath(
  volume: SgrepVolume,
  filepath: string,
  mode: SgrepPathMode,
): Promise<string> {
  if (mode === "file") {
    if (filepath !== "/" && filepath.endsWith("/")) {
      throw new Error(`'${filepath}' ends with '/'; remove it when using --file`);
    }
    return filepath;
  }
  if (mode === "directory" || filepath.endsWith("/")) return withTrailingSlash(filepath);

  const cachedFile = volume.pathIndex.isFile(filepath);
  const cachedDirectory = volume.pathIndex.isDirectory(filepath);
  if (cachedFile && cachedDirectory) {
    throw new Error(
      `'${filepath}' is both a file and a directory; choose --file ${filepath} or --dir ${filepath}`,
    );
  }
  if (cachedFile) return filepath;
  if (cachedDirectory) return withTrailingSlash(filepath);

  const directoryPath = withTrailingSlash(filepath);
  const [exactMatches, descendantMatches] = await Promise.all([
    volume.listByPrefix(filepath, { exact: true, limit: 1 }),
    volume.listByPrefix(directoryPath, { limit: 1 }),
  ]);
  const isFile = exactMatches.length > 0;
  const isDirectory = descendantMatches.length > 0;
  if (isFile && isDirectory) {
    throw new Error(
      `'${filepath}' is both a file and a directory; choose --file ${filepath} or --dir ${filepath}`,
    );
  }
  if (isFile) return filepath;
  if (isDirectory) return directoryPath;
  throw new Error(`path '${filepath}' was not found; use --file or --dir to force a scope`);
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
      const filepath = parsed.filepath
        ? await resolveSgrepFilepath(fs.volume, parsed.filepath, parsed.filepathMode)
        : undefined;
      const resp = await fs.volume.search({
        q: parsed.query,
        ...(filepath ? { filepath } : {}),
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
