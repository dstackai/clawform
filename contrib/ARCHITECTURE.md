# Clawform Architecture

Last updated: 2026-04-11  
Status: v0 (implemented baseline)

## 1) Product Goal

Clawform runs agent work from markdown files instead of chat windows.

A **program** is one markdown file (`*.md`) representing one task.

- frontmatter is tool-owned and strict (`id`, `model`, `skills`, `variables`)
- markdown body is agent-facing and free-form

## 2) Implemented v0 Scope

1. Public command: `cf -f program.md` (explicit equivalent: `cf apply -f program.md`, full binary form: `clawform apply -f program.md`)
2. Confirmation prompt is default in interactive shell; use `--yes` to skip
3. One program file = one session execution
4. Config path is fixed: `<cwd>/.clawform/config.json`
5. Provider support in v0: Codex and Claude
6. Live progress is on by default (`--progress off` disables, `--progress plain` keeps non-interactive streaming, rich mode is used automatically in an interactive TTY)
7. Sandbox selector is exposed as `--sandbox auto|workspace|full-access` (default: `auto`) plus shorthand flags `--auto`, `--workspace`, and `--full-access`
8. Session artifacts and run history are stored under `.clawform/`

## 3) Config and Program

## 3.1 Config

Path:

- `<cwd>/.clawform/config.json`

Rules:

1. when no explicit provider override is passed, exactly one provider must have `"default": true`
2. provider `type` must be `"codex"` or `"claude"` in v0

Example:

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

## 3.2 Program

Frontmatter (strict):

- `id` (optional)
- `model` (optional override)
- `skills` (optional list of required provider-native skill names)
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

Skill rules:

1. `skills` is a shared frontmatter list, for example `skills: [dstack]`
2. each listed skill name must be a single non-empty token without whitespace
3. Clawform prepends explicit provider-native skill validation/invocation lines to the real session prompt before the normal apply contract
4. skill command syntax is provider-specific (`$skill` for Codex, `/skill` for Claude)
5. the injected validation line is phrased as `Fail if skill is not found.`
6. the injected prelude also instructs the agent to write `.clawform/agent_result.json` with `status: failure` and `reason: program_blocked` before stopping
7. if the provider still surfaces a missing-skill response without writing the result artifact, Clawform uses provider-specific fallback detection and fails the apply run

## 4) Apply Session Flow (Current Behavior)

1. Load program + config and resolve provider + model (`-p/--provider` overrides default provider selection).
2. Resolve program variables (frontmatter defaults + CLI `--var` overrides).
3. Validate variable definitions and `${{ var.NAME }}` references.
4. Build preview from previous run records:
   - last session status/summary (if exists)
   - program diff vs last session snapshot (if available)
   - variable diff vs last session variable snapshot (if available)
5. Ask for confirmation (interactive default; skipped by `--yes`).
6. Clear prior run protocol files in `.clawform/` and write runtime variables file (`.clawform/agent_variables.json`) when variables are present.
7. Build runtime prompt; if the program declares `skills`, prepend provider-native skill invocation commands first. In `workspace` and `auto` modes include explicit verdict-gate rules for sandbox-vs-program blocking.
8. Run provider in the current workspace (no temp workspace copy).
9. Normalize provider-native events into shared progress categories, stream them to terminal, and during the run write session `commands/*` and `messages/*`. In debug mode, also write session `events.ndjson`.
10. In `auto` mode, allow at most one retry in `full-access` mode only when current-run `.clawform/agent_result.json` reports `status=partial|failure` and `reason=sandbox_blocked` (no stdout/stderr heuristic fallback).
11. Read agent status from `.clawform/agent_result.json` (required) and validate strict status/reason schema.
12. Collect reported changed files from `.clawform/agent_outputs.json` when that file exists and was updated in this run.
13. Persist run-end records (`output.md`, `outcome.json`) and append `.clawform/history/index.jsonl`.
14. Persist program snapshot (`program.md`) and variable snapshot (`variables.json`) on success.

## 4.1 Progress Rendering Semantics

- In an interactive TTY, rich progress keeps a spinner plus a live `running` or `running: <activity>` status line.
- The run-start line includes the session id, execution mode, a compact `provider:model` suffix, and when applicable a compact `skills:` suffix, for example `🧵 <session> | workspace | codex:gpt-5-codex | skills:dstack`.
- `running` means the provider run is still alive. It is a liveness indicator, not the same thing as reasoning text.
- The live activity suffix is derived from the highest-priority active provider item, for example `search ...`, `fetch ...`, `use ...`, `edit ...`, or `update plan`.
- Completed progress lines are normalized across Claude and Codex into shared categories such as reasoning (`💭`), assistant text (`💬`), search (`🔎`), fetch (`🌐`), command (`❱`), file change (`✏️`), plan update (`update plan | ...`), generic tool (`🔧`), and unknown provider event (`📦`).
- In `plain` mode, Clawform prints stable progress lines without the interactive spinner/status renderer.
- Unknown tool-like and non-tool provider items are rendered through generic fallbacks instead of being silently dropped.
- Interrupting with `Ctrl+C` should surface cancellation rather than a raw provider stdout/stderr dump.

## 5) State and Storage Layout

Clawform keeps local data for three explicit behaviors:

Path aliases used in this section:

- `<protocol_root> = <cwd>/.clawform`
- `<history_index> = <protocol_root>/history/index.jsonl`
- `<session_root> = <protocol_root>/programs/<program_id>/sessions/<session_id>`
- `<last_session_root> = <protocol_root>/programs/<program_id>/sessions/<last_session_id>`
- `<last_session_id>` = `session_id` from the newest `<history_index>` record for the same `program_id`

1. Before provider execution, write `<protocol_root>/agent_variables.json` (when variables exist) so the agent can read resolved `${{ var.NAME }}` values for this run.
2. After provider execution, read agent protocol files from `<protocol_root>/`:
   - required: `agent_result.json`
   - expected for file reporting: `agent_outputs.json`
   - optional summary: `agent_output.md`
3. After apply completes, persist session/history files (`<session_root>/program.md`, `<session_root>/variables.json`, `<session_root>/output.md`, `<session_root>/outcome.json`, `<history_index>`) so the next run can compute diffs and include previous-run status/summary.

## 5.1 When Data Is Used

During the current run:

- Write `<protocol_root>/agent_variables.json` (when variables exist); the agent reads this file for resolved `${{ var.NAME }}` values.
- Write `<session_root>/commands/*` and `<session_root>/messages/*` as per-session execution artifacts.
- In debug mode, also write `<session_root>/events.ndjson` as a per-session event trace artifact.
- Read `<protocol_root>/agent_result.json`, `<protocol_root>/agent_outputs.json`, and optional `<protocol_root>/agent_output.md` at run end to determine status, changed files, and summary.

On the next run of the same program:

- Read `<history_index>`, select the newest record for the same `program_id`, and use its `session_id` as `<last_session_id>`.
- Read `<last_session_root>/program.md` and compare it to the current program file to compute program-text diff.
- Read `<last_session_root>/variables.json` and compare it to current resolved variables to compute variable diff.
- Read `<last_session_root>/output.md` as the prior run summary shown in plan preview and included in the runtime prompt.

For audit/debug visibility:

- Write `<session_root>/outcome.json` as the final machine-readable run outcome. It is for inspection and is not currently used as a control input for future apply decisions.

## 5.2 Data Inventory (What, Why, When)

| Data path | Scope | Why we store it | When it is used |
|---|---|---|---|
| `<protocol_root>/agent_variables.json` | Workspace-global scratch file for the currently running apply (overwritten on each apply) | Provide resolved runtime variables to the agent | Read by the agent during that same apply run |
| `<protocol_root>/agent_result.json` | Workspace-global scratch file for the currently running apply (overwritten on each apply) | Receive final structured run verdict (`status`, optional `reason`, `message`) where `reason` is strict enum (`sandbox_blocked` or `program_blocked`) | Read by Clawform at run end; in sandbox auto mode also used as the only retry signal source, only if file mtime is from this run |
| `<protocol_root>/agent_outputs.json` | Workspace-global scratch file for the currently running apply (overwritten on each apply) | Receive changed-file list from the agent | Read by Clawform at run end for file summary/history, only if file mtime is from this run |
| `<protocol_root>/agent_output.md` | Workspace-global scratch file for the currently running apply (optional; overwritten on each apply) | Receive agent-written summary text | Read by Clawform at run end; then copied into session `output.md` |
| `<session_root>/events.ndjson` | Per-session (`<program_id>/<session_id>`), debug mode only | Preserve normalized provider events plus raw source lines for this session | Used for audit/debug flows and event-based validation |
| `<session_root>/commands/*.txt` | Per-session (`<program_id>/<session_id>`) | Preserve command output artifacts for this session | Used for progress drilldown and debugging |
| `<session_root>/messages/*.md` | Per-session (`<program_id>/<session_id>`) | Preserve assistant/message artifacts for this session | Used for progress drilldown and fallback summary source |
| `<session_root>/output.md` | Per-session (`<program_id>/<session_id>`) | Store stable summary artifact for this session | Used on next run of same `program_id` for preview/prompt reference |
| `<session_root>/program.md` | Per-session (`<program_id>/<session_id>`) | Snapshot program text that produced this session | Used on next run of same `program_id` to compute program diff |
| `<session_root>/variables.json` | Per-session (`<program_id>/<session_id>`) | Snapshot resolved variables for this session | Used on next run of same `program_id` to compute variables diff |
| `<session_root>/outcome.json` | Per-session (`<program_id>/<session_id>`) | Store machine-readable session outcome with status/error/token/file counters | Inspection/audit; not currently a control input |
| `<history_index>` | Workspace-global append-only index | Store per-run summary metadata (status, summary_short, file/token stats) | Loaded at next run start for previous-run status/summary/stats in preview and prompt |

Compatibility behavior:

- No read fallback is used for `agent_summary.md`.
- Current apply reads only the current protocol files documented in this section.
- Sandbox auto-retry does not parse provider stdout/stderr for sandbox heuristics; it only trusts current-run `agent_result.json`.

Current limitation:

- `.clawform/agent_*.json|md` protocol files are workspace-global and not namespaced by `program_id/session_id`.
- This can conflict if two applies run concurrently in the same workspace.
- TODO: move current-run protocol files to per-session paths (for example: `<session_root>/protocol/agent_result.json`, `agent_outputs.json`, `agent_output.md`, `agent_variables.json`).

## 5.3 Data We Intentionally Do Not Persist

Current apply does not persist:

- `prompt.md`
- `plan.json`
- provider stdout/stderr artifact logs
- `events.ndjson` on non-debug runs

## 5.4 Agent Result Protocol Rules

Protocol file: `<protocol_root>/agent_result.json`

Expected shape:

```json
{
  "status": "success|partial|failure",
  "reason": "sandbox_blocked|program_blocked",
  "message": "short human-readable summary"
}
```

Rules:

1. `status` is required and strict enum: `success | partial | failure`.
2. `reason` is strict enum: `sandbox_blocked | program_blocked`.
3. `reason` is required for `partial` and `failure`; omitted for `success`.
4. Unknown `reason` values are rejected when Clawform parses `agent_result.json`.
5. In `workspace` and `auto` modes, runtime prompt enforces verdict gate semantics:
   - first restriction symptom triggers block-cause classification
   - any sandbox evidence (including non-fatal permission/network warnings), mixed evidence, or uncertainty => `reason: sandbox_blocked`
   - `reason: program_blocked` only when zero restriction symptoms appeared and one read-only check confirms an independent non-sandbox cause
   - no workaround/fallback commands before writing the verdict

## 5.5 Auto Sandbox Retry and Progress Output

Applies only when sandbox mode is `auto`:

1. First pass runs in `workspace` mode.
2. One retry in `full-access` mode is allowed only when current-run `agent_result.json` reports:
   - `status` in `partial|failure`
   - `reason: sandbox_blocked`
3. No retry is triggered from command-output text heuristics.
4. When retry is triggered, Clawform emits a retry-decision progress line and then launches one `full-access` attempt.

## 6) Known Bugs

### 6.1 Workspace-Global Protocol File Collisions Under Concurrent Applies

Steps to reproduce:

1. Start `cf -f <program-a.md>` in one terminal (same workspace).
2. While it is still running, start `cf -f <program-b.md>` in another terminal (same workspace).
3. Both runs write/read `.clawform/agent_result.json` / `.clawform/agent_outputs.json` / `.clawform/agent_output.md`.

Expected:

- Each run uses isolated protocol files for its own session.

Actual:

- Protocol files are shared workspace-global scratch paths and can overwrite each other.

## 7) Backlog (Out of Scope for now)

These items are intentionally deferred. Each item describes desired product capability, not implementation.

1. Memory support  
   Goal: support durable context across sessions with predictable usage rules.
2. Plan support  
   Goal: support planning as a first-class workflow, separate from execution.
3. Persisted provider trace artifacts  
   Goal: optionally persist canonical normalized provider events for audit/debug flows without requiring raw provider stdout parsing.
4. Changes/diff reliability and consistency  
   Goal: for the same session, preview, apply output, debug output, and history should report the same changed-file set and line counts, with generated/noise files handled consistently.
5. Agent-reported changes as single source of truth  
   Goal: remove local diff-based change reporting and use agent-reported change data consistently across apply, debug, and history.
6. MCP and broader tool integration model  
   Goal: support richer external tool and integration patterns.
7. Multi-agent orchestration model  
   Goal: support coordinated workflows that involve more than one agent.
8. Additional providers beyond Codex and Claude  
   Goal: support multiple model providers in a consistent user experience.
9. Improved session storage and retrieval performance  
   Goal: keep history/state operations fast and scalable as usage grows.
10. Session-scoped protocol files for concurrent apply safety  
    Goal: move `.clawform/agent_*.json|md` to per-session protocol paths.
11. Optional Claude `--bare` provider mode
    Goal: add a more deterministic no-ambient-context Claude launch mode as an explicit provider option without making it required for v1.
