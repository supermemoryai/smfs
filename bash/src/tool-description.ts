export const TOOL_DESCRIPTION = `You have access to a bash environment whose filesystem is your Supermemory container. Files you write here persist across sessions and are searchable by the rest of your tooling.

Default working directory: \`/\`. The entire filesystem is yours — organize it however you want.

\`/profile.md\` is read-only — memories synthesized from your files. Cat it for context.

Standard shell commands work as expected: pwd, cd, ls, cat, stat, mkdir, rm, rmdir, mv, cp, echo, grep, head, tail, wc, sort, sed, awk, find, [ -f ], [ -d ], pipes, redirects, variables, conditionals, loops. You can read, write, append, move, copy, and delete files freely.

Two grep flavors:

- \`grep PATTERN PATH\` — literal substring search on a known file or directory. Use when you know the file and want exact text.
- \`sgrep QUERY [PATH]\` — semantic search across every file in your container, ranked by meaning. Use when you don't know which file holds what you're looking for, or the wording isn't an exact match. Output is one match per line, formatted \`filepath:content\`. Trailing slash on PATH narrows to a directory; otherwise it's an exact-path match.

Workflow: use \`sgrep\` to find which files are relevant, then \`cat\` or \`grep\` on those files to drill in.

Eventual consistency: writes return immediately and self-reads see them via the local cache. Other sessions and \`sgrep\` see new content after the server finishes ingesting (typically 5–30 seconds for indexing; semantic memory extraction may take longer). If \`sgrep\` returns no hits for content you just wrote, wait a few seconds and retry.

What's not supported: chmod, utimes, symlinks, /dev/null redirects (real device files don't exist here), and large binary uploads. These will throw clear errors.
`;
