# @supermemory/bash

A virtual bash environment for AI agents, backed by your [Supermemory](https://supermemory.ai) container. Files persist across sessions, and a built-in `sgrep` command does semantic search across the entire filesystem.

## Contents

- [Install](#install)
- [Quickstart](#quickstart)
- [Hand the bash tool to your LLM](#hand-the-bash-tool-to-your-llm)
- [Options](#options)
- [What's not supported](#whats-not-supported)
- [License](#license)

## Install

```bash
npm install @supermemory/bash
# or
bun add @supermemory/bash
```

You'll need a Supermemory API key. Get one at [supermemory.ai](https://supermemory.ai).

## Quickstart

```typescript
import { createBash } from "@supermemory/bash";

const { bash, toolDescription } = await createBash({
  apiKey: process.env.SUPERMEMORY_API_KEY!,
  containerTag: "user_42",
});

// Run any shell command:
const r = await bash.exec("echo 'hello' > /a.md && cat /a.md");
console.log(r.stdout);  // "hello\n"

// Files persist across sessions, even from a fresh process:
const r2 = await bash.exec("cat /a.md");
console.log(r2.stdout);  // "hello\n"

// Semantic search across the whole container:
const r3 = await bash.exec("sgrep 'authentication tokens'");
console.log(r3.stdout);
// /work/auth.md:OAuth implementation handles token refresh and session management.
// /notes/security.md:Two-factor authentication via TOTP is required for admin accounts.
```

`sgrep` detects whether an existing path is a file or directory, so `sgrep "query" /trajectories` and `sgrep "query" /trajectories/` both search that directory. If a file and directory intentionally share the same path, use `--file /path` or `--dir /path` to disambiguate. The grep-compatible `-n` flag is accepted, but semantic passages do not have exact line numbers.

## Hand the bash tool to your LLM

`createBash` returns a `toolDescription` field. It's the package's opinionated description of the bash tool (sgrep guidance, persistence semantics, eventual-consistency notes, what's not supported), shipped so the agent doesn't have to discover any of it on its own. Drop it into the `description` field of your tool schema.

The same string is also exported as the named constant `TOOL_DESCRIPTION` if you'd rather import it directly (`import { TOOL_DESCRIPTION } from "@supermemory/bash"`). Either form works. Examples below use the destructured field for consistency.

The agent gets:

- All standard shell commands: `cat`, `ls`, `mkdir`, `rm`, `mv`, `cp`, `grep`, `head`, `tail`, `wc`, `sed`, `awk`, pipes, redirects, conditionals, loops.
- A custom `sgrep` command for semantic search across every file in the container.
- Files persist: writes are durable, reads work across sessions.

### Vercel AI SDK

```typescript
import { generateText, tool } from "ai";
import { openai } from "@ai-sdk/openai";
import { z } from "zod";
import { createBash } from "@supermemory/bash";

const { bash, toolDescription } = await createBash({
  apiKey: process.env.SUPERMEMORY_API_KEY!,
  containerTag: "user_42",
});

const result = await generateText({
  model: openai("gpt-4o"),
  prompt: "Search my notes for authentication.",
  tools: {
    bash: tool({
      description: toolDescription,
      inputSchema: z.object({ cmd: z.string() }),
      execute: async ({ cmd }) => bash.exec(cmd),
    }),
  },
  maxSteps: 8,
});
```

### Anthropic tool-use

```typescript
import Anthropic from "@anthropic-ai/sdk";
import { createBash } from "@supermemory/bash";

const { bash, toolDescription } = await createBash({
  apiKey: process.env.SUPERMEMORY_API_KEY!,
  containerTag: "user_42",
});

const client = new Anthropic({ apiKey: process.env.ANTHROPIC_API_KEY! });
const response = await client.messages.create({
  model: "claude-sonnet-4-6",
  max_tokens: 4096,
  tools: [{
    name: "bash",
    description: toolDescription,
    input_schema: { type: "object", properties: { cmd: { type: "string" } }, required: ["cmd"] },
  }],
  messages: [{ role: "user", content: "Find my notes about authentication and summarize." }],
});

// In your tool-use loop, call bash.exec(cmd) and feed the result back.
```

## Options

```typescript
createBash({
  apiKey: string,
  containerTag: string,         // one container per user / project
  baseURL?: string,             // SDK override
  eagerLoad?: boolean,          // default: true (warm pathIndex at construction)
  eagerContent?: boolean,       // default: true (also warm content cache)
  cacheTtlMs?: number | null,   // default: 150_000 (2.5 min). null = never expires (single-writer). 0 = no cache.
  cwd?: string,                 // default: "/home/user"
  env?: Record<string, string>,
  executionLimits?: ExecutionLimits,  // pass-through to just-bash
  logger?: BashLogger,                // pass-through to just-bash
});
```

For very large containers (10k+ docs), set `eagerContent: false` to skip the content warm and pay HTTP per `cat`. Path resolution stays warm.

`cacheTtlMs` controls how long the in-memory content cache trusts itself. The default (2.5 min) assumes other writers exist (other agent sessions, dashboard uploads, webhooks). Single-writer apps can pass `null` for max speed.

## What's not supported

- `chmod`, `utimes`, symlinks (`ln -s`, `readlink`). Supermemory has no permission or symlink model; these throw `ENOSYS`.
- `/dev/null` redirects. `/dev/null` exists as a directory marker but isn't a writable target. Use `2>/tmp/discard.log` if you need to discard output.
- Truly binary uploads. Content gets text-extracted server-side; raw binary write is not supported in this version.

## License

MIT
