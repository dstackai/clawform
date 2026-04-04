use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tempfile::TempDir;

use claudeform_core::provider::{ProviderRequest, ProviderRunResult, ProviderRunner};
use claudeform_core::{run_apply, AgentStatus, ApplyRequest, RunHistoryRecord, RunStatus};

const AGENT_RESULT_SUCCESS_JSON: &str = r#"{"status":"success","message":"done"}"#;
const AGENT_RESULT_PARTIAL_JSON: &str =
    r#"{"status":"partial","message":"could not run tests in this environment"}"#;

#[derive(Clone)]
struct MockRunner {
    writes: Vec<(PathBuf, &'static str)>,
    calls: Arc<AtomicUsize>,
    fail: bool,
    prompts: Arc<Mutex<Vec<String>>>,
}

impl ProviderRunner for MockRunner {
    fn run(&self, request: &ProviderRequest) -> Result<ProviderRunResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prompts
            .lock()
            .expect("prompt mutex poisoned")
            .push(request.prompt.clone());

        if self.fail {
            return Ok(ProviderRunResult {
                session_id: Some("mock-session-fail".to_string()),
                exit_code: Some(1),
                stdout: String::new(),
                stderr: "forced failure".to_string(),
                usage: Default::default(),
            });
        }

        for (rel, content) in &self.writes {
            let abs = request.workspace_root.join(rel);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(abs, content)?;
        }

        Ok(ProviderRunResult {
            session_id: Some("mock-session-ok".to_string()),
            exit_code: Some(0),
            stdout: "mock ok".to_string(),
            stderr: String::new(),
            usage: Default::default(),
        })
    }
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn setup_workspace(program_markdown: &str) -> Result<TempDir> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();

    write_file(
        &root.join(".claudeform/config.json"),
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

    write_file(&root.join("program.md"), program_markdown)?;
    Ok(temp)
}

fn make_runner(writes: Vec<(PathBuf, &'static str)>, fail: bool) -> (MockRunner, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        MockRunner {
            writes,
            calls: calls.clone(),
            fail,
            prompts: Arc::new(Mutex::new(Vec::new())),
        },
        calls,
    )
}

fn run(
    ws: &TempDir,
    runner: &MockRunner,
    use_history_context: bool,
) -> Result<claudeform_core::ApplyResult> {
    run_apply(
        &ApplyRequest {
            workspace_root: ws.path().to_path_buf(),
            program_path: ws.path().join("program.md"),
            confirm: false,
            debug: false,
            progress: false,
            interactive_ui: false,
            show_intermediate_steps: false,
            use_history_context,
        },
        runner,
    )
}

#[test]
fn first_apply_creates_history_and_output() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
---
## Instruction
Write ./out.txt.
"#,
    )?;
    let (runner, calls) = make_runner(
        vec![
            (PathBuf::from("out.txt"), "OK\n"),
            (
                PathBuf::from(".claudeform/agent_result.json"),
                AGENT_RESULT_SUCCESS_JSON,
            ),
        ],
        false,
    );

    let result = run(&ws, &runner, false)?;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fs::read_to_string(ws.path().join("out.txt"))?, "OK\n");
    assert!(!result.history_injected_success);
    assert!(!result.history_injected_failure);
    assert!(result.prompt_artifact.is_some());
    assert!(result.plan_artifact.is_some());
    assert!(result.provider_stdout_artifact.is_some());
    assert!(result.provider_stderr_artifact.is_some());
    assert!(result.events_artifact.is_none());
    let prompt_artifact = result
        .prompt_artifact
        .as_deref()
        .context("missing prompt artifact")?;
    let plan_artifact = result
        .plan_artifact
        .as_deref()
        .context("missing plan artifact")?;
    let stdout_artifact = result
        .provider_stdout_artifact
        .as_deref()
        .context("missing provider stdout artifact")?;
    let stderr_artifact = result
        .provider_stderr_artifact
        .as_deref()
        .context("missing provider stderr artifact")?;
    assert!(ws.path().join(prompt_artifact).exists());
    assert!(ws.path().join(plan_artifact).exists());
    assert!(ws.path().join(stdout_artifact).exists());
    assert!(ws.path().join(stderr_artifact).exists());

    let history = read_history(ws.path())?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, RunStatus::Success);
    assert_eq!(history[0].program_id, "mock_program");
    Ok(())
}

#[test]
fn reads_agent_human_output_and_excludes_internal_output_files() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
---
## Instruction
Write ./out.txt.
"#,
    )?;
    let (runner, _) = make_runner(
        vec![
            (PathBuf::from("out.txt"), "OK\n"),
            (
                PathBuf::from(".claudeform/agent_output.md"),
                "Created out.txt with OK\n",
            ),
            (
                PathBuf::from(".claudeform/agent_outputs.json"),
                "[\"out.txt\", \".claudeform/agent_output.md\"]\n",
            ),
            (
                PathBuf::from(".claudeform/agent_result.json"),
                AGENT_RESULT_SUCCESS_JSON,
            ),
        ],
        false,
    );

    let result = run(&ws, &runner, false)?;
    assert_eq!(
        result.agent_human_summary.as_deref(),
        Some("Created out.txt with OK")
    );
    assert!(result.file_results.iter().any(|f| f.path == "out.txt"));
    assert!(!result
        .file_results
        .iter()
        .any(|f| f.path == ".claudeform/agent_output.md"));
    assert!(!result
        .file_results
        .iter()
        .any(|f| f.path == ".claudeform/agent_summary.md"));
    Ok(())
}

#[test]
fn reads_legacy_agent_summary_path_for_backward_compatibility() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
---
## Instruction
Write ./out.txt.
"#,
    )?;
    let (runner, _) = make_runner(
        vec![
            (PathBuf::from("out.txt"), "OK\n"),
            (
                PathBuf::from(".claudeform/agent_summary.md"),
                "Legacy summary path still works\n",
            ),
            (
                PathBuf::from(".claudeform/agent_result.json"),
                AGENT_RESULT_SUCCESS_JSON,
            ),
        ],
        false,
    );

    let result = run(&ws, &runner, false)?;
    assert_eq!(
        result.agent_human_summary.as_deref(),
        Some("Legacy summary path still works")
    );
    Ok(())
}

#[test]
fn reads_agent_result_status_without_affecting_system_success() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
---
## Instruction
Write ./out.txt.
"#,
    )?;
    let (runner, _) = make_runner(
        vec![
            (PathBuf::from("out.txt"), "OK\n"),
            (
                PathBuf::from(".claudeform/agent_result.json"),
                AGENT_RESULT_PARTIAL_JSON,
            ),
        ],
        false,
    );

    let result = run(&ws, &runner, false)?;
    assert_eq!(
        result.agent_result.as_ref().map(|r| &r.status),
        Some(&AgentStatus::Partial)
    );
    assert_eq!(
        result
            .agent_result
            .as_ref()
            .and_then(|r| r.message.as_deref()),
        Some("could not run tests in this environment")
    );
    assert!(!result
        .file_results
        .iter()
        .any(|f| f.path == ".claudeform/agent_result.json"));
    let history = read_history(ws.path())?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, RunStatus::Success);
    Ok(())
}

#[test]
fn provider_failure_appends_failure_history_record() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
---
## Instruction
Write ./out.txt.
"#,
    )?;
    let (ok_runner, _) = make_runner(
        vec![
            (PathBuf::from("out.txt"), "V1\n"),
            (
                PathBuf::from(".claudeform/agent_result.json"),
                AGENT_RESULT_SUCCESS_JSON,
            ),
        ],
        false,
    );
    run(&ws, &ok_runner, false)?;

    let (failing_runner, _) = make_runner(Vec::new(), true);
    let err = run(&ws, &failing_runner, false)
        .err()
        .context("expected apply failure")?;
    assert!(format!("{:#}", err).contains("provider execution failed"));

    let history = read_history(ws.path())?;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].status, RunStatus::Success);
    assert_eq!(history[1].status, RunStatus::Failure);
    Ok(())
}

#[test]
fn history_context_is_injected_when_enabled() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
---
## Instruction
Write ./out.txt.
"#,
    )?;
    let (runner1, _) = make_runner(
        vec![
            (PathBuf::from("out.txt"), "OK\n"),
            (
                PathBuf::from(".claudeform/agent_output.md"),
                "first summary\n",
            ),
            (
                PathBuf::from(".claudeform/agent_result.json"),
                AGENT_RESULT_SUCCESS_JSON,
            ),
        ],
        false,
    );
    run(&ws, &runner1, false)?;

    let (runner2, _) = make_runner(
        vec![
            (PathBuf::from("out.txt"), "OK\n"),
            (
                PathBuf::from(".claudeform/agent_result.json"),
                AGENT_RESULT_SUCCESS_JSON,
            ),
        ],
        false,
    );
    let result = run(&ws, &runner2, true)?;
    assert!(result.history_injected_success);

    let prompts = runner2.prompts.lock().expect("prompt mutex poisoned");
    let last = prompts.last().cloned().unwrap_or_default();
    assert!(last.contains("Claudeform apply session contract"));
    assert!(last.contains("Last session details"));
    assert!(last.contains("Program changes since last session"));
    assert!(last.contains(".claudeform/programs/mock_program/sessions/mock-session-ok/program.md"));
    Ok(())
}

#[test]
fn repro_changed_program_without_agent_result_now_fails() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
---
## Task
Write ./out.txt with `v1`.
"#,
    )?;

    // Baseline success run to establish "last_success" snapshot/history.
    let (runner1, _) = make_runner(
        vec![
            (PathBuf::from("out.txt"), "v1\n"),
            (
                PathBuf::from(".claudeform/agent_result.json"),
                AGENT_RESULT_SUCCESS_JSON,
            ),
        ],
        false,
    );
    run(&ws, &runner1, true)?;

    // Change the program significantly (mirrors the "program diff changed" case).
    write_file(
        &ws.path().join("program.md"),
        r#"---
id: mock_program
---
## Task
Update ./out.txt with `v2` and verify it.

## Requirements
- Adapt implementation to this updated instruction.
- Confirm completion with concrete evidence.
"#,
    )?;

    // Repro runner: returns success but performs no file writes.
    let (runner2, _) = make_runner(Vec::new(), false);
    let err = run(&ws, &runner2, true)
        .err()
        .context("expected apply to fail without agent_result")?;
    assert!(format!("{:#}", err).contains(".claudeform/agent_result.json"));

    let history = read_history(ws.path())?;
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].status, RunStatus::Failure);
    assert_eq!(history[1].files_total, 0);

    let prompts = runner2.prompts.lock().expect("prompt mutex poisoned");
    let last = prompts.last().cloned().unwrap_or_default();
    assert!(last.contains("Program changes since last session"));
    assert!(last.contains("Program change summary:"));
    assert!(!last.contains("Claudeform plan context"));
    Ok(())
}

fn read_history(workspace_root: &Path) -> Result<Vec<RunHistoryRecord>> {
    let path = workspace_root.join(".claudeform/history/index.jsonl");
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed reading history file '{}'", path.display()))?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(
            serde_json::from_str::<RunHistoryRecord>(trimmed)
                .with_context(|| format!("invalid history line '{}'", trimmed))?,
        );
    }
    Ok(out)
}
