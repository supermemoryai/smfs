import type Supermemory from "supermemory";
import type { DocumentListResponse } from "supermemory/resources/documents";
import type { SearchMemoriesParams } from "supermemory/resources/search";
import type { ProfileResponse } from "supermemory/resources/top-level";
import { ebusy, eexist, efbig, eio, enoent } from "./errors.js";
import { PathIndex } from "./path-index.js";
import { SessionCache, type SessionCacheOptions } from "./session-cache.js";
import { assertWritable } from "./validation/pipeline.js";

// SDK's Memory type omits `filepath` and the per-status error fields.
type MemoryWithPath = DocumentListResponse.Memory & {
  filepath?: string;
  errorMessage?: string;
  errorReason?: string;
  error?: string;
  failureReason?: string;
};

function normalizeStatus(s: string): DocStatus {
  if (s === "done") return "done";
  if (s === "failed") return "failed";
  return "processing";
}

export type DocStatus = "done" | "failed" | "processing";

export interface DocResult {
  id: string;
  content: string | Uint8Array;
  status: DocStatus;
  errorReason?: string;
  virtual?: boolean;
}

export interface DocSummary {
  id: string;
  filepath: string;
  status: DocStatus;
  size: number;
  mtime: Date;
  content?: string;
}

export interface DocStat {
  id?: string;
  isFile: boolean;
  isDirectory: boolean;
  size: number;
  mtime: Date;
  status?: DocStatus;
}

export interface ListByPrefixOpts {
  withContent?: boolean;
  exact?: boolean;
  limit?: number;
}

export interface SearchResult {
  id: string;
  filepath?: string;
  memory?: string;
  chunk?: string;
  similarity: number;
}

export interface SearchResp {
  results: SearchResult[];
  total?: number;
  timing?: number;
}

export interface RemoveByPrefixResult {
  deleted: number;
  errors: Error[];
}

export interface SearchParams {
  q: string;
  filepath?: string;
}

export interface SupermemoryVolumeOptions {
  pathIndex?: PathIndex;
  cache?: SessionCache;
  cacheOptions?: SessionCacheOptions;
}

export class SupermemoryVolume {
  readonly client: Supermemory;
  readonly containerTag: string;
  readonly pathIndex: PathIndex;
  readonly cache: SessionCache;
  private allPathsCache: { paths: string[]; at: number } | null = null;
  private lastConfiguredPaths: string | null = null;
  private static readonly ALL_PATHS_TTL_MS = 60_000;
  private static readonly ALL_PATHS_HARD_CAP = 5000;
  static readonly PROFILE_PATH = "/profile.md";

  constructor(client: Supermemory, containerTag: string, options: SupermemoryVolumeOptions = {}) {
    this.client = client;
    this.containerTag = containerTag;
    this.pathIndex = options.pathIndex ?? new PathIndex();
    this.cache = options.cache ?? new SessionCache(options.cacheOptions);
  }

  private async *iterContainer(
    opts: { filepath?: string; includeContent?: boolean } = {},
  ): AsyncIterable<MemoryWithPath> {
    let page = 1;
    const includeContent = opts.includeContent ?? false;
    while (true) {
      const params: Record<string, unknown> = {
        containerTags: [this.containerTag],
        limit: 100,
        page,
        includeContent,
      };
      if (opts.filepath !== undefined) params.filepath = opts.filepath;
      const resp = await this.client.documents.list(
        params as Parameters<typeof this.client.documents.list>[0],
      );
      for (const m of resp.memories ?? []) yield m as MemoryWithPath;
      const total = resp.pagination?.totalPages ?? 1;
      if (page >= total) break;
      page++;
    }
  }

  private async lookupDocId(path: string): Promise<string | null> {
    const cached = this.pathIndex.resolve(path);
    if (cached) return cached;
    try {
      const resp = await this.client.documents.list({
        containerTags: [this.containerTag],
        limit: 1,
        page: 1,
        // @ts-expect-error filepath not in DocumentListParams typing yet (wire accepts it; exact match without trailing slash)
        filepath: path,
      });
      const m = resp.memories?.[0] as MemoryWithPath | undefined;
      if (!m) return null;
      if (m.filepath === path) {
        this.pathIndex.insert(path, m.id);
        return m.id;
      }
      return null;
    } catch (err) {
      throw eio(`lookupDocId(${path}): ${(err as Error).message}`);
    }
  }

  private filterArgFor(prefix: string, exact: boolean): string | undefined {
    if (prefix === "") return undefined;
    if (exact) return prefix;
    return prefix.endsWith("/") ? prefix : `${prefix}/`;
  }

  async addDoc(
    path: string,
    content: string | Uint8Array,
  ): Promise<{ id: string; status: DocStatus }> {
    if (content instanceof Uint8Array) {
      throw efbig(path);
    }
    assertWritable({ path, intent: "addDoc", pathIndex: this.pathIndex });

    const existing = this.pathIndex.resolve(path);
    let id: string;
    let serverStatus: string;

    try {
      if (existing) {
        const resp = await this.client.documents.update(existing, {
          content,
          containerTag: this.containerTag,
          // @ts-expect-error filepath not in DocumentUpdateParams typing yet
          filepath: path,
        });
        id = resp.id ?? existing;
        serverStatus = resp.status ?? "unknown";
      } else {
        const resp = await this.client.documents.add({
          content,
          containerTag: this.containerTag,
          // @ts-expect-error filepath not in DocumentAddParams typing yet
          filepath: path,
        });
        id = resp.id;
        serverStatus = resp.status;
      }
    } catch (err) {
      throw eio(`addDoc(${path}): ${(err as Error).message}`);
    }

    const status = normalizeStatus(serverStatus);
    this.pathIndex.insert(path, id);
    this.cache.set(path, content, status);
    return { id, status };
  }

  async updateDoc(
    path: string,
    content: string | Uint8Array,
  ): Promise<{ id: string; status: DocStatus }> {
    if (!(await this.lookupDocId(path))) {
      throw enoent(path);
    }
    return this.addDoc(path, content);
  }

  async getDoc(path: string): Promise<DocResult | null> {
    const cachedFast = this.cache.get(path);
    if (cachedFast) {
      const docId = this.pathIndex.resolve(path);
      if (docId) return { id: docId, content: cachedFast.content, status: cachedFast.status };
    }

    let resp: { memories?: MemoryWithPath[] };
    try {
      resp = await this.client.documents.list({
        containerTags: [this.containerTag],
        limit: 1,
        page: 1,
        includeContent: true,
        // @ts-expect-error filepath not in DocumentListParams typing yet (wire accepts it)
        filepath: path,
      });
    } catch (err) {
      throw eio(`getDoc(${path}): ${(err as Error).message}`);
    }

    const m = resp.memories?.[0];
    // Server may echo a non-matching filepath on stale listings — treat as not-found.
    if (!m || (m.filepath !== undefined && m.filepath !== path)) {
      this.pathIndex.remove(path);
      this.cache.delete(path);
      return null;
    }
    const status = normalizeStatus(m.status ?? "unknown");
    const rawContent = m.content ?? "";

    let content: string = rawContent;
    let errorReason: string | undefined;
    if (status === "failed") {
      errorReason = m.errorMessage || m.errorReason || m.error || m.failureReason || "(unknown)";
      content = `[supermemory.error: processing-failed]\n\nThis document could not be processed.\nReason: ${errorReason}`;
    }

    if (m.id) this.pathIndex.insert(path, m.id);
    this.cache.set(path, content, status);
    return errorReason ? { id: m.id, content, status, errorReason } : { id: m.id, content, status };
  }

  async removeDoc(path: string): Promise<void> {
    const docId = await this.lookupDocId(path);
    if (!docId) return;

    try {
      await this.client.documents.delete(docId);
    } catch (err) {
      const status = (err as { status?: number }).status;
      if (status === 409) throw ebusy(path);
      if (status === 404) {
        this.pathIndex.remove(path);
        this.cache.delete(path);
        return;
      }
      throw eio(`removeDoc(${path}): ${(err as Error).message}`);
    }

    this.pathIndex.remove(path);
    this.cache.delete(path);
  }

  async removeByPrefix(prefix: string): Promise<RemoveByPrefixResult> {
    const filterArg = this.filterArgFor(prefix, false);
    if (filterArg === undefined) {
      // Empty prefix would also wipe filepath-NULL docs; iterate so we only touch the ones we model.
      return this.removeByPrefixViaList(prefix);
    }

    let deleted = 0;
    const errors: Error[] = [];
    try {
      const resp = await this.client.documents.deleteBulk({
        containerTags: [this.containerTag],
        // @ts-expect-error filepath not in DocumentDeleteBulkParams typing yet (wire accepts it)
        filepath: filterArg,
      });
      deleted = resp.deletedCount ?? 0;
      for (const e of resp.errors ?? []) {
        errors.push(new Error(`${e.id}: ${e.error}`));
      }
    } catch (err) {
      errors.push(new Error(`removeByPrefix(${prefix}): ${(err as Error).message}`));
      return { deleted, errors };
    }

    for (const p of this.pathIndex.paths()) {
      if (p.startsWith(prefix)) {
        this.pathIndex.remove(p);
        this.cache.delete(p);
      }
    }
    this.evictSyntheticUnder(prefix);

    return { deleted, errors };
  }

  private evictSyntheticUnder(prefix: string): void {
    const dirSelf = prefix.endsWith("/") ? prefix.slice(0, -1) : prefix;
    for (const d of this.pathIndex.syntheticDirPaths()) {
      if (d === dirSelf || d.startsWith(prefix)) {
        this.pathIndex.removeSyntheticDir(d);
      }
    }
  }

  private async removeByPrefixViaList(prefix: string): Promise<RemoveByPrefixResult> {
    const matches: Array<{ id: string; filepath: string }> = [];
    for await (const m of this.iterContainer({ includeContent: false })) {
      if (typeof m.filepath === "string" && m.filepath.startsWith(prefix)) {
        matches.push({ id: m.id, filepath: m.filepath });
      }
    }
    if (matches.length === 0) return { deleted: 0, errors: [] };
    let deleted = 0;
    const errors: Error[] = [];
    for (let i = 0; i < matches.length; i += 100) {
      const batch = matches.slice(i, i + 100);
      try {
        const resp = await this.client.documents.deleteBulk({ ids: batch.map((m) => m.id) });
        deleted += resp.deletedCount ?? 0;
        for (const e of resp.errors ?? []) {
          errors.push(new Error(`${e.id}: ${e.error}`));
        }
      } catch (err) {
        const msg = (err as Error).message;
        for (const m of batch) errors.push(new Error(`${m.id}: ${msg}`));
      }
    }
    const erredIds = new Set<string>();
    for (const e of errors) {
      const id = e.message.split(":")[0]?.trim();
      if (id) erredIds.add(id);
    }
    for (const m of matches) {
      if (!erredIds.has(m.id)) {
        this.pathIndex.remove(m.filepath);
        this.cache.delete(m.filepath);
      }
    }
    if (errors.length === 0) this.evictSyntheticUnder(prefix);
    return { deleted, errors };
  }

  async moveDoc(from: string, to: string): Promise<void> {
    assertWritable({ path: to, intent: "moveDoc", pathIndex: this.pathIndex });
    const docId = await this.lookupDocId(from);
    if (!docId) throw enoent(from);
    if (await this.lookupDocId(to)) throw eexist(to);

    // PATCH with filepath only (no content) renames; PATCH with content silently ignores filepath.
    try {
      await this.client.documents.update(docId, {
        containerTag: this.containerTag,
        // @ts-expect-error filepath not in DocumentUpdateParams typing yet
        filepath: to,
      });
    } catch (err) {
      const status = (err as { status?: number }).status;
      if (status === 404) {
        this.pathIndex.remove(from);
        this.cache.delete(from);
        throw enoent(from);
      }
      if (status === 409) throw ebusy(from);
      throw eio(`moveDoc(${from} → ${to}): ${(err as Error).message}`);
    }

    const cached = this.cache.get(from);
    this.pathIndex.remove(from);
    this.pathIndex.insert(to, docId);
    if (cached) {
      this.cache.set(to, cached.content, cached.status);
      this.cache.delete(from);
    }
  }

  async listByPrefix(prefix: string, opts: ListByPrefixOpts = {}): Promise<DocSummary[]> {
    const out: DocSummary[] = [];
    const limit = opts.limit ?? Infinity;
    const filterArg = this.filterArgFor(prefix, opts.exact ?? false);
    for await (const m of this.iterContainer({
      filepath: filterArg,
      includeContent: opts.withContent ?? false,
    })) {
      if (typeof m.filepath !== "string") continue;
      const matches = opts.exact ? m.filepath === prefix : m.filepath.startsWith(prefix);
      if (!matches) continue;
      const status = normalizeStatus(m.status ?? "unknown");
      const content = typeof m.content === "string" ? m.content : undefined;
      const summary: DocSummary = {
        id: m.id,
        filepath: m.filepath,
        status,
        size: content?.length ?? 0,
        mtime: m.updatedAt ? new Date(m.updatedAt) : new Date(0),
        ...(content !== undefined ? { content } : {}),
      };
      out.push(summary);
      this.pathIndex.insert(m.filepath, m.id);
      if (opts.withContent && content !== undefined) {
        this.cache.set(m.filepath, content, status);
      }
      if (out.length >= limit) break;
    }
    return out;
  }

  async listAllPaths(): Promise<string[]> {
    const paths: string[] = [];
    for await (const m of this.iterContainer({ includeContent: false })) {
      if (typeof m.filepath !== "string") continue;
      paths.push(m.filepath);
      this.pathIndex.insert(m.filepath, m.id);
      if (paths.length > SupermemoryVolume.ALL_PATHS_HARD_CAP) {
        throw eio(
          `listAllPaths exceeded ${SupermemoryVolume.ALL_PATHS_HARD_CAP} docs in container '${this.containerTag}'`,
        );
      }
    }
    paths.sort();
    this.allPathsCache = { paths, at: Date.now() };
    return paths;
  }

  cachedAllPaths(): string[] {
    if (!this.allPathsCache) return [];
    if (Date.now() - this.allPathsCache.at > SupermemoryVolume.ALL_PATHS_TTL_MS) return [];
    return this.allPathsCache.paths;
  }

  async statDoc(path: string): Promise<DocStat | null> {
    if (this.pathIndex.isDirectory(path) && !this.pathIndex.isFile(path)) {
      return { isFile: false, isDirectory: true, size: 0, mtime: new Date(0) };
    }

    const cached = this.cache.get(path);
    if (cached) {
      const docId = this.pathIndex.resolve(path);
      if (docId) {
        return {
          id: docId,
          isFile: true,
          isDirectory: false,
          size:
            typeof cached.content === "string" ? cached.content.length : cached.content.byteLength,
          mtime: new Date(0),
          status: cached.status,
        };
      }
    }

    let resp: { memories?: MemoryWithPath[] };
    try {
      resp = await this.client.documents.list({
        containerTags: [this.containerTag],
        limit: 1,
        page: 1,
        includeContent: true,
        // @ts-expect-error filepath not in DocumentListParams typing yet (wire accepts it)
        filepath: path,
      });
    } catch (err) {
      throw eio(`statDoc(${path}): ${(err as Error).message}`);
    }

    const m = resp.memories?.[0];
    if (!m || (m.filepath !== undefined && m.filepath !== path)) {
      this.pathIndex.remove(path);
      this.cache.delete(path);
      return null;
    }
    const status = normalizeStatus(m.status ?? "unknown");
    const rawContent = m.content ?? "";
    if (m.id) this.pathIndex.insert(path, m.id);
    this.cache.set(path, rawContent, status);
    return {
      id: m.id,
      isFile: true,
      isDirectory: false,
      size: rawContent.length,
      mtime: m.updatedAt ? new Date(m.updatedAt) : new Date(0),
      status,
    };
  }

  markSyntheticDir(path: string): void {
    this.pathIndex.markSyntheticDir(path);
  }

  async isDirEmpty(path: string): Promise<boolean> {
    const prefix = path === "/" ? "/" : `${path}/`;
    const probe = await this.listByPrefix(prefix, { limit: 1 });
    if (probe.length > 0) return false;
    for (const d of this.pathIndex.syntheticDirPaths()) {
      if (d !== path && d.startsWith(prefix)) return false;
    }
    return true;
  }

  async moveTree(src: string, dest: string): Promise<{ errors: Error[] }> {
    const srcPrefix = src.endsWith("/") ? src : `${src}/`;
    const destPrefix = dest.endsWith("/") ? dest : `${dest}/`;
    const entries = await this.listByPrefix(srcPrefix);
    const errors: Error[] = [];
    const concurrency = 4;
    for (let i = 0; i < entries.length; i += concurrency) {
      const batch = entries.slice(i, i + concurrency);
      await Promise.all(
        batch.map(async (e) => {
          const newPath = destPrefix + e.filepath.slice(srcPrefix.length);
          try {
            await this.moveDoc(e.filepath, newPath);
          } catch (err) {
            errors.push(err as Error);
          }
        }),
      );
    }
    for (const d of this.pathIndex.syntheticDirPaths()) {
      if (d === src) {
        this.pathIndex.removeSyntheticDir(d);
      } else if (d.startsWith(srcPrefix)) {
        this.pathIndex.removeSyntheticDir(d);
        this.pathIndex.markSyntheticDir(destPrefix + d.slice(srcPrefix.length));
      }
    }
    this.pathIndex.markSyntheticDir(dest);
    return { errors };
  }

  async copyTree(src: string, dest: string): Promise<{ errors: Error[] }> {
    const srcPrefix = src.endsWith("/") ? src : `${src}/`;
    const destPrefix = dest.endsWith("/") ? dest : `${dest}/`;
    const entries = await this.listByPrefix(srcPrefix, { withContent: true });
    const errors: Error[] = [];
    const concurrency = 4;
    for (let i = 0; i < entries.length; i += concurrency) {
      const batch = entries.slice(i, i + concurrency);
      await Promise.all(
        batch.map(async (e) => {
          const newPath = destPrefix + e.filepath.slice(srcPrefix.length);
          try {
            await this.addDoc(newPath, e.content ?? "");
          } catch (err) {
            errors.push(err as Error);
          }
        }),
      );
    }
    for (const d of this.pathIndex.syntheticDirPaths()) {
      if (d.startsWith(srcPrefix)) {
        this.pathIndex.markSyntheticDir(destPrefix + d.slice(srcPrefix.length));
      }
    }
    this.pathIndex.markSyntheticDir(dest);
    return { errors };
  }

  async search(params: SearchParams): Promise<SearchResp> {
    let resp: { results?: unknown[] };
    try {
      const body: SearchMemoriesParams & { filepath?: string } = {
        q: params.q,
        containerTag: this.containerTag,
        searchMode: "hybrid",
        include: { documents: true },
      };
      if (params.filepath !== undefined) body.filepath = params.filepath;
      resp = await this.client.search.memories(body);
    } catch (err) {
      throw eio(`search(${params.q}): ${(err as Error).message}`);
    }

    const out: SearchResult[] = [];
    for (const r of resp.results ?? []) {
      const rec = r as {
        id: string;
        memory?: string;
        chunk?: string;
        similarity?: number;
        filepath?: string | null;
        documents?: Array<{ id?: string; documentId?: string }>;
      };
      const docId = rec.documents?.[0]?.id ?? rec.documents?.[0]?.documentId ?? rec.id;
      // Old containers return a null filepath; reverse-lookup via PathIndex covers that.
      const filepath =
        (typeof rec.filepath === "string" ? rec.filepath : undefined) ??
        (docId ? (this.pathIndex.findPath(docId) ?? undefined) : undefined);
      if (params.filepath) {
        const wantsPrefix = params.filepath.endsWith("/");
        if (!filepath) continue;
        if (wantsPrefix) {
          if (!filepath.startsWith(params.filepath)) continue;
        } else if (filepath !== params.filepath) {
          continue;
        }
      }
      out.push({
        id: docId,
        filepath,
        ...(rec.memory ? { memory: rec.memory } : {}),
        ...(rec.chunk ? { chunk: rec.chunk } : {}),
        similarity: rec.similarity ?? 0,
      });
    }
    return { results: out };
  }

  async fetchProfile(): Promise<string> {
    const cached = this.cache.get(SupermemoryVolume.PROFILE_PATH);
    if (cached && typeof cached.content === "string") return cached.content;

    let resp: ProfileResponse;
    try {
      resp = await this.client.profile({ containerTag: this.containerTag });
    } catch (err) {
      throw eio(`profile: ${(err as Error).message}`);
    }
    const body = formatProfile(resp);
    this.cache.set(SupermemoryVolume.PROFILE_PATH, body, "done");
    return body;
  }

  isReservedPath(path: string): boolean {
    return path === SupermemoryVolume.PROFILE_PATH;
  }

  async configureMemoryPaths(paths: string[]): Promise<void> {
    const key = JSON.stringify(paths);
    if (this.lastConfiguredPaths === key) return;

    try {
      await this.client.patch(`/v3/container-tags/${encodeURIComponent(this.containerTag)}`, {
        body: { memoryFilesystemPaths: paths },
      });
    } catch (err) {
      throw eio(`configureMemoryPaths: ${(err as Error).message}`);
    }
    this.lastConfiguredPaths = key;
  }
}

const PROFILE_HEADER = `# Memory Profile

This file is auto-generated from your memories. To change what appears
here, modify the source files in your folder.
`;

export function formatProfile(resp: ProfileResponse): string {
  const staticItems = resp.profile?.static ?? [];
  const dynamicItems = resp.profile?.dynamic ?? [];
  if (staticItems.length === 0 && dynamicItems.length === 0) {
    return `${PROFILE_HEADER}\n(no memories extracted yet — write some files and check back in a few minutes)\n`;
  }
  const parts: string[] = [PROFILE_HEADER];
  if (staticItems.length > 0) {
    parts.push("\n## Core Knowledge\n");
    for (const item of staticItems) parts.push(`- ${item}\n`);
  }
  if (dynamicItems.length > 0) {
    parts.push("\n## Recent Context\n");
    for (const item of dynamicItems) parts.push(`- ${item}\n`);
  }
  return parts.join("");
}
