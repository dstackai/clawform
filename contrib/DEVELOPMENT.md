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

Disable live progress events:

```bash
cargo run -p claudeform -- apply -f examples/smoke.md --no-progress
```

Force plain (non-interactive) progress output even in a TTY:

```bash
cargo run -p claudeform -- apply -f examples/smoke.md --no-interactive
```

Intermediate progress steps (read/search/text/turn details) are enabled by default.

Hide intermediate progress steps:

```bash
cargo run -p claudeform -- apply -f examples/smoke.md --no-intermediate
```

Skip confirmation prompt:

```bash
cargo run -p claudeform -- apply -f examples/smoke.md --yes
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
