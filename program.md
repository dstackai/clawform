---
id: provider-search-trace
---

# Provider Search Tool Probe

Use the provider's built-in web or search capability at least once before you answer.

Requirements:

- You must attempt a built-in search or web tool call.
- Do not use Bash, `curl`, `wget`, Python, or any other direct network workaround instead of the built-in search or web tool.
- Do not answer from memory before attempting the built-in search or web tool.
- If no built-in search or web tool is available, say so explicitly instead of working around it.

Use this stable query:

- `Example Domain example.com`

Final answer format:

- `search_attempted: yes|no`
- `search_available: yes|no`
- `finding: <one short line>`
