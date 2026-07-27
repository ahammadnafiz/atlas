<!-- #todo BANNER: replace with a wide product banner (1584x396 works well) -->
![Atlas](landing/og-image.png)

<div align="center">

[![Discord](https://img.shields.io/badge/Discord-Join%20Server-5865F2.svg?style=for-the-badge&logo=discord&logoColor=white)](https://discord.gg/GmnFggaPfP)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=for-the-badge)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/pacifio/atlas?include_prereleases&label=Release&style=for-the-badge)](https://github.com/pacifio/atlas/releases)
[![Contributors](https://img.shields.io/github/contributors/pacifio/atlas?style=for-the-badge)](https://github.com/pacifio/atlas/graphs/contributors)

**[Download](https://github.com/pacifio/atlas/releases)** · **[Discord](https://discord.gg/GmnFggaPfP)** · **[Docs](https://cersei.tryatlas.cc/docs)** · **[Roadmap](#roadmap)**

</div>

# Atlas

<!-- #todo one-liner not settled — three candidates on the table -->

Atlas is version control for coding agents. Run **Claude Code**, **Codex**, and Atlas's own native agent side by side on the same codebase, with shared memory between them and a searchable history of what each one changed.

<!-- #todo six bullets is one too many — "Semantic recall" overlaps "One memory, three agents" -->

- **Run agents in parallel.** Multiple sessions across tabs, each streaming independently. Switching tabs never freezes or drops a run in flight.
- **One memory, three agents.** A decision Claude Code made shows up in Codex's next prompt. Plans, file changes, failures, and architecture notes are shared automatically.
- **Semantic recall on every turn.** Your message is embedded on-device and matched against the project index, so relevant history is pulled into context without you re-explaining it.
- **Your notes are agent context.** Markdown in `.atlas/knowledge/`, plus the `CLAUDE.md` and `AGENTS.md` you already wrote, feed every agent in the project.
- **`@` anything into a prompt.** Files, folders, symbols, branches, commits, notes, papers, and past sessions resolve locally before the prompt is sent.
- **Local by default.** Code, notes, and sessions stay on your machine. Sign in and create an organisation when you want to sync across a team.

**[Join the Discord](https://discord.gg/GmnFggaPfP)** · `#general` chat · `#dev` build questions · `#feature-requests` ideas · `#bugs` report breakage

## Download

Grab the latest `.dmg` from [tryatlas.cc](https://www.tryatlas.cc/) or the [releases page](https://github.com/pacifio/atlas/releases). macOS is the supported platform.

<!-- #todo homebrew tap so this becomes `brew install atlas` -->

Prefer to compile it yourself? See [Build from source](#build-from-source).

## Table of contents

- [Download](#download)
- [Why Atlas](#why-atlas)
- [How it works](#how-it-works)
- [Features](#features)
- [Build from source](#build-from-source)
- [Contributing](#contributing)
- [Roadmap](#roadmap)
- [Local by default](#local-by-default)
- [Links](#links)

## Why Atlas

<!-- #todo bullets to be refined in a later pass -->

- **Agents start from zero every session.** Atlas keeps a persistent, on-device memory of decisions, plans, and changes, and pushes the relevant parts into each new turn.
- **Switching agents loses everything.** In Atlas, the first message of a new session carries a curated fact pack plus the tail of your last session — including one from a different agent.
- **You can't review what you can't see.** Every session is stored and searchable, alongside a real commit graph and file-level diffs of what actually landed.
- **Context lives in ten places.** The knowledge base, `CLAUDE.md`, `AGENTS.md`, Claude Code's own memory files, and Codex's history are folded into one index every agent reads from.
- **Nothing is locked in.** Notes are markdown, canvases are JSON, sessions are JSONL, and the editor is a file on disk. Close Atlas and pick up in vim.
- **Built for agents, not adapted for them.** Atlas is not a fork of an existing editor with a chat pane added. It is what an IDE looks like when agents are the primary user.

## How it works

Atlas runs Claude Code and Codex as they are, and enriches what they see.

Both run as external subprocesses over [ACP](https://github.com/zed-industries/agent-client-protocol). The Atlas agent runs in-process on [Cersei](https://cersei.tryatlas.cc/docs), our Rust agent framework. All three go through the same send path, so everything below applies whichever one you pick.

Before your message reaches the agent, Atlas assembles context around it:

| Injected | Where it comes from | When |
|---|---|---|
| **`@` mentions** | Resolved locally in Rust before the prompt is sent. Notes, skills, papers, and past sessions are inlined; files and folders resolve to a path | Every turn |
| **Shared agent memory** | Active plan, decisions, file changes, failures, and architecture notes, written by any agent | Every turn |
| **Semantic matches** | Your message is embedded on-device and matched against the project's memory index | Every turn |
| **Session handoff** | A curated fact pack plus the tail of your last session in this project, including one from a different agent | First message |
| **What you already wrote** | Knowledge notes, `CLAUDE.md`, `AGENTS.md`, Claude Code's memory files, and Codex's history, folded into one index | Continuously |

- **One path, no per-agent special-casing.** Run your existing Claude Code or Codex subscription through Atlas and the session gets more context, with no change to how you work.
- **Claude Code's memory is visible to Codex, and the reverse.** Neither agent can read the other's history on its own.
- **Folders resolve to a pointer, not a paste.** `@`-ing a 5000-line file sends a path the agent reads on demand, so one mention doesn't occupy the context window for the rest of the session.
- **Embedding runs on your machine.** Retrieval never leaves the device.

## Features

### Agents

| Capability | Description |
|---|---|
| **Multi-agent sessions** | Claude Code, Codex, and Atlas's native agent, selectable per session and running in parallel across tabs. Sessions are independent of tabs, so switching never drops a run in flight |
| **Shared agent memory** | On-device semantic index (local embeddings, HNSW search) that every agent reads from and writes to |
| **`@` mentions** | Local resolution of files, folders, symbols, branches, commits, notes, skills, papers, and past sessions |
| **Skills** | `SKILL.md` files scoped globally or per project, enabled per agent by symlinking into that agent's own skills directory |
| **Packs** | Install a GitHub repo of skills, subagents, commands, hooks, rules, and scripts, discovered through the skills.sh index |
| **Model chat** | Talk to a model directly in its own tab, with no agent loop around it |
| **Organisations** | Sign in, create an organisation, and sync across devices and teammates |

### Agent history

| Capability | Description |
|---|---|
| **Session capture** | Every session recorded to `.atlas/sessions.db`: prompts, messages, tool calls, the files each one touched, and the patches it applied |
| **Checkpoints** | Each session linked to the commits it produced. Commits are observed rather than intercepted, so one made from a terminal, from another editor, or while Atlas was closed still finds its session |
| **Survives history rewrites** | Links re-point through amend and rebase by patch-id reconciliation. When a squash makes the link genuinely ambiguous, it orphans instead of guessing |
| **Transcript import** | Backfills your existing Claude Code history, so the record starts before you installed Atlas |
| **Secrets scrubbed on write** | Redaction runs before anything is persisted, so the local store is never itself a disclosure risk |
| **Capture health** | One signal per workspace, OK, Degraded, or Stopped, each with a reason and the next step |
| **Mission control** | Dashboard for agent activity: usage over time, consumption breakdown, timelines, and a filterable log table |

Works with no account and no network.

### The workspace

| Capability | Description |
|---|---|
| **Editor** | CodeMirror editing surface, with per-project editor state restored across restarts |
| **Git** | Real commit graph with lane assignment, stage/unstage/commit, branch operations, and file-level diffs |
| **Terminal** | Block terminal where each command carries its own output, exit code, and duration, plus a full interactive surface for `vim`, `htop`, and friends |
| **Knowledge base** | Plain markdown notes in `.atlas/knowledge/`, versioned next to the code, with backlinks, a link graph, and export to HTML or a standalone server binary |
| **Research** | Search arXiv and Semantic Scholar, pull papers in, read them in-app, and `@`-mention them into a prompt |
| **Browser** | Native WebKit webview in a tab, with real logins, cookies, and a reader mode |
| **Spaces** | Spatial board for notes and their connections, persisted as JSON in the project |
| **Split view** | Up to three resizable columns, each with its own tabs |
| **Activity log** | Every significant event in the project, filterable, with rows you can pin across restarts |

## Build from source

Linux and Windows build from the same Tauri codebase but are untested.

To use the Claude Code agent, install the `claude` CLI and put it on your `PATH`. Atlas's native agent needs no external CLI.

Requires **[Bun](https://bun.sh/)**, **Rust** (stable, via [rustup](https://rustup.rs/)), and **Xcode Command Line Tools**.

```bash
git clone https://github.com/pacifio/atlas
cd atlas
bun install
bun run dev:app
```

The first Rust compile takes a few minutes; after that it is seconds. Use `bun run dev` for frontend-only iteration, though anything calling `invoke()` needs `dev:app`.

Production builds:

```bash
bun run build:app       # .app bundle
bun run build:app:dmg   # .app + .dmg installer
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Two things catch people out:

- **Everyone forks.** Nobody has direct push access, including the core team. Fork, branch, open a PR.
- **Feature work targets the current version branch**, not `main`. `main` only receives a finished version branch, and that merge is the release.

[ARCHITECTURE.md](ARCHITECTURE.md) covers how Atlas is built. [SECURITY.md](SECURITY.md) covers reporting vulnerabilities.

## Roadmap

<!-- #todo roadmap wording to be worked on later -->

- Source control for agents
- AI gateway via Atlas accounts
- Timeline boards covering how team members are changing code
- Organisational agents
- Pick up work from any device
- Issue tracking
- Shared documentation

## Local by default

- **Your code, notes, and sessions stay on your machine.** Nothing is uploaded to run an agent.
- **Secrets are scrubbed before anything is written to disk.** Not before upload, before persistence.
- **Accounts are opt-in.** Sign in to create an organisation and sync across devices and teammates.
- **Anonymous usage analytics are on by default.** Coarse metadata, never code or prompts. [What's collected, and how to turn it off](TELEMETRY.md).

## Links

- **Website:** [tryatlas.cc](https://www.tryatlas.cc/)
- **Cersei docs:** [cersei.tryatlas.cc/docs](https://cersei.tryatlas.cc/docs)
- **Discord:** [discord.gg/GmnFggaPfP](https://discord.gg/GmnFggaPfP)
- **Issues:** [github.com/pacifio/atlas/issues](https://github.com/pacifio/atlas/issues)
- **Telemetry:** [what Atlas collects, and how to turn it off](TELEMETRY.md)

## License

MIT. See [LICENSE](LICENSE).
