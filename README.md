# Claudeform

Claudeform executes agentic programs from markdown files.

You keep instructions in repo files, run `cf apply -f <program.md>`, and Claudeform executes the program with an agent while preserving session context in local state. This gives you a file-based workflow outside chat UI and reduces dependence on proprietary interfaces.

Yes, it is called Claudeform. Yes, Codex is the first supported provider. Claude support is on the way.

## Why It Exists

- move agent workflows from chat windows into versioned files
- run the same program repeatedly with session context and diff-aware previews
- keep execution history in your workspace, not only in a provider UI
- keep the control surface simple: markdown programs + CLI apply

## What A Program Is

A **program** is one markdown file that describes desired outcomes, constraints, and execution context for an agent.

This follows the `program.md` style used in agentic programming workflows: the markdown body is intentionally flexible and expressive, while Claudeform keeps only tool-critical fields strict.

Program frontmatter:

- `id` (optional)
- `model` (optional)

## How `apply` Works

`cf apply -f <program.md>` runs one **session** for one program.

Before execution, Claudeform previews:

- last session status and summary (if available)
- last session changed files
- program diff since the last comparable snapshot

Then it asks for confirmation and executes the program with the configured provider.

During execution, Claudeform streams agent progress events to the terminal and records the full event stream and outputs for later inspection.

After execution, Claudeform stores:

- agent/session outcome
- changed files reported for this session
- prompt, events, logs, and snapshots

## Install

Install latest stable:

```bash
curl -fsSL https://raw.githubusercontent.com/dstackai/claudeform/main/install.sh | sh
```

Install a specific version:

```bash
CLAUDEFORM_VERSION=v0.0.2 curl -fsSL https://raw.githubusercontent.com/dstackai/claudeform/main/install.sh | sh
```

## Quick Start

1. Create `.claudeform/config.json`:

```json
{
  "claudeform": {
    "providers": {
      "codex": {
        "type": "codex",
        "default": true,
        "default_model": "gpt-5-codex"
      }
    }
  }
}
```

2. Run:

```bash
cf apply -f examples/smoke.md
```

Override a program variable for one run:

```bash
cf apply -f examples/smoke.md --var SMOKE_VALUE=YU
```

The confirmation preview includes a variables summary, for example: `variables: 1 value changed, 0 added, 0 removed`.

Program variables are defined in frontmatter under `variables` (`NAME: {}` for required, `NAME: { default: "..." }` for optional defaults) and referenced as `${{ var.NAME }}`.

Example output (will vary by session/model):

```text
cf apply -f examples/smoke.md
Last session: 019d5843-eb2d-70b1-b49a-343033117944 (success, 43m ago)
  program: examples/smoke.md unchanged
  changes: 0 files
Proceed? [y/N] y
session 019d586b-aa65-78b2-8a0d-27b5543c59bb
✔ cat examples/smoke.md | 1ms | out
💬 Verified `example-data/output-smoke.txt:1` already contains the required `SMOKE_OK` line with trailing newline. | msg
turn 1 | tokens: in=117k out=1.6k cached=107k
total | tokens: in=117k out=1.6k cached=107k
changes: 0 files
```

## State Layout

Claudeform keeps local state under `.claudeform/`:

- config: `.claudeform/config.json`
- history index: `.claudeform/history/index.jsonl`
- per-program sessions: `.claudeform/programs/<program_id>/sessions/<session_id>/`

Session folders include prompt, plan metadata, streamed events, provider stdout/stderr, outcome, and session output summary.

## Commands

```bash
cf apply -f <program.md>
```

## Status

Claudeform is a work in progress. Issues and feedback are very welcome.

Additional links:

- [`contrib/ARCHITECTURE.md`](contrib/ARCHITECTURE.md)
- [`contrib/DEVELOPMENT.md`](contrib/DEVELOPMENT.md)
