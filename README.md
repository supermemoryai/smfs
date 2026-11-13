<img width="728" height="288" alt="SMFS - folder icon" src="https://github.com/user-attachments/assets/81aa246b-a1c9-489e-a643-0db0c875fa0a" />


# smfs

Your Supermemory container, exposed as a filesystem. Read, write, and `grep` your memory like any local directory.

Two access flows depending on the runtime:

- **Mount it as a directory.** A real local folder for editors, scripts, and any tool that reads files. Works wherever a kernel and filesystem exist (macOS, Linux, devcontainers, Codespaces, Docker, microVMs).
- **Plug the virtual bash tool into the agent's tool-set.** A TypeScript package for runtimes with no local filesystem at all: Cloudflare Workers, serverless functions, edge runtimes, browser-based agents.

## Contents

- [API v5 compatibility](#api-v5-compatibility)
- [Install](#install)
- [Quickstart](#quickstart)
- [Memory generation paths](#memory-generation-paths)
- [Commands](#commands)
- [Mount flags](#mount-flags)
- [Semantic search](#semantic-search)
- [`bash/` virtual bash tool](#bash-virtual-bash-tool)
- [Build from source](#build-from-source)
- [Docker and devcontainers](#docker-and-devcontainers)
- [License](#license)

## API v5 compatibility

This checkout uses the namespace-scoped v5 API for documents, uploads, lists,
search, profiles and memory-path settings. The existing mount argument is the
namespace: `smfs mount agent_memory` still targets `agent_memory`. Do not rename
it or switch to `sm_project_default` when upgrading an existing mount.

Unmount cleanly and let pending writes drain before replacing the binary. Keep
its existing cache and credentials; this change does not wipe the cache, re-import
data or change namespace identities. Old cached document IDs are checked before deletion
reconciliation and retained as local aliases when v5 returns a caller-defined
ID. Cached IDs are resolved once per mount before list-driven synchronization. An
unresolved check leaves synchronization unavailable rather than guessing a
deletion; the existing offline-mount behavior is retained.
On a fresh mount, documents without a filesystem path use their public v5 ID
for the synthesized filename. The cache alias path is intended to preserve
existing cached names. A live Linux upgrade from the previous v4 binary retained
the original cache and internal document ID and replayed a queued offline edit.

The filesystem-specific `filepath` and `memoryFilesystemPaths` fields remain
supported by the API but are hidden from its public SDK types. SMFS retains
these fields, including existing server-side collision and memory-path rules;
it does not move paths into metadata or rewrite stored documents. Authentication
still uses the supported `/v3/session` route, not an invented v5 auth endpoint.

The TypeScript bash package requires the `supermemory` **5.x** SDK; SDK 4.x is no
longer compatible. Its `namespace` option is preferred; `containerTag` remains
accepted for existing callers. Python accepts `namespace` while retaining
`container_tag`. Supply the same existing value. Self-hosted servers must support
v5 (server v0.0.9 or later); there is no automatic fallback to legacy data routes.
Namespace names must meet v5's 1–100-character alphanumeric, underscore, colon or
hyphen constraint. Incompatible legacy names are rejected, never silently renamed.

There is no v5 processing-list or filepath bulk-delete endpoint. The daemon polls
only tracked documents, with at most eight requests in flight. Directory deletes
first collect matching IDs, then delete batches of at most 100, checking per-ID
errors. Successful deletions are not rolled back when another item fails.
Directory deletion is a snapshot, not an atomic transaction with concurrent
remote renames; avoid other writers while deleting a tree.

New text files receive explicit v5 IDs so identical-content copies remain
separate documents. The native daemon keeps creation IDs stable across retries
and restarts using its existing cache, without rewriting old remote IDs.

Building this checkout does not update published binaries or packages. Release
publication is a separate step.

## Install

```sh
curl -fsSL https://smfs.ai/install | bash
```

Supports macOS (arm64, x64) and Linux (arm64, x64).

You'll need a Supermemory API key. Get one at [supermemory.ai](https://supermemory.ai).

## Quickstart

```sh
smfs login                  # one-time, stores your API key
smfs mount agent_memory     # mounts the container tag at ./agent_memory/
ls agent_memory/
cat agent_memory/memory/notes.md
```

`smfs mount <container_tag>` mounts the named Supermemory container as a real directory. By default the folder name matches the container tag and lands in your current working directory. Override with `--path /somewhere/else`.

Inside the mount, files behave like any other folder: edit them, `cat` them, `grep` them, point your editor at them. Writes upload to Supermemory in the background; remote changes pull in every 30 seconds.

Unmount when done:

```sh
smfs unmount agent_memory
```

## Memory generation paths

Files stored in a mount are durable everywhere, but only files under the container's **memory paths** get processed by Supermemory's memory pipeline (the part that extracts structured facts and makes them semantically searchable). Everything else is plain durable storage.

By default the server applies its built-in path scope per container. Override it for a mount with `--memory-paths`:

```sh
# Scope memory generation to specific paths
# Trailing slash = match any file inside that folder recursively
# No trailing slash = exact file match
smfs mount agent_memory --memory-paths "/notes/,/journal.md,/work/"

# Disable memory generation entirely (mount becomes pure storage)
smfs mount agent_memory --memory-paths ""

# Omit the flag entirely to leave the existing server config alone
smfs mount agent_memory
```

The flag writes the configuration to the container tag, so the next mount sees the same scope until you change it again.

## Commands

```
smfs login                      one-time auth, stores API key locally
smfs whoami                     show current user, org, API endpoint
smfs mount <tag>                mount a container tag
smfs unmount <tag>              unmount and drain pending pushes
smfs list                       show all running mounts
smfs status <tag>               daemon health and queue depth
smfs logs <tag>                 tail the daemon log
smfs sync <tag>                 force a sync cycle now
smfs grep "query" [path]        semantic search inside a container
smfs init                       install the grep shell wrapper
smfs install                    self-install the binary to ~/.local/bin
smfs logout                     remove stored credentials
```

Run `smfs --help` or `smfs <command> --help` for full flag listings.

## Mount flags

```
--path <DIR>             override the mount path (default: ./<tag>/)
--backend fuse|nfs       defaults: fuse on Linux, nfs on macOS
--foreground             run in foreground instead of detaching
--memory-paths "<csv>"   which paths produce memories (see above)
--ephemeral              in-memory cache; nothing persists after unmount
--clean                  wipe local cache before mounting
--sync-interval <secs>   pull interval, default 30
--no-sync                disable the pull side; local writes still push
--drain-timeout <secs>   max wait at unmount to drain the push queue, default 30
--key <KEY>              API key (otherwise resolved from stored credentials)
--api-url <URL>          override the API base URL
```

## Semantic search via plain `grep`

Run `smfs init` once. After that, `grep` inside any mount routes through Supermemory's semantic index automatically when called without flags. No new command to learn, no new tool to teach an agent.

```sh
cd agent_memory/

grep "OAuth refresh tokens"          # semantic: finds files about the topic
grep "design review notes" work/     # semantic, scoped to a directory

grep -F "exact string" notes.md      # any flag falls through to real grep
grep -rF "literal" .                 # also real grep (literal substring)
```

The wrapper detects when your shell is inside an smfs mount via a hidden `.smfs` marker. Outside a mount, `grep` is unchanged. Inside a mount, flagless `grep` is semantic and flagged `grep` is literal: that split is the whole UX.

If you need to run a semantic search from outside a mount, `smfs grep "query" --tag <container_tag>` does the same thing without the wrapper.

## `bash/` virtual bash tool

A TypeScript package (`@supermemory/bash`) for AI agents running where there is no local filesystem to mount onto: Cloudflare Workers, serverless functions, edge runtimes, browser-based agents. The bash tool *is* the filesystem. Drop a single `run_bash` tool into the agent's tool-set, and the agent uses every Unix command it already knows, plus an `sgrep` command for semantic search across the whole container.

```ts
import { createBash } from "@supermemory/bash";

const { bash, toolDescription } = await createBash({
  apiKey: process.env.SUPERMEMORY_API_KEY!,
  namespace: "user_42",
});

await bash.exec("echo 'hello' > /a.md && cat /a.md");
await bash.exec("sgrep 'authentication tokens'");
```

Full quickstart, options, and Vercel AI SDK examples: [`bash/README.md`](bash/README.md).

## Build from source

```sh
cargo build --release
./target/release/smfs --help
```

Requires Rust 1.80 or newer.

## Docker and devcontainers

### Run smfs in Docker

Build the development image from this checkout:

```sh
docker build -t smfs:dev .
docker run --rm smfs:dev --help
```

Mount a Supermemory container inside Docker with FUSE enabled:

```sh
docker run --rm -it \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  -e SUPERMEMORY_API_KEY="$SUPERMEMORY_API_KEY" \
  smfs:dev mount agent_memory --path /mnt/memory
```

The release Dockerfile uses the same installer as the shell quickstart:

```sh
docker build -t smfs:release -f docker/Dockerfile.release .
```

When an official image is published, the same run flags apply:

```sh
docker run --rm -it \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  -e SUPERMEMORY_API_KEY="$SUPERMEMORY_API_KEY" \
  ghcr.io/supermemoryai/smfs:latest mount agent_memory --path /mnt/memory
```

### Use smfs inside a devcontainer

Open this repository in VS Code or Cursor and choose "Reopen in Container".
The devcontainer builds the root `Dockerfile`, passes through `SUPERMEMORY_API_KEY`
and `SUPERMEMORY_API_URL`, and starts with `smfs` on `PATH`.

### FUSE requirements

Docker runs Linux containers, so smfs uses the FUSE backend in Docker. The NFS
backend is for macOS hosts and is not used inside Linux containers.

FUSE needs access to `/dev/fuse` and the `SYS_ADMIN` capability:

```sh
--device /dev/fuse --cap-add SYS_ADMIN
```

If your Docker environment does not expose `/dev/fuse`, use a Linux host or a
container runtime that supports FUSE devices.

## License

MIT. See [`LICENSE`](LICENSE).
