import type { CommandName } from "just-bash";

export interface ToolDescriptionOptions {
  readOnly?: boolean;
  commands?: readonly CommandName[];
}

export function createToolDescription(options: ToolDescriptionOptions = {}): string {
  const access = options.readOnly
    ? "This filesystem is read-only. Reads, pipelines, and searches are allowed; writes, appends, redirects to files, mkdir, rm, mv, and cp are rejected with EPERM."
    : "Files you write here persist across sessions and are searchable by the rest of your tooling.";
  const organization = options.readOnly
    ? "Explore the existing filesystem; do not attempt to reorganize it."
    : "The entire filesystem is yours — organize it however you want.";
  const commands = options.commands
    ? `Configured built-in commands: ${options.commands.length > 0 ? options.commands.join(", ") : "(none)"}. The custom sgrep command and shell syntax such as pipes, variables, conditionals, and loops remain available.`
    : "Standard shell commands work as expected: pwd, cd, ls, cat, stat, mkdir, rm, rmdir, mv, cp, echo, grep, rg, head, tail, wc, sort, sed, awk, find, [ -f ], [ -d ], pipes, redirects, variables, conditionals, loops.";
  const consistency = options.readOnly
    ? "Search reflects the container's indexed content. Recently ingested documents may take 5–30 seconds to become searchable; semantic memory extraction may take longer."
    : "Writes return immediately and self-reads see them via the local cache. Other sessions and sgrep see new content after the server finishes ingesting (typically 5–30 seconds for indexing; semantic memory extraction may take longer). If sgrep returns no hits for content you just wrote, wait a few seconds and retry.";

  return `You have access to a bash environment whose filesystem is your Supermemory container. ${access}

Default working directory: \`/\`. ${organization}

\`/profile.md\` is read-only — memories synthesized from your files. Cat it for context.

${commands}

Two grep flavors:

- \`grep PATTERN PATH\` or \`rg PATTERN PATH\` — literal search on a known file or directory. Use when you know the file and want exact text.
- \`sgrep QUERY [PATH]\` — semantic search across every file in your container, ranked by meaning. Use when you don't know which file holds what you're looking for, or the wording isn't an exact match. Output is one match per line, formatted \`filepath:content\`. Existing file and directory scopes are detected automatically, so both \`/trajectories\` and \`/trajectories/\` find that directory. If a file and directory share a path, choose explicitly with \`--file PATH\` or \`--dir PATH\`. The common grep flag \`-n\` is accepted, but semantic passages do not have exact line numbers.

Workflow: use \`sgrep\` to find which files are relevant, then \`cat\`, \`grep\`, or \`rg\` on those files to drill in.

Eventual consistency: ${consistency}

What's not supported: chmod, utimes, symlinks, /dev/null redirects (real device files don't exist here), and large binary uploads. These will throw clear errors.
`;
}

/** Backward-compatible description for the default writable configuration. */
export const TOOL_DESCRIPTION = createToolDescription();
