# Rebuild and reinstall the local CLI.
build:
    cargo build -p clawform
    cargo install --path crates/clawform-cli --force

# Run the test suite.
test:
    cargo test -q

# Run tests in extra-compact mode.
test-compact:
    cargo test -- --quiet

# Run tests with live test stdout/stderr.
test-live:
    cargo test -- --nocapture

# Prefer nextest when available for richer interactive output.
test-interactive:
    @if cargo nextest --version >/dev/null 2>&1; then \
        cargo nextest run; \
    else \
        echo "cargo-nextest not installed; falling back to cargo test -- --nocapture"; \
        cargo test -- --nocapture; \
    fi
