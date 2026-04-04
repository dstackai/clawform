# Claudeform Architecture

Last updated: 2026-04-04  
Status: v0 (execution-first)

This file is the source of truth for v0 behavior.

## 1) Product Goal

Claudeform runs markdown programs with agents from files instead of chat windows.

A **program** is one `*.md` file that represents one task.

- frontmatter is strict and tool-owned (`id`, `model`)
- markdown body is agent-owned and free-form

## 2) v0 Scope

1. One public command surface with two binary names: `claudeform apply -f <program.md> [--confirm]` and `cf apply -f <program.md> [--confirm]`
2. One program file = one task
3. Strict config file at `<cwd>/.claudeform/config.json`
4. Local state at `<cwd>/.claudeform/state`
5. Codex provider only in v0
6. Optional confirmation (`Proceed? (y/N)` only when `--confirm`)
7. Live progress stream from provider is enabled by default (`--no-progress` disables it)
8. Provider execution has heartbeat output; runtime budget/timeout policy is provider-defined in v0

## 3) Critical v0 Principle

Claudeform does **not** parse or enforce strict input/output contracts from markdown body in v0.

- Inputs/Outputs in markdown are for the agent to interpret.
- Terraform-like strict I/O planning is out-of-scope for v0.

## 4) Files and Ownership

## 4.1 `.claudeform/config.json`

Purpose:

- tool configuration only (not agent prompt content)
- provider selection and default model

Path resolution in v0:

- Claudeform loads exactly `<cwd>/.claudeform/config.json`

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

Validation:

1. exactly one provider must set `"default": true`
2. provider `type` must be `"codex"` in v0

## 4.2 Program Markdown (`*.md`)

Frontmatter (strict, tool-owned):

- `id` (optional)
- `model` (optional override)

Body (free-form, agent-owned):

- instructions, context, examples, any markdown structure

Program key:

1. if `id` exists, key = `id`
2. otherwise key = filename stem

## 5) Runtime Model

`apply` pipeline:

1. load program markdown
2. load strict config
3. resolve provider + model
4. optionally ask confirmation (`Proceed? (y/N)` with `--confirm`)
5. copy workspace to temp directory
6. run `codex exec` in temp workspace with:
   - full markdown program
   - Claudeform runtime instruction block
   - instruction to write `./.claudeform/agent_outputs.json` (JSON array of changed files)
7. detect changed files by diffing temp workspace files against real workspace files
8. promote changed files from temp to real workspace
9. read agent output manifest for user-facing output summary (informational only)
10. persist local state on success

State is never updated on failed provider run.

## 5.1 Provider Layer (Codex-first)

Claudeform v0 uses a provider adapter layer even though only Codex is implemented.

Why:

- keep Claudeform runtime/provider boundary stable
- make Claude/other providers an adapter task, not a core rewrite

Current adapter in v0:

- `CodexRunner` (invokes `codex exec`)

Provider contract in v0 (high-level):

1. `run(request)` executes one apply run
2. `capabilities()` returns provider feature support flags
3. provider emits live progress events (when enabled) that Claudeform normalizes for UI

## 5.2 Normalized Event Model

Claudeform normalizes provider stream events into a common internal model:

- `run_started`
- `turn_started`
- `turn_completed` (with usage if available)
- `turn_failed`
- `item_started` / `item_updated` / `item_completed`
- `error`
- `heartbeat`
- `raw_event` / `raw_text` (passthrough fallback)

This model is intentionally small in v0.  
It is for progress UX and orchestration, not for strict planning.

## 5.3 Capability Handshake

Before running, Claudeform can read provider capabilities from the adapter.

v0 capability fields:

- `live_events`
- `partial_text`
- `tool_call_events`
- `file_change_events`
- `resume`
- `cancel`
- `approvals`

Codex v0 capability profile:

- live events: yes
- partial text: no (v0 does not rely on token deltas)
- tool call events: yes (from Codex item events)
- file change events: yes
- resume: yes
- cancel: no
- approvals: no

## 5.4 Codex Mapping in v0

Codex JSON event types (for example `thread.started`, `turn.*`, `item.*`, `error`) are mapped into the normalized event model.

Claudeform then:

1. prints concise progress lines
2. uses heartbeat for liveness visibility (no Claudeform-imposed runtime timeout in v0)
3. keeps raw stdout/stderr for debug output

## 6) Safety Model

1. Provider execution happens in a temp workspace copy.
2. Claudeform promotes changed files from temp to real workspace.
3. `.git`, `target`, `.claudeform/state`, and `.claudeform/agent_outputs.json` are excluded from copy/promote scanning.
4. Confirmation gate is optional (`--confirm`).

## 7) Local State

Location:

- `<cwd>/.claudeform/state/index.json`
- `<cwd>/.claudeform/state/programs/<program_key>.json`

Program state includes:

- `program_key`
- source program path
- resolved model
- provider fingerprint
- program fingerprint
- last success timestamp
- hashes map of files promoted in last successful run

State is Claudeform-owned and independent from agent session memory.

## 8) Out of Scope (v0)

1. Strict typed input/output schema in markdown body
2. Canonical I/O extraction/IR and strict output validation
3. Deterministic `no-op` planning based on typed I/O contracts
4. Standalone `plan` command
5. Saved plan artifact (`plan` file passed to `apply`)
6. MCP/tool integration schema and auth bootstrap
7. Skills configuration policy
8. Multi-agent orchestration
9. OpenClaw integration
10. Remote/shared state backends
11. Multi-user locking/concurrency model
12. Claude provider adapter implementation
13. Variables
14. Configurable runtime budget / max-time policy
15. Automatic workspace forking and revert/rollback semantics (including failure-time revert)

## 8.1 TODO (Post-v0): Multi-agent Orchestrator

Codex CLI does not provide a first-class parent/child multi-agent primitive for Claudeform v0.

Planned direction after v0:

1. Claudeform orchestrates multiple provider runs in parallel (external orchestration layer)
2. each run is an isolated task unit with its own state checkpoint
3. Claudeform merges outputs/conflicts and emits one combined progress view

## 8.2 TODO (Post-v0): Runtime Strategy Decision

For advanced capabilities (tool calling, MCP integration, richer model controls, multi-agent orchestration), Claudeform must choose one runtime strategy:

1. Adapter-over-agent-CLIs
   - pros: fastest integration for existing agent UX
   - cons: inconsistent event schemas, weaker cross-provider parity, less control over orchestration internals
2. Direct provider runtime (own orchestrator over provider APIs/SDKs)
   - pros: strongest control for unified events, tool routing, MCP policy, and orchestration
   - cons: highest implementation and maintenance cost
3. Hybrid (CLI adapters in v1, direct runtime for critical paths later)
   - pros: fastest path now, controlled migration path later
   - cons: temporary dual architecture complexity

Decision status: deferred post-v0.  
v0 remains Codex-adapter-first and intentionally minimal.

## 8.3 TODO (Post-v0): Canonical Log

Goal:

- keep an exact run history with minimum Claudeform-specific interpretation
- separate provider facts from Claudeform-derived summaries

Planned approach:

1. store append-only raw provider events as NDJSON (canonical source of provider behavior)
2. store Claudeform-observed outcomes separately (real changed files, diff stats, apply status)
3. keep derived UI projections rebuildable from canonical + outcomes logs
4. avoid mixed blobs that combine text, commands, and file contents in one field

Proposed layout:

- `<cwd>/.claudeform/sessions/<program_key>/<session_id>/events.ndjson`
- `<cwd>/.claudeform/sessions/<program_key>/<session_id>/outcome.json`
- `<cwd>/.claudeform/sessions/index.jsonl` (compact cross-program index)

Notes:

- canonical log should preserve provider-native identifiers (for example session/thread ids)
- Claudeform should only add minimal envelope fields (sequence, receive timestamp, provider name)
- high-level summaries should remain optional views, not source of truth
