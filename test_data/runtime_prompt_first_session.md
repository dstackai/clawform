Clawform apply session contract

Current session
- Program ID: `calculator`
- Program: `examples/calc.md`

Session context
- This program is being performed for the first time (no previous sessions).

What to do in this session
- Read and implement `examples/calc.md`.
- Use workspace files and tools as needed.
- Continue until the program result is correct, or stop only when there is no practical way forward.
- Verdict gate (required): after the first restriction symptom, stop normal task work and classify the block cause.
- Restriction symptom means any sandbox-denial, file-access/permission error, or network/connection restriction signal.
- Do not grade severity: even non-fatal warnings are sandbox evidence.
- After a restriction symptom, you may run at most one read-only check that does not require the blocked capability.
- If any sandbox evidence remains, or evidence is mixed/uncertain, write `./.clawform/agent_result.json` with `status: failure` and `reason: sandbox_blocked`, then stop.
- Use `reason: program_blocked` only when zero restriction symptoms appeared in failing required commands and one read-only check confirms an independent non-sandbox cause.
- Mandatory self-check before writing `program_blocked`: "Did any restriction symptom appear in this run?" If yes, change reason to `sandbox_blocked`.
- No workaround/fallback commands before verdict.
- Auto mode only: if blocked by sandbox, set `reason: sandbox_blocked`; this triggers one unsandboxed retry.
- Keep edits scoped to files needed for this program.
- Do not make unrelated edits.

Before finishing this session (required)
- Write `./.clawform/agent_outputs.json`.
- Write `./.clawform/agent_result.json`.

Required report files

1) `./.clawform/agent_outputs.json`

Exact format:
```json
[
  { "path": "relative/path.ext", "change": "created|modified|deleted" }
]
```

Rules:
- Include files created/modified/deleted in this session.
- Use repo-relative paths.
- Exclude `.clawform/*` bookkeeping files.
- Deduplicate entries.

2) `./.clawform/agent_result.json`

Exact format:
```json
{
  "status": "success|partial|failure",
  "reason": "sandbox_blocked|program_blocked",
  "message": "short human-readable summary"
}
```

Rules:
- `success`: program is complete and correct.
- `partial`: useful progress was made, but program is not complete.
- `failure`: program could not be completed.
- For `partial` or `failure`, set `reason`.
- For `success`, omit `reason`.
- Reason precedence: use `sandbox_blocked` if any restriction symptom appears in a failing required command, or evidence is mixed/uncertain; use `program_blocked` only when zero restriction symptoms appeared and an independent non-sandbox cause is confirmed.
- Write this verdict before any fallback/workaround/mutating commands.
- `message`: one short sentence about this session result.

User-facing message rule
- In user-facing text, describe program results only.
- Do not mention `.clawform/*` bookkeeping files unless explicitly asked.
