use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serial_test::serial;

fn should_run_e2e() -> bool {
    std::env::var("CLAUDEFORM_E2E_CODEX")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn ensure_codex_auth() -> Result<()> {
    let out = Command::new("codex")
        .args(["login", "status"])
        .output()
        .context("failed to run 'codex login status'")?;

    if !out.status.success() {
        bail!(
            "codex login status failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    Ok(())
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    Ok(())
}

fn make_workspace(program_name: &str, program_body: &str) -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    write(
        &dir.path().join(".claudeform/config.json"),
        r#"{
  "claudeform": {
    "providers": {
      "codex": {
        "type": "codex",
        "default": true,
        "default_model": "gpt-5-codex"
      }
    }
  }
}"#,
    )?;
    write(&dir.path().join(program_name), program_body)?;
    Ok(dir)
}

fn run_apply(dir: &Path, file: &str) -> Result<std::process::Output> {
    let bin = env!("CARGO_BIN_EXE_claudeform");
    let output = Command::new(bin)
        .args(["apply", "-f", file])
        .current_dir(dir)
        .output()
        .context("failed running claudeform apply")?;
    Ok(output)
}

#[test]
#[serial]
fn codex_smoke_apply_creates_output() -> Result<()> {
    if !should_run_e2e() {
        eprintln!("skipping codex e2e: set CLAUDEFORM_E2E_CODEX=1 to run");
        return Ok(());
    }

    ensure_codex_auth()?;

    let ws = make_workspace(
        "smoke.md",
        r#"
---
id: e2e_smoke
---
## Instruction
Write exactly one file `./example-data/output-smoke.txt` with content `SMOKE_OK` and trailing newline.
"#,
    )?;

    let out = run_apply(ws.path(), "smoke.md")?;
    if !out.status.success() {
        bail!(
            "claudeform apply failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let content = fs::read_to_string(ws.path().join("example-data/output-smoke.txt"))?;
    assert_eq!(content.trim(), "SMOKE_OK");
    Ok(())
}

#[test]
#[serial]
fn codex_second_apply_runs_and_prints_files_summary() -> Result<()> {
    if !should_run_e2e() {
        eprintln!("skipping codex e2e: set CLAUDEFORM_E2E_CODEX=1 to run");
        return Ok(());
    }

    ensure_codex_auth()?;

    let ws = make_workspace(
        "smoke.md",
        r#"
---
id: e2e_smoke_second
---
## Instruction
Write exactly one file `./example-data/output-smoke.txt` with content `SMOKE_OK` and trailing newline.
"#,
    )?;

    let first = run_apply(ws.path(), "smoke.md")?;
    if !first.status.success() {
        bail!("first apply failed");
    }

    let second = run_apply(ws.path(), "smoke.md")?;
    if !second.status.success() {
        bail!("second apply failed");
    }

    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(!stdout.contains("Action:"));
    assert!(stdout.contains("output-smoke.txt"));
    assert!(stdout.contains("tokens:"));
    Ok(())
}

#[test]
#[serial]
fn codex_apply_can_create_multiple_files() -> Result<()> {
    if !should_run_e2e() {
        eprintln!("skipping codex e2e: set CLAUDEFORM_E2E_CODEX=1 to run");
        return Ok(());
    }

    ensure_codex_auth()?;

    let ws = make_workspace(
        "multi.md",
        r#"
---
id: e2e_multi
---
## Instruction
Write `./declared.txt` with text `DECLARED_OK` and write `./extra.txt` with text `EXTRA_OK`.
"#,
    )?;

    let out = run_apply(ws.path(), "multi.md")?;
    if !out.status.success() {
        bail!("apply failed for multi-file test");
    }

    assert!(ws.path().join("declared.txt").exists());
    assert!(ws.path().join("extra.txt").exists());
    Ok(())
}
