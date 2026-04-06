---
variables:
  SMOKE_VALUE:
    default: "SMOKE_OK"
---

## Outputs

Expected output file:
- `./example-data/output-smoke.txt`

## Instruction

This is a smoke test for Claudeform apply.

Write exactly one file: `./example-data/output-smoke.txt`.
File content must be exactly:
`${{ var.SMOKE_VALUE }}`

Rules:

- content must be a single line with trailing newline
- do not write any other files
