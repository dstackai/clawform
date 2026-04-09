---
id: sandbox-check
---

## Outputs

Expected output file:
- `./example-data/output-sandbox-check.txt`

## Instruction

This program is for sandbox behavior verification.

1. Fetch `https://example.com` using a shell network command (`curl` or `wget`).
2. The fetch command must succeed.
3. After successful fetch, write `./example-data/output-sandbox-check.txt` with exactly:
`NETWORK_OK`

Rules:

- write only `./example-data/output-sandbox-check.txt` (plus required `.clawform` report files)
- do not write `NETWORK_OK` unless the fetch command succeeded
- output file must be exactly one line with trailing newline
