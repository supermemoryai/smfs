import type {
  BufferEncoding,
  CpOptions,
  FileContent,
  FsStat,
  IFileSystem,
  MkdirOptions,
  RmOptions,
} from "just-bash";
import {
  eexist,
  eio,
  eisdir,
  enoent,
  enosys,
  enotdir,
  enotempty,
  eperm,
  FsError,
} from "./errors.js";
import { SupermemoryVolume } from "./volume.js";

// Mirrored from just-bash/fs/interface.ts — not re-exported there.
interface ReadFileOptions {
  encoding?: BufferEncoding | null;
}
interface WriteFileOptions {
  encoding?: BufferEncoding;
}
interface DirentEntry {
  name: string;
  isFile: boolean;
  isDirectory: boolean;
  isSymbolicLink: boolean;
}

function normalizePath(input: string): string {
  if (!input || input === "/") return "/";
  const segments = input.split("/").filter((s) => s !== "" && s !== ".");
  const stack: string[] = [];
  for (const seg of segments) {
    if (seg === "..") stack.pop();
    else stack.push(seg);
  }
  return `/${stack.join("/")}`;
}

export interface SupermemoryFsOptions {
  /** Reject every filesystem mutation while keeping reads and search available. */
  readOnly?: boolean;
}

export class SupermemoryFs implements IFileSystem {
  readonly readOnly: boolean;

  constructor(
    public readonly volume: SupermemoryVolume,
    options: SupermemoryFsOptions = {},
  ) {
    this.readOnly = options.readOnly ?? false;
  }

  private assertMutable(path: string, operation: string): void {
    if (this.readOnly) throw eperm(normalizePath(path), operation);
  }

  resolvePath(base: string, path: string): string {
    const absolute = path.startsWith("/") ? path : `${base.replace(/\/$/, "")}/${path}`;
    return normalizePath(absolute);
  }

  async realpath(path: string): Promise<string> {
    const norm = normalizePath(path);
    await this.stat(norm); // throws ENOENT if missing
    return norm;
  }

  async exists(path: string): Promise<boolean> {
    try {
      await this.stat(path);
      return true;
    } catch {
      return false;
    }
  }

  async stat(path: string): Promise<FsStat> {
    const norm = normalizePath(path);
    if (this.volume.isReservedPath(norm)) {
      const cached = this.volume.cache.get(norm);
      const size =
        cached && typeof cached.content === "string"
          ? new TextEncoder().encode(cached.content).length
          : 0;
      return {
        isFile: true,
        isDirectory: false,
        isSymbolicLink: false,
        mode: 0o444,
        size,
        mtime: new Date(),
      };
    }
    const docStat = await this.volume.statDoc(norm);
    if (!docStat) throw enoent(norm);
    return {
      isFile: docStat.isFile,
      isDirectory: docStat.isDirectory,
      isSymbolicLink: false,
      mode: docStat.isDirectory ? (this.readOnly ? 0o555 : 0o755) : this.readOnly ? 0o444 : 0o644,
      size: docStat.size,
      mtime: docStat.mtime,
    };
  }

  async lstat(path: string): Promise<FsStat> {
    return this.stat(path);
  }

  async readFile(path: string, _options?: ReadFileOptions | BufferEncoding): Promise<string> {
    const norm = normalizePath(path);
    if (this.volume.isReservedPath(norm)) return this.volume.fetchProfile();
    if (this.volume.pathIndex.isDirectory(norm) && !this.volume.pathIndex.isFile(norm)) {
      throw eisdir(norm);
    }
    const doc = await this.volume.getDoc(norm);
    if (!doc) throw enoent(norm);
    return typeof doc.content === "string" ? doc.content : new TextDecoder().decode(doc.content);
  }

  async readFileBuffer(path: string): Promise<Uint8Array> {
    const norm = normalizePath(path);
    if (this.volume.isReservedPath(norm)) {
      const body = await this.volume.fetchProfile();
      return new TextEncoder().encode(body);
    }
    if (this.volume.pathIndex.isDirectory(norm) && !this.volume.pathIndex.isFile(norm)) {
      throw eisdir(norm);
    }
    const doc = await this.volume.getDoc(norm);
    if (!doc) throw enoent(norm);
    return typeof doc.content === "string" ? new TextEncoder().encode(doc.content) : doc.content;
  }

  async readdir(path: string): Promise<string[]> {
    const entries = await this.readdirWithFileTypes(path);
    return entries.map((e) => e.name);
  }

  async readdirWithFileTypes(path: string): Promise<DirentEntry[]> {
    const norm = normalizePath(path);
    if (this.volume.pathIndex.isFile(norm)) throw enotdir(norm);

    const prefix = norm === "/" ? "/" : `${norm}/`;
    const summaries = await this.volume.listByPrefix(prefix);

    const isKnownDir = norm === "/" || this.volume.pathIndex.isDirectory(norm);
    if (summaries.length === 0 && !isKnownDir) throw enoent(norm);

    const entries = new Map<string, { isFile: boolean; isDirectory: boolean }>();
    for (const s of summaries) {
      const rest = s.filepath.slice(prefix.length);
      if (!rest) continue;
      const slash = rest.indexOf("/");
      const name = slash === -1 ? rest : rest.slice(0, slash);
      const isFile = slash === -1;
      const existing = entries.get(name);
      if (!existing) {
        entries.set(name, { isFile, isDirectory: !isFile });
      } else if (!isFile) {
        existing.isDirectory = true;
        existing.isFile = false;
      }
    }
    // Without this, `ls /` on a fresh container (synthetic dirs only) is empty.
    for (const synth of this.volume.pathIndex.syntheticDirPaths()) {
      if (!synth.startsWith(prefix)) continue;
      const rest = synth.slice(prefix.length);
      if (!rest || rest.includes("/")) continue;
      if (!entries.has(rest)) {
        entries.set(rest, { isFile: false, isDirectory: true });
      }
    }
    if (norm === "/") {
      const reservedName = SupermemoryVolume.PROFILE_PATH.slice(1);
      if (!entries.has(reservedName)) {
        entries.set(reservedName, { isFile: true, isDirectory: false });
      }
    }

    return [...entries.entries()]
      .map(([name, kind]) => ({
        name,
        isFile: kind.isFile,
        isDirectory: kind.isDirectory,
        isSymbolicLink: false,
      }))
      .sort((a, b) => a.name.localeCompare(b.name));
  }

  async writeFile(
    path: string,
    content: FileContent,
    _options?: WriteFileOptions | BufferEncoding,
  ): Promise<void> {
    const norm = normalizePath(path);
    this.assertMutable(norm, "write");
    if (this.volume.isReservedPath(norm)) throw eperm(norm, "write");
    if (this.volume.pathIndex.isDirectory(norm) && !this.volume.pathIndex.isFile(norm)) {
      throw eisdir(norm);
    }
    const data =
      typeof content === "string" ? content : new TextDecoder().decode(content as Uint8Array);
    await this.volume.addDoc(norm, data);
  }

  async appendFile(
    path: string,
    content: FileContent,
    _options?: WriteFileOptions | BufferEncoding,
  ): Promise<void> {
    const norm = normalizePath(path);
    this.assertMutable(norm, "append");
    if (this.volume.isReservedPath(norm)) throw eperm(norm, "write");
    if (this.volume.pathIndex.isDirectory(norm) && !this.volume.pathIndex.isFile(norm)) {
      throw eisdir(norm);
    }
    const existing = await this.volume.getDoc(norm);
    let head = "";
    if (existing) {
      head =
        typeof existing.content === "string"
          ? existing.content
          : new TextDecoder().decode(existing.content);
    }
    const tail =
      typeof content === "string" ? content : new TextDecoder().decode(content as Uint8Array);
    await this.volume.addDoc(norm, head + tail);
  }

  async mkdir(path: string, options?: MkdirOptions): Promise<void> {
    const norm = normalizePath(path);
    this.assertMutable(norm, "mkdir");
    if (this.volume.isReservedPath(norm)) throw eperm(norm, "mkdir");
    if (this.volume.pathIndex.isFile(norm)) throw enotdir(norm);
    if (this.volume.pathIndex.isDirectory(norm) && !options?.recursive) {
      throw eexist(norm);
    }
    if (options?.recursive) {
      const segments = norm.split("/").filter(Boolean);
      let cur = "";
      for (const seg of segments) {
        cur += `/${seg}`;
        this.volume.markSyntheticDir(cur);
      }
    } else {
      this.volume.markSyntheticDir(norm);
    }
  }

  async rm(path: string, options?: RmOptions): Promise<void> {
    const norm = normalizePath(path);
    this.assertMutable(norm, "rm");
    if (this.volume.isReservedPath(norm)) throw eperm(norm, "unlink");
    const isDir = this.volume.pathIndex.isDirectory(norm) && !this.volume.pathIndex.isFile(norm);
    if (isDir) {
      if (!options?.recursive) throw eisdir(norm);
      const prefix = norm.endsWith("/") ? norm : `${norm}/`;
      const result = await this.volume.removeByPrefix(prefix);
      if (result.errors.length > 0 && !options.force) {
        throw eio(`rm(${norm}): ${result.errors.length} subpath(s) failed to delete`);
      }
      return;
    }
    try {
      const docId = this.volume.pathIndex.resolve(norm);
      if (!docId) {
        if (options?.force) return;
        throw enoent(norm);
      }
      await this.volume.removeDoc(norm);
    } catch (err) {
      if (options?.force && err instanceof FsError && err.code === "ENOENT") return;
      throw err;
    }
  }

  async rmdir(path: string, _options?: RmOptions): Promise<void> {
    const norm = normalizePath(path);
    this.assertMutable(norm, "rmdir");
    if (this.volume.pathIndex.isFile(norm)) throw enotdir(norm);
    if (!this.volume.pathIndex.isDirectory(norm)) throw enoent(norm);
    if (!(await this.volume.isDirEmpty(norm))) throw enotempty(norm);
    this.volume.pathIndex.removeSyntheticDir(norm);
  }

  async mv(src: string, dest: string): Promise<void> {
    const srcN = normalizePath(src);
    const destN = normalizePath(dest);
    this.assertMutable(srcN, "rename");
    this.assertMutable(destN, "rename");
    if (this.volume.isReservedPath(srcN)) throw eperm(srcN, "rename");
    if (this.volume.isReservedPath(destN)) throw eperm(destN, "rename");
    const isDir = this.volume.pathIndex.isDirectory(srcN) && !this.volume.pathIndex.isFile(srcN);
    if (isDir) {
      if (!this.volume.pathIndex.isDirectory(srcN)) throw enoent(srcN);
      const result = await this.volume.moveTree(srcN, destN);
      if (result.errors.length > 0) {
        throw eio(`mv(${srcN} → ${destN}): ${result.errors.length} subpath(s) failed`);
      }
      return;
    }
    await this.volume.moveDoc(srcN, destN);
  }

  async cp(src: string, dest: string, options?: CpOptions): Promise<void> {
    const srcN = normalizePath(src);
    const destN = normalizePath(dest);
    this.assertMutable(destN, "copy");
    if (this.volume.isReservedPath(destN)) throw eperm(destN, "write");
    if (this.volume.isReservedPath(srcN)) {
      const body = await this.volume.fetchProfile();
      await this.volume.addDoc(destN, body);
      return;
    }
    const isDir = this.volume.pathIndex.isDirectory(srcN) && !this.volume.pathIndex.isFile(srcN);
    if (isDir) {
      if (!options?.recursive) throw eisdir(srcN);
      const result = await this.volume.copyTree(srcN, destN);
      if (result.errors.length > 0) {
        throw eio(`cp(${srcN} → ${destN}): ${result.errors.length} subpath(s) failed`);
      }
      return;
    }
    const doc = await this.volume.getDoc(srcN);
    if (!doc) throw enoent(srcN);
    await this.volume.addDoc(destN, doc.content);
  }

  async chmod(_path: string, _mode: number): Promise<void> {
    throw enosys("chmod");
  }

  async utimes(_path: string, _atime: Date, _mtime: Date): Promise<void> {
    throw enosys("utimes");
  }

  async symlink(_target: string, _linkPath: string): Promise<void> {
    throw enosys("symlink");
  }

  async link(_existingPath: string, _newPath: string): Promise<void> {
    throw enosys("link");
  }

  async readlink(_path: string): Promise<string> {
    throw enosys("readlink");
  }

  /** Sync inventory used by just-bash for ls and glob expansion. */
  getAllPaths(): string[] {
    const paths = new Set<string>();
    // Root must be present so `ls /` resolves (matches InMemoryFs).
    paths.add("/");
    paths.add(SupermemoryVolume.PROFILE_PATH);
    for (const p of this.volume.cachedAllPaths()) {
      paths.add(p);
      const segments = p.split("/").filter(Boolean);
      let cur = "";
      for (let i = 0; i < segments.length - 1; i++) {
        cur += `/${segments[i]}`;
        paths.add(cur);
      }
    }
    for (const d of this.volume.pathIndex.syntheticDirPaths()) paths.add(d);
    return [...paths].sort();
  }
}
