# Claudeform Development

This project implements Claudeform v0 in Rust.

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
curl -fsSL https://raw.githubusercontent.com/dstackai/claudeform/main/install.sh | sh
```

Install a pinned version (including pre-release tags):

```bash
CLAUDEFORM_VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/dstackai/claudeform/main/install.sh | sh
CLAUDEFORM_VERSION=v0.2.0-rc.1 curl -fsSL https://raw.githubusercontent.com/dstackai/claudeform/main/install.sh | sh
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
cargo build -p claudeform
```

## Install Locally

Install CLI binaries into your Cargo bin directory (`claudeform` and `cf`):

```bash
cargo install --path crates/claudeform-cli
```

If you already installed the same crate version and want to refresh from local changes without bumping version:

```bash
cargo install --path crates/claudeform-cli --force
```

Verify install:

```bash
claudeform --help
cf --help
```

If either command is not found, ensure Cargo bin is on your `PATH`:

```bash
. "$HOME/.cargo/env"
```

## Run Locally

Apply a program from repo root (progress on by default):

```bash
cargo run -p claudeform -- apply -f examples/smoke.md
```

Installed binary equivalents:

```bash
claudeform apply -f examples/smoke.md
cf apply -f examples/smoke.md
```

Interactive progress UI is enabled automatically only when stdin/stdout are attached to an interactive terminal.

Show provider raw logs (debug mode):

```bash
cargo run -p claudeform -- apply -f examples/smoke.md --debug
```

Disable progress rendering entirely:

```bash
cargo run -p claudeform -- apply -f examples/smoke.md --progress off
```

Force plain (non-interactive) progress output even in a TTY:

```bash
cargo run -p claudeform -- apply -f examples/smoke.md --progress plain
```

Intermediate progress steps (read/search/text/turn details) are enabled by default.

Hide intermediate progress steps:

```bash
cargo run -p claudeform -- apply -f examples/smoke.md --quiet
```

Ignore prior run history context for a fresh apply:

```bash
cargo run -p claudeform -- apply -f examples/smoke.md --reset
```

Control sandbox policy for model-generated shell commands:

```bash
# default behavior (auto escalation when needed)
cargo run -p claudeform -- apply -f examples/smoke.md --sandbox auto

# force sandboxed execution
cargo run -p claudeform -- apply -f examples/smoke.md --sandbox workspace-write

# force unsandboxed execution
cargo run -p claudeform -- apply -f examples/smoke.md --sandbox danger-full-access
```

Simple sandbox behavior check:

```bash
# force sandboxed mode (network fetch may fail in restricted environments)
cargo run -p claudeform -- apply -f examples/sandbox-check.md --sandbox workspace-write --yes

# auto mode may escalate and complete with NETWORK_OK
cargo run -p claudeform -- apply -f examples/sandbox-check.md --sandbox auto --yes
cat example-data/output-sandbox-check.txt
```

Skip confirmation prompt:

```bash
cargo run -p claudeform -- apply -f examples/smoke.md --yes
```

Reset session history:

```bash
# delete history for one program (interactive confirm in TTY)
cargo run -p claudeform -- reset --program smoke

# delete history for one program without prompt
cargo run -p claudeform -- reset --program smoke --yes

# delete history for all programs
cargo run -p claudeform -- reset --all
```

Installed binary equivalents:

```bash
claudeform reset --program smoke
cf reset --all --yes
```

## Test

Run deterministic unit + mock integration tests:

```bash
cargo test
```

Run only core integration tests:

```bash
cargo test -p claudeform-core --test apply_mock
```

## Real Codex Integration Tests (Opt-in)

Real provider tests are in `crates/claudeform-cli/tests/codex_e2e.rs` and are skipped unless explicitly enabled.

Enable and run:

```bash
CLAUDEFORM_E2E_CODEX=1 cargo test -p claudeform --test codex_e2e -- --test-threads=1
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
  - `claudeform_linux_x86_64.tar.gz`
  - `claudeform_darwin_x86_64.tar.gz`
  - `claudeform_darwin_aarch64.tar.gz`
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
- Claudeform performs DNS preflight for `api.openai.com`; fix DNS/VPN/proxy first if it fails early.
- Check stderr output from `claudeform apply` for model/auth/runtime failures.
- During long runs, Claudeform prints live progress events and periodic heartbeat lines.
- In v0, Claudeform does not enforce its own max runtime timeout; provider behavior determines run duration.

5. Unexpected file writes after apply
- v0 treats markdown I/O as agent-interpreted and runs directly in the current workspace.
- Tighten the markdown instruction text if the agent is writing too broadly.
