Clawform apply session contract

You are running the "current session".

Fixed terms used in this prompt:
- "program": the new program version for this session, stored at `examples/calc.md`
- "current session": this session
- "last session": the most recent finished session for this program

What is expected in this "current session":
- Complete the "program".
- Use files and tools in the workspace as needed to complete the "program".
- Use "last session" details to understand what was already done.
- Keep correct work from "last session"; do not redo work without a clear reason.
- If program changes require updates, apply only the updates required by those changes.
- If verification shows issues, fix them in this "current session".
- Continue until the program result is correct, or stop only when there is no practical way forward.
- You may change workspace files, but only files needed to complete the "program".

Required execution order:
1) Read the new program version: `examples/calc.md`.
2) Read "last session" files:
   `.clawform/programs/calculator/sessions/019d55f0-fd15-7041-bca3-979c467b67eb/program.md`
   and
   `.clawform/programs/calculator/sessions/019d55f0-fd15-7041-bca3-979c467b67eb/output.md`.
3) Read program changes between:
   `.clawform/programs/calculator/sessions/019d55f0-fd15-7041-bca3-979c467b67eb/program.md`
   and
   the new program version (`examples/calc.md`).
4) Execute the "program" for this "current session".
5) Before finishing, write both required report files:
   `./.clawform/agent_outputs.json`
   and
   `./.clawform/agent_result.json`.

Program

- Program ID: `calculator`
- New program version: `examples/calc.md`

---

Last session details

- last_session_id: `019d55f0-fd15-7041-bca3-979c467b67eb`
- last_session_status: `success`
- last_session_time_unix: `1775263601`
- last_session_program_file: `.clawform/programs/calculator/sessions/019d55f0-fd15-7041-bca3-979c467b67eb/program.md`
- last_session_output_file: `.clawform/programs/calculator/sessions/019d55f0-fd15-7041-bca3-979c467b67eb/output.md`
- session_history_path (open only if needed): `.clawform/programs/calculator/sessions/`

How to use "last session" details in this "current session":
- Understand what was completed in "last session".
- Verify whether that result is still correct for the "program".
- If "last session" work is still correct and program changes do not require more edits, keep that work.
- If "last session" work is incorrect or incomplete for the "program", update it.

---

Program changes since last session

- Last session program file to compare from:
  `.clawform/programs/calculator/sessions/019d55f0-fd15-7041-bca3-979c467b67eb/program.md`
- Program file for the "current session" to compare to:
  `examples/calc.md`
- Program change summary:
  `6 lines changed, 0 added, 24 deleted`

How to apply program changes in this "current session":
- Treat the new program version as what you must implement now.
- Use the program change summary in this prompt to understand what changed since "last session".
- Apply only the edits needed to satisfy the changed program.
- If no meaningful program change exists, first verify the result is still correct; only edit files if verification finds a real gap.

---

Execution and stop rules for this "current session"

- Keep working until the new program version is satisfied.
- Stop only if there is no practical way to complete the "program" in this environment.
- If blocked, report that in the required status file.
- Verdict gate (required): after the first restriction symptom, stop normal task work and classify the block cause.
- Restriction symptom means any sandbox-denial, file-access/permission error, or network/connection restriction signal.
- Do not grade severity: even non-fatal warnings are sandbox evidence.
- After a restriction symptom, you may run at most one read-only check that does not require the blocked capability.
- If any sandbox evidence remains, or evidence is mixed/uncertain, write `./.clawform/agent_result.json` with `status: failure` and `reason: sandbox_blocked`, then stop.
- Use `reason: program_blocked` only when zero restriction symptoms appeared in failing required commands and one read-only check confirms an independent non-sandbox cause.
- Mandatory self-check before writing `program_blocked`: "Did any restriction symptom appear in this run?" If yes, change reason to `sandbox_blocked`.
- No workaround/fallback commands before verdict.
- Auto mode only: if blocked by sandbox, set `reason: sandbox_blocked`; this triggers one unsandboxed retry.
- Keep edits within program scope:
  files required to satisfy the "program".
- Do not make unrelated edits.

---

Required report files for this "current session" (must write both)

1) `./.clawform/agent_outputs.json`

Exact format:
```json
[
  { "path": "relative/path.ext", "change": "created|modified|deleted" }
]
```

Rules:
- Include files created/modified/deleted in this "current session".
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
- `success`: the "program" is complete and correct.
- `partial`: useful progress was made, but program is not complete.
- `failure`: program could not be completed.
- For `partial` or `failure`, set `reason`.
- For `success`, omit `reason`.
- Reason precedence: use `sandbox_blocked` if any restriction symptom appears in a failing required command, or evidence is mixed/uncertain; use `program_blocked` only when zero restriction symptoms appeared and an independent non-sandbox cause is confirmed.
- Write this verdict before any fallback/workaround/mutating commands.
- `message`: one short sentence about this "current session" result.

---

User-facing message rule for this "current session"

- In user-facing text, describe program results only.
- Do not mention `.clawform/*` bookkeeping files unless user explicitly asks.
