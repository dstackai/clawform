# Clawform

Clawform executes agentic programs from markdown files.

You keep instructions in repo files and run `cf -f program.md` (equivalent to `cf apply -f program.md`). Each run writes session data under `.clawform/programs/<program_id>/sessions/<session_id>/` and appends `.clawform/history/index.jsonl`.

## Why It Exists

- move agent workflows from chat windows into versioned files
- run the same program repeatedly with last-session diffs (`program.md`/`variables.json`) and history-based previews
- keep execution history in your workspace, not only in a provider UI
- keep the control surface simple: markdown programs + CLI apply

## What A Program Is

A **program** is one markdown file that describes desired outcomes, constraints, and execution context for an agent.

This follows the `program.md` style used in agentic programming workflows: the markdown body is intentionally flexible and expressive, while Clawform keeps only tool-critical fields strict.

Program frontmatter:

- `id` (optional)
- `model` (optional)

## How `apply` Works

`cf -f program.md` runs one **session** for one program.

Before execution, Clawform previews:

- last session status and summary (if available)
- last session changed files
- program diff since the last comparable snapshot

Then it asks for confirmation and executes the program with the configured provider, or with the explicitly selected provider when you pass `-p/--provider`.

During execution, Clawform streams progress events to the terminal. Use `--progress rich|plain|off` to control rendering. Use `--sandbox auto|workspace|full-access` to choose sandbox policy (default: `auto`), or the shorthand flags `--auto`, `--workspace`, and `--full-access`.

After execution, Clawform stores:

- run outcome and summary (`outcome.json`, `output.md`)
- per-session snapshots for next-run diff (`program.md`, `variables.json`)
- changed files reported for this session

## Install

Install latest stable:

```bash
curl -fsSL https://raw.githubusercontent.com/dstackai/clawform/main/install.sh | sh
```

Install a specific version:

```bash
CLAWFORM_VERSION=v0.0.8 curl -fsSL https://raw.githubusercontent.com/dstackai/clawform/main/install.sh | sh
```

## Quick Start

1. Create `.clawform/config.json`:

```json
{
  "clawform": {
    "providers": {
      "codex": {
        "type": "codex",
        "default": false,
        "default_model": "gpt-5-codex"
      },
      "claude": {
        "type": "claude",
        "default": true,
        "default_model": "sonnet"
      }
    }
  }
}
```

2. Run:

```bash
cf -f examples/smoke.md
```

Run the same program with a specific provider override:

```bash
cf -f examples/smoke.md -p claude
cf -f examples/smoke.md -p codex
```

Override a program variable for one run:

```bash
cf -f examples/smoke.md --var SMOKE_VALUE=YU
```

The confirmation preview includes a variables summary, for example: `variables: 1 value changed, 0 added, 0 removed`.

Program variables are defined in frontmatter under `variables` (`NAME: {}` for required, `NAME: { default: "..." }` for optional defaults) and referenced as `${{ var.NAME }}`.

Example output (will vary by session/model):

```text
cf -f examples/smoke.md
Last session: 019d5843-eb2d-70b1-b49a-343033117944 (success, 43m ago)
  program: examples/smoke.md unchanged
  changes: 0 files
Proceed? [y/N] y
🧵 019d586b-aa65-78b2-8a0d-27b5543c59bb | workspace
✔ cat examples/smoke.md | 1ms | out
💬 Verified `example-data/output-smoke.txt:1` already contains the required `SMOKE_OK` line with trailing newline. | msg
turn 1 | tokens: in=117k out=1.6k cached=107k
✅ Verified `example-data/output-smoke.txt` already contains `SMOKE_OK`. | file
total | tokens: in=117k out=1.6k cached=107k
changes: 0 files
```

## State Layout

Clawform keeps local state under `.clawform/`:

- config: `.clawform/config.json`
- history index: `.clawform/history/index.jsonl`
- per-program sessions: `.clawform/programs/<program_id>/sessions/<session_id>/`

Session folders keep `program.md`, `variables.json`, `output.md`, `outcome.json`, plus `commands/*` and `messages/*` used by interactive `out`/`msg` links.

For internal protocol details and strict agent result schema, see `contrib/ARCHITECTURE.md`.

## Commands

```bash
cf -f program.md
cf apply -f program.md
cf -f program.md -p claude
cf -f program.md --auto
cf -f program.md --progress plain
cf -f program.md --workspace
cf -f program.md --full-access
```

## Status

Clawform is a work in progress. Issues and feedback are very welcome.

Additional links:

- [`contrib/ARCHITECTURE.md`](contrib/ARCHITECTURE.md)
- [`contrib/DEVELOPMENT.md`](contrib/DEVELOPMENT.md)
