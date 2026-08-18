import type { BashExecResult, BashLogger, BashOptions, CommandName, ExecOptions } from "just-bash";
import { Bash } from "just-bash";
import Supermemory from "supermemory";
import { createSgrepCommand } from "./commands/sgrep.js";
import { FsError } from "./errors.js";
import { READ_ONLY_COMMANDS } from "./read-only-policy.js";
import { SupermemoryFs } from "./supermemory-fs.js";
import { createToolDescription } from "./tool-description.js";
import { SupermemoryVolume } from "./volume.js";

type ExecutionLimits = NonNullable<BashOptions["executionLimits"]>;

export interface CreateBashOptions {
  apiKey: string;
  containerTag: string;
  baseURL?: string;
  /** Warm PathIndex at construction. Default true. */
  eagerLoad?: boolean;
  /** Also warm content cache during eager load. Default true. Set false for huge containers. */
  eagerContent?: boolean;
  env?: Record<string, string>;
  executionLimits?: ExecutionLimits;
  logger?: BashLogger;
  /** Restrict the registered just-bash built-ins. sgrep remains available. */
  commands?: readonly CommandName[];
  /**
   * Reject every filesystem mutation. When commands is omitted, a read-oriented
   * built-in allowlist is also applied. Default false.
   */
  readOnly?: boolean;
  /**
   * Content-cache TTL in ms.
   *   undefined → 150_000 (2.5 min, multi-writer default)
   *   null      → never expires (single-writer; only LRU evicts)
   *   0         → no caching
   */
  cacheTtlMs?: number | null;
  /**
   * When true, sgrep output includes a `[doc:<uuid>]` annotation per result
   * so callers can extract the source document IDs from stdout.
   * Default false — off by default to preserve existing output format.
   */
  includeDocIds?: boolean;
}

export interface CreateBashResult {
  bash: Bash;
  volume: SupermemoryVolume;
  toolDescription: string;
  configureMemoryPaths: (paths: string[]) => Promise<void>;
  /** Re-run the eager listing. Useful after another process writes to the container. */
  refresh: () => Promise<void>;
}

export async function createBash(opts: CreateBashOptions): Promise<CreateBashResult> {
  const client = new Supermemory({
    apiKey: opts.apiKey,
    ...(opts.baseURL ? { baseURL: opts.baseURL } : {}),
  });
  const volume = new SupermemoryVolume(client, opts.containerTag, {
    cacheOptions: opts.cacheTtlMs === undefined ? undefined : { ttlMs: opts.cacheTtlMs },
  });
  const fs = new SupermemoryFs(volume, { readOnly: opts.readOnly });

  const doWarm = async () => {
    await volume.listByPrefix("/", { withContent: opts.eagerContent ?? true });
  };
  if (opts.eagerLoad !== false) {
    await doWarm();
  }

  // Empty PATH skips just-bash's `/usr/bin/<cmd>` stat lookup, which our
  // wire-fallback would turn into EIO instead of ENOENT — that breaks
  // customCommand resolution.
  const env: Record<string, string> = { PATH: "", ...(opts.env ?? {}) };

  const commands = opts.commands
    ? [...opts.commands]
    : opts.readOnly
      ? [...READ_ONLY_COMMANDS]
      : undefined;

  const bash = new Bash({
    fs,
    customCommands: [createSgrepCommand({ includeDocIds: opts.includeDocIds })],
    cwd: "/",
    env,
    // just-bash's defense-in-depth patches setTimeout, which the Supermemory SDK uses for retries.
    defenseInDepth: false,
    ...(commands ? { commands } : {}),
    ...(opts.executionLimits ? { executionLimits: opts.executionLimits } : {}),
    ...(opts.logger ? { logger: opts.logger } : {}),
  });

  const origExec = bash.exec.bind(bash);
  bash.exec = async (cmd: string, options?: ExecOptions): Promise<BashExecResult> => {
    try {
      return await origExec(cmd, options);
    } catch (err) {
      if (err instanceof FsError) {
        return {
          stdout: "",
          stderr: `bash: ${err.message}\n`,
          exitCode: 1,
          env: bash.getEnv(),
        };
      }
      throw err;
    }
  };

  return {
    bash,
    volume,
    toolDescription: createToolDescription({
      readOnly: opts.readOnly,
      ...(commands ? { commands } : {}),
    }),
    configureMemoryPaths: (paths: string[]) => volume.configureMemoryPaths(paths),
    refresh: doWarm,
  };
}
