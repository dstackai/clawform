Claudeform apply session contract

Current session
- Program ID: `calculator`
- Program: `examples/calc.md`

Session context
- This program is being performed for the first time (no previous sessions).

What to do in this session
- Read and implement `examples/calc.md`.
- Use workspace files and tools as needed.
- Continue until the program result is correct, or stop only when there is no practical way forward.
- If a required network command fails in sandbox with DNS/connectivity errors (for example: `Could not resolve host`, `failed to lookup address information`, `network is unreachable`, `no route to host`, `connection timed out`), treat it as sandbox network restriction immediately.
- In that case, do not spend time on network workarounds (forced IP/`--resolve`, alternate download tools, local TLS/proxy emulation).
- Immediately write `./.claudeform/agent_result.json` with `status: failure` and `reason: sandbox_network_blocked`, then stop.
- Keep edits scoped to files needed for this program.
- Do not make unrelated edits.

Before finishing this session (required)
- Write `./.claudeform/agent_outputs.json`.
- Write `./.claudeform/agent_result.json`.

Required report files

1) `./.claudeform/agent_outputs.json`

Exact format:
```json
[
  { "path": "relative/path.ext", "change": "created|modified|deleted" }
]
```

Rules:
- Include files created/modified/deleted in this session.
- Use repo-relative paths.
- Exclude `.claudeform/*` bookkeeping files.
- Deduplicate entries.

2) `./.claudeform/agent_result.json`

Exact format:
```json
{
  "status": "success|partial|failure",
  "reason": "optional machine-readable reason",
  "message": "short human-readable summary"
}
```

Rules:
- `success`: program is complete and correct.
- `partial`: useful progress was made, but program is not complete.
- `failure`: program could not be completed.
- If blocked by sandbox network restrictions, set `reason` to exactly `sandbox_network_blocked`.
- Omit `reason` when not needed.
- `message`: one short sentence about this session result.

User-facing message rule
- In user-facing text, describe program results only.
- Do not mention `.claudeform/*` bookkeeping files unless explicitly asked.
