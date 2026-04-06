# Claudeform Architecture

Last updated: 2026-04-04  
Status: v0 (implemented baseline)

## 1) Product Goal

Claudeform runs agent work from markdown files instead of chat windows.

A **program** is one markdown file (`*.md`) representing one task.

- frontmatter is tool-owned and strict (`id`, `model`)
- markdown body is agent-facing and free-form

## 2) Implemented v0 Scope

1. Public command: `claudeform apply -f <program.md>` (alias: `cf apply -f <program.md>`)
2. Confirmation prompt is default in interactive shell; use `--yes` to skip
3. One program file = one session execution
4. Config path is fixed: `<cwd>/.claudeform/config.json`
5. Provider support in v0: Codex only
6. Live progress events are on by default (`--no-progress` disables)
7. Session artifacts and run history are stored under `.claudeform/`

## 3) Config and Program

## 3.1 Config

Path:

- `<cwd>/.claudeform/config.json`

Rules:

1. exactly one provider has `"default": true`
2. provider `type` must be `"codex"` in v0

Example:

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

## 3.2 Program

Frontmatter (strict):

- `id` (optional)
- `model` (optional override)

Program key resolution:

1. use `id` if present
2. otherwise use filename stem

Markdown body remains untyped in v0 and is interpreted by the agent.

## 4) Apply Session Flow (Current Behavior)

1. Load program + config and resolve model.
2. Build preview from last session context:
   - last session status/summary (if exists)
   - program diff vs last session snapshot (if available)
3. Ask for confirmation (interactive default; skipped by `--yes`).
4. Run provider in the current workspace (no temp workspace copy).
5. Stream provider events to terminal and persist artifacts.
6. Read agent status from `.claudeform/agent_result.json` (required).
7. Collect reported changed files (events-first, manifest fallback).
8. Persist session artifacts + history record.
9. Persist program snapshot (`program.md`) on success.

## 5) State and Storage Layout

## 5.1 Session Artifacts

Per program/session:

- `<cwd>/.claudeform/programs/<program_id>/sessions/<session_id>/prompt.md`
- `<cwd>/.claudeform/programs/<program_id>/sessions/<session_id>/plan.json`
- `<cwd>/.claudeform/programs/<program_id>/sessions/<session_id>/events.ndjson`
- `<cwd>/.claudeform/programs/<program_id>/sessions/<session_id>/provider.stdout.log`
- `<cwd>/.claudeform/programs/<program_id>/sessions/<session_id>/provider.stderr.log`
- `<cwd>/.claudeform/programs/<program_id>/sessions/<session_id>/outcome.json`
- `<cwd>/.claudeform/programs/<program_id>/sessions/<session_id>/output.md` (Claudeform summary)
- `<cwd>/.claudeform/programs/<program_id>/sessions/<session_id>/program.md` (success snapshot)
- `<cwd>/.claudeform/programs/<program_id>/sessions/<session_id>/commands/*` (captured command outputs)
- `<cwd>/.claudeform/programs/<program_id>/sessions/<session_id>/messages/*` (captured message outputs)

## 5.2 Run History Index

- `<cwd>/.claudeform/history/index.jsonl`

This is Claudeform-owned history, independent from provider-side memory.

## 5.3 Agent Report Files in Workspace Root

Agent may write:

- `<cwd>/.claudeform/agent_result.json` (required)
- `<cwd>/.claudeform/agent_output.md` (optional human summary)
- `<cwd>/.claudeform/agent_outputs.json` (optional fallback list of changed files)

These files are execution protocol files, not user deliverables.

## 6) Known Bugs

### 6.1 Interrupted Runs Recorded as Failure

Steps to reproduce:

1. Start `cf apply -f <program.md>` in interactive mode.
2. Interrupt with `Ctrl+C`.
3. Run apply again and inspect the last-session preview/history.

Expected:

- Interrupted runs are shown as `interrupted`/`canceled`.

Actual:

- Interrupted runs are recorded and shown as generic `failure`.

## 7) Backlog (Out of Scope for now)

These items are intentionally deferred. Each item describes desired product capability, not implementation.

1. Program variables support  
   Goal: allow programs to define reusable runtime inputs with clear behavior.
2. Memory support  
   Goal: support durable context across sessions with predictable usage rules.
3. Plan support  
   Goal: support planning as a first-class workflow, separate from execution.
4. Interrupted/canceled session handling  
   Goal: represent and communicate non-completed runs clearly to users.
5. Changes/diff reliability and consistency  
   Goal: for the same session, preview, apply output, debug output, and history should report the same changed-file set and line counts, with generated/noise files handled consistently.
6. Agent-reported changes as single source of truth  
   Goal: remove legacy local diff-based change reporting and use agent-reported change data consistently across apply, debug, and history.
7. MCP and broader tool integration model  
   Goal: support richer external tool and integration patterns.
8. Multi-agent orchestration model  
   Goal: support coordinated workflows that involve more than one agent.
9. Additional providers beyond Codex  
   Goal: support multiple model providers in a consistent user experience.
10. Improved session storage and retrieval performance  
   Goal: keep history/state operations fast and scalable as usage grows.
