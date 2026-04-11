---
id: claude-search-trace
---

# Claude Search Trace Check

Use Claude's built-in search or web tooling before you answer.

Requirements:

- You must attempt a built-in search or web tool call at least once.
- Prefer `ToolSearch` and then `WebSearch` or `WebFetch` if available.
- Do not use Bash, `curl`, `wget`, Python, or any other direct network workaround instead of the built-in search or web tool.
- Do not answer from memory before attempting the built-in search or web tool.
- If the built-in search or web tool is unavailable, say so explicitly instead of working around it.

Use this exact search query:

- `Example Domain example.com`

Keep the final answer short and use exactly these lines:

- `search_attempted: yes|no`
- `search_available: yes|no`
- `finding: <one short line>`
