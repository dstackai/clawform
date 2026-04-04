# Rebuild and reinstall the local CLI.
build:
    cargo build -p claudeform
    cargo install --path crates/claudeform-cli --force
