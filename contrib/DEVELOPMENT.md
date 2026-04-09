# Clawform Development

This project implements Clawform v0 in Rust.

## Prerequisites

1. Install Rust (stable) via `rustup`.
2. Install Codex CLI and ensure it is on `PATH`.
3. Verify Codex authentication:

```bash
codex login status
```

Expected for authenticated runs: a successful status output.

## Install From Releases (No Source Build)

Install latest stable release:

```bash
curl -fsSL https://raw.githubusercontent.com/dstackai/clawform/main/install.sh | sh
```

Install a pinned version (including pre-release tags):

```bash
CLAWFORM_VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/dstackai/clawform/main/install.sh | sh
CLAWFORM_VERSION=v0.2.0-rc.1 curl -fsSL https://raw.githubusercontent.com/dstackai/clawform/main/install.sh | sh
```

Notes:

- default install target is `~/.local/bin`
- installer verifies `SHA256SUMS` from GitHub Release artifacts
- `latest` resolves only latest stable GitHub release (pre-releases are opt-in via explicit version pinning)

## Build

From repository root:

```bash
cargo build
```

Build CLI binary explicitly:

```bash
cargo build -p clawform
```

## Install Locally

Install CLI binaries into your Cargo bin directory (`clawform` and `cf`):

```bash
cargo install --path crates/clawform-cli
```

If you already installed the same crate version and want to refresh from local changes without bumping version:

```bash
cargo install --path crates/clawform-cli --force
```

Verify install:

```bash
clawform --help
cf --help
```

If either command is not found, ensure Cargo bin is on your `PATH`:

```bash
. "$HOME/.cargo/env"
```

## Run Locally

Apply a program from repo root (progress on by default):

```bash
cargo run -p clawform -- apply -f examples/smoke.md
```

Installed binary equivalents:

```bash
clawform apply -f examples/smoke.md
cf apply -f examples/smoke.md
```

Interactive progress UI is enabled automatically only when stdin/stdout are attached to an interactive terminal.

Show provider raw logs (debug mode):

```bash
cargo run -p clawform -- apply -f examples/smoke.md --debug
```

Disable progress rendering entirely:

```bash
cargo run -p clawform -- apply -f examples/smoke.md --progress off
```

Force plain (non-interactive) progress output even in a TTY:

```bash
cargo run -p clawform -- apply -f examples/smoke.md --progress plain
```

Intermediate progress steps (read/search/text/turn details) are enabled by default.

Hide intermediate progress steps:

```bash
cargo run -p clawform -- apply -f examples/smoke.md --quiet
```

Ignore prior run history context for a fresh apply:

```bash
cargo run -p clawform -- apply -f examples/smoke.md --reset
```

Control sandbox policy for model-generated shell commands:

```bash
# default behavior (auto escalation when needed)
cargo run -p clawform -- apply -f examples/smoke.md --sandbox auto

# force sandboxed execution
cargo run -p clawform -- apply -f examples/smoke.md --sandbox workspace-write

# force unsandboxed execution
cargo run -p clawform -- apply -f examples/smoke.md --sandbox danger-full-access
```

Simple sandbox behavior check:

```bash
# force sandboxed mode (network fetch may fail in restricted environments)
cargo run -p clawform -- apply -f examples/sandbox-check.md --sandbox workspace-write --yes

# auto mode may escalate and complete with NETWORK_OK
cargo run -p clawform -- apply -f examples/sandbox-check.md --sandbox auto --yes
cat example-data/output-sandbox-check.txt
```

Skip confirmation prompt:

```bash
cargo run -p clawform -- apply -f examples/smoke.md --yes
```

Pass program variables at apply time (repeat `--var` as needed):

```bash
# uses smoke frontmatter default (SMOKE_OK)
cargo run -p clawform -- apply -f examples/smoke.md --yes

# smoke has default SMOKE_OK, and this overrides it for one run
cargo run -p clawform -- apply -f examples/smoke.md --var SMOKE_VALUE=YU --yes
```

Notes:

- Variables are defined in program frontmatter under `variables`.
- Required variable syntax (no default): `NAME: {}`.
- Optional variable syntax with default: `NAME: { default: "value" }`.
- Program body references variables via `${{ var.NAME }}`.
- Confirmation preview includes a variable-diff summary against last session when available.
- Runtime resolved values are written to `.clawform/agent_variables.json` for the agent.
- Successful sessions persist a snapshot at `.clawform/programs/<program_id>/sessions/<session_id>/variables.json`.
- If a required variable is missing at apply time, apply fails before provider execution.

Reset session history:

```bash
# delete history for one program (interactive confirm in TTY)
cargo run -p clawform -- reset --program smoke

# delete history for one program without prompt
cargo run -p clawform -- reset --program smoke --yes

# delete history for all programs
cargo run -p clawform -- reset --all
```

Installed binary equivalents:

```bash
clawform reset --program smoke
cf reset --all --yes
```

## Test

Run deterministic unit + mock integration tests:

```bash
cargo test
```

Run only core integration tests:

```bash
cargo test -p clawform-core --test apply_mock
```

## Real Codex Integration Tests (Opt-in)

Real provider tests are in `crates/clawform-cli/tests/codex_e2e.rs` and are skipped unless explicitly enabled.

Enable and run:

```bash
CLAWFORM_E2E_CODEX=1 cargo test -p clawform --test codex_e2e -- --test-threads=1
```

Notes:

- These tests require valid Codex auth (`codex login status`).
- These tests require DNS/connectivity to `api.openai.com`.
- They may consume API credits and run slower/flakier than mock tests.
- Keep them opt-in locally and in CI.

## Release Automation

GitHub release workflow:

- file: `.github/workflows/release.yml`
- trigger: push tag matching `v*`
- outputs:
  - `clawform_linux_x86_64.tar.gz`
  - `clawform_darwin_x86_64.tar.gz`
  - `clawform_darwin_aarch64.tar.gz`
  - `SHA256SUMS`

Release type:

- tags without `-` become stable releases (for example `v0.3.0`)
- tags with `-` become pre-releases (for example `v0.3.0-rc.1`)

## Troubleshooting

1. `rustc: command not found`
- Install Rust via `rustup` and open a new shell.

2. `codex: command not found`
- Install Codex CLI and ensure its install location is on `PATH`.

3. `codex login status` fails
- Authenticate first, then rerun tests/commands.

4. Apply fails with provider execution error
- Rerun `codex login status`, then retry apply.
- Clawform performs DNS preflight for `api.openai.com`; fix DNS/VPN/proxy first if it fails early.
- Check stderr output from `clawform apply` for model/auth/runtime failures.
- During long runs, Clawform prints live progress events and periodic heartbeat lines.
- In v0, Clawform does not enforce its own max runtime timeout; provider behavior determines run duration.

5. Unexpected file writes after apply
- v0 treats markdown I/O as agent-interpreted and runs directly in the current workspace.
- Tighten the markdown instruction text if the agent is writing too broadly.
