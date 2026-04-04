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

## 6) Known v0 Limits

1. Markdown body is untyped/unvalidated (no strict input/output schema).
2. No standalone `plan` command.
3. No MCP/tool integration schema yet.
4. No multi-agent orchestration.
5. No Claude provider adapter yet.

## 7) TODO (Next)

## 7.1 Interrupted Session Semantics

Problem today:

- `Ctrl+C` is recorded as `failure`, which can later show `snapshot unavailable` in preview.

Need:

1. introduce explicit interrupted/canceled session reason in session outcome/history
2. preview should show `interrupted`/`canceled` instead of generic `failure` when applicable
3. program diff fallback should use last successful snapshot when last session has no snapshot
4. inject this context clearly to agent in next session prompt

## 7.2 Changes/Diff Reliability

Need:

1. make changed-file reporting consistent between normal mode and debug mode
2. improve noise filtering for generated/build-cache files
3. keep preview/apply/history change summaries aligned

## 7.3 Apply Contract Polish

Need:

1. tighten prompt contract so reruns focus on required deltas
2. clarify when agent should rework vs verify vs no-op
3. keep status semantics (`success`/`partial`/`failure`) explicit and consistent

## 7.4 Future Decisions (Out of Scope for v0)

1. MCP and broader tool integration model
2. multi-agent orchestration model
3. additional providers beyond Codex
4. strict typed program schema for markdown body
