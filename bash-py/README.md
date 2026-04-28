# supermemory-bash

A virtual bash environment for AI agents, backed by your [Supermemory](https://supermemory.ai) container. Files persist across sessions, and a built-in `sgrep` command does semantic search across the entire filesystem.

## Contents

- [Install](#install)
- [Quickstart](#quickstart)
- [Hand the bash tool to your LLM](#hand-the-bash-tool-to-your-llm)
- [Options](#options)
- [What's not supported](#whats-not-supported)
- [License](#license)

## Install

```sh
pip install supermemory-bash
# or
uv add supermemory-bash
```

You'll need a Supermemory API key. Get one at [supermemory.ai](https://supermemory.ai).

## Quickstart

```python
import asyncio
from supermemory_bash import create_bash

async def main():
    result = await create_bash(
        api_key="sm-...",
        container_tag="user_42",
    )
    bash = result.bash

    # Run any shell command:
    r = await bash.exec("echo 'hello' > /a.md && cat /a.md")
    print(r.stdout)  # "hello\n"

    # Files persist across sessions, even from a fresh process:
    r2 = await bash.exec("cat /a.md")
    print(r2.stdout)  # "hello\n"

    # Semantic search across the whole container:
    r3 = await bash.exec("sgrep 'authentication tokens'")
    print(r3.stdout)
    # /work/auth.md:OAuth implementation handles token refresh and session management.
    # /notes/security.md:Two-factor authentication via TOTP is required for admin accounts.

asyncio.run(main())
```

## Hand the bash tool to your LLM

`create_bash` returns a `tool_description` field. It's the package's opinionated description of the bash tool (sgrep guidance, persistence semantics, eventual-consistency notes, what's not supported), shipped so the agent doesn't have to discover any of it on its own. Drop it into the `description` field of your tool schema.

The same string is also exported as the named constant `TOOL_DESCRIPTION` if you'd rather import it directly (`from supermemory_bash import TOOL_DESCRIPTION`). Either form works. Examples below use the result field for consistency.

The agent gets:

- All standard shell commands: `cat`, `ls`, `mkdir`, `rm`, `mv`, `cp`, `grep`, `head`, `tail`, `wc`, `sed`, pipes, redirects.
- A custom `sgrep` command for semantic search across every file in the container.
- A read-only `/profile.md` virtual file with memories synthesized from the container's content.
- Files persist: writes are durable, reads work across sessions.

### OpenAI

```python
from openai import AsyncOpenAI
from supermemory_bash import create_bash

result = await create_bash(api_key="sm-...", container_tag="user_42")

client = AsyncOpenAI()
response = await client.chat.completions.create(
    model="gpt-5.5",
    messages=[{"role": "user", "content": "Search my notes for authentication."}],
    tools=[{
        "type": "function",
        "function": {
            "name": "bash",
            "description": result.tool_description,
            "parameters": {
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "required": ["cmd"],
            },
        },
    }],
)

# In your tool-use loop, call `await result.bash.exec(cmd)` and feed the result back.
```

### Anthropic

```python
from anthropic import AsyncAnthropic
from supermemory_bash import create_bash

result = await create_bash(api_key="sm-...", container_tag="user_42")

client = AsyncAnthropic()
response = await client.messages.create(
    model="claude-sonnet-4-6",
    max_tokens=4096,
    tools=[{
        "name": "bash",
        "description": result.tool_description,
        "input_schema": {
            "type": "object",
            "properties": {"cmd": {"type": "string"}},
            "required": ["cmd"],
        },
    }],
    messages=[{"role": "user", "content": "Find my notes about authentication and summarize."}],
)

# In your tool-use loop, call `await result.bash.exec(cmd)` and feed the result back.
```

## Options

```python
await create_bash(
    api_key="sm-...",
    container_tag="user_42",        # one container per user / project
    base_url=None,                  # API override
    eager_load=True,                # default: True (warm path_index at construction)
    eager_content=True,             # default: True (also warm content cache)
    cache_ttl_ms=150_000,           # default: 150_000 (2.5 min). None = never expires (single-writer). 0 = no cache.
    cwd="/",                        # default working directory
    env=None,                       # extra environment variables
)
```

For very large containers (10k+ docs), set `eager_content=False` to skip the content warm and pay HTTP per `cat`. Path resolution stays warm.

`cache_ttl_ms` controls how long the in-memory content cache trusts itself. The default (2.5 min) assumes other writers exist (other agent sessions, dashboard uploads, webhooks). Single-writer apps can pass `None` for max speed.

## What's not supported

- `chmod`, `utimes`, symlinks (`ln -s`, `readlink`). Supermemory has no permission or symlink model; these throw `ENOSYS`.
- `/dev/null` redirects. `/dev/null` exists as a directory marker but isn't a writable target. Use `2>/tmp/discard.log` if you need to discard output.
- Truly binary uploads. Content gets text-extracted server-side; raw binary write is not supported in this version.

## License

MIT
