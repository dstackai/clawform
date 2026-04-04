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
  "message": "short human-readable summary"
}
```

Rules:
- `success`: program is complete and correct.
- `partial`: useful progress was made, but program is not complete.
- `failure`: program could not be completed.
- `message`: one short sentence about this session result.

User-facing message rule
- In user-facing text, describe program results only.
- Do not mention `.claudeform/*` bookkeeping files unless explicitly asked.
