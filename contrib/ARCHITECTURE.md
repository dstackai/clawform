# Clawform Architecture

Last updated: 2026-04-06  
Status: v0 (implemented baseline)

## 1) Product Goal

Clawform runs agent work from markdown files instead of chat windows.

A **program** is one markdown file (`*.md`) representing one task.

- frontmatter is tool-owned and strict (`id`, `model`, `variables`)
- markdown body is agent-facing and free-form

## 2) Implemented v0 Scope

1. Public command: `cf -f <program.md>` (explicit equivalent: `cf apply -f <program.md>`, full binary form: `clawform apply -f <program.md>`)
2. Confirmation prompt is default in interactive shell; use `--yes` to skip
3. One program file = one session execution
4. Config path is fixed: `<cwd>/.clawform/config.json`
5. Provider support in v0: Codex only
6. Live progress events are on by default (`--no-progress` disables)
7. Session artifacts and run history are stored under `.clawform/`

## 3) Config and Program

## 3.1 Config

Path:

- `<cwd>/.clawform/config.json`

Rules:

1. exactly one provider has `"default": true`
2. provider `type` must be `"codex"` in v0

Example:

```json
{
  "clawform": {
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

## 3.2 Program

Frontmatter (strict):

- `id` (optional)
- `model` (optional override)
- `variables` (optional map)
  - key: variable name (`[A-Za-z_][A-Za-z0-9_]*`)
  - value:
    - required (no default): `NAME: {}`
    - optional default: `NAME: { default: "value" }`

Program key resolution:

1. use `id` if present
2. otherwise use filename stem

Markdown body remains untyped in v0 and is interpreted by the agent.

Variable rules:

1. markdown may reference variables as `${{ var.NAME }}`
2. referenced variables must be defined in frontmatter
3. apply-time `--var NAME=VALUE` overrides frontmatter default
4. variables without default are required at apply time

## 4) Apply Session Flow (Current Behavior)

1. Load program + config and resolve model.
2. Resolve program variables (frontmatter defaults + CLI `--var` overrides).
3. Validate variable definitions and `${{ var.NAME }}` references.
4. Build preview from last session context:
   - last session status/summary (if exists)
   - program diff vs last session snapshot (if available)
   - variable diff vs last session variable snapshot (if available)
5. Ask for confirmation (interactive default; skipped by `--yes`).
6. Write runtime variables file (`.clawform/agent_variables.json`) when variables are present.
7. Run provider in the current workspace (no temp workspace copy).
8. Stream provider events to terminal and persist artifacts.
9. Read agent status from `.clawform/agent_result.json` (required).
10. Collect reported changed files (events-first, manifest fallback).
11. Persist session artifacts + history record.
12. Persist program snapshot (`program.md`) and variable snapshot (`variables.json`) on success.

## 5) State and Storage Layout

## 5.1 Session Artifacts

Per program/session:

- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/prompt.md`
- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/plan.json`
- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/events.ndjson`
- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/provider.stdout.log`
- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/provider.stderr.log`
- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/outcome.json`
- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/output.md` (Clawform summary)
- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/program.md` (success snapshot)
- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/variables.json` (success snapshot)
- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/commands/*` (captured command outputs)
- `<cwd>/.clawform/programs/<program_id>/sessions/<session_id>/messages/*` (captured message outputs)

## 5.2 Run History Index

- `<cwd>/.clawform/history/index.jsonl`

This is Clawform-owned history, independent from provider-side memory.

## 5.3 Agent Report Files in Workspace Root

Agent may write:

- `<cwd>/.clawform/agent_result.json` (required)
- `<cwd>/.clawform/agent_output.md` (optional human summary)
- `<cwd>/.clawform/agent_outputs.json` (optional fallback list of changed files)
- `<cwd>/.clawform/agent_variables.json` (runtime resolved variable values provided by Clawform)

These files are execution protocol files, not user deliverables.

## 6) Known Bugs

### 6.1 Interrupted Runs Recorded as Failure

Steps to reproduce:

1. Start `cf -f <program.md>` in interactive mode.
2. Interrupt with `Ctrl+C`.
3. Run apply again and inspect the last-session preview/history.

Expected:

- Interrupted runs are shown as `interrupted`/`canceled`.

Actual:

- Interrupted runs are recorded and shown as generic `failure`.

## 7) Backlog (Out of Scope for now)

These items are intentionally deferred. Each item describes desired product capability, not implementation.

1. Memory support  
   Goal: support durable context across sessions with predictable usage rules.
2. Plan support  
   Goal: support planning as a first-class workflow, separate from execution.
3. Interrupted/canceled session handling  
   Goal: represent and communicate non-completed runs clearly to users.
4. Changes/diff reliability and consistency  
   Goal: for the same session, preview, apply output, debug output, and history should report the same changed-file set and line counts, with generated/noise files handled consistently.
5. Agent-reported changes as single source of truth  
   Goal: remove legacy local diff-based change reporting and use agent-reported change data consistently across apply, debug, and history.
6. MCP and broader tool integration model  
   Goal: support richer external tool and integration patterns.
7. Multi-agent orchestration model  
   Goal: support coordinated workflows that involve more than one agent.
8. Additional providers beyond Codex  
   Goal: support multiple model providers in a consistent user experience.
9. Improved session storage and retrieval performance  
   Goal: keep history/state operations fast and scalable as usage grows.
