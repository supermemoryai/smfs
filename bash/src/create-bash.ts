import type { BashExecResult, BashLogger, BashOptions, ExecOptions } from "just-bash";
import { Bash } from "just-bash";
import Supermemory from "supermemory";
import { createSgrepCommand } from "./commands/sgrep.js";
import { FsError } from "./errors.js";
import { SupermemoryFs } from "./supermemory-fs.js";
import { TOOL_DESCRIPTION } from "./tool-description.js";
import { SupermemoryVolume } from "./volume.js";

type ExecutionLimits = NonNullable<BashOptions["executionLimits"]>;

export interface CreateBashOptions {
  apiKey: string;
  namespace?: string;
  containerTag?: string;
  baseURL?: string;
  /** Warm PathIndex at construction. Default true. */
  eagerLoad?: boolean;
  /** Also warm content cache during eager load. Default true. Set false for huge containers. */
  eagerContent?: boolean;
  env?: Record<string, string>;
  executionLimits?: ExecutionLimits;
  logger?: BashLogger;
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
  if (opts.namespace && opts.containerTag && opts.namespace !== opts.containerTag) {
    throw new Error("namespace and containerTag must identify the same namespace");
  }
  const namespace = opts.namespace ?? opts.containerTag;
  if (!namespace) throw new Error("namespace is required");
  const client = new Supermemory({
    apiKey: opts.apiKey,
    ...(opts.baseURL ? { baseUrl: opts.baseURL } : {}),
  });
  const volume = new SupermemoryVolume(client, namespace, {
    cacheOptions: opts.cacheTtlMs === undefined ? undefined : { ttlMs: opts.cacheTtlMs },
  });
  const fs = new SupermemoryFs(volume);

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

  const bash = new Bash({
    fs,
    customCommands: [createSgrepCommand({ includeDocIds: opts.includeDocIds })],
    cwd: "/",
    env,
    // just-bash's defense-in-depth patches setTimeout, which the Supermemory SDK uses for retries.
    defenseInDepth: false,
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
    toolDescription: TOOL_DESCRIPTION,
    configureMemoryPaths: (paths: string[]) => volume.configureMemoryPaths(paths),
    refresh: doWarm,
  };
}
