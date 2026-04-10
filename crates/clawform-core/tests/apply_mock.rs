use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tempfile::TempDir;

use clawform_core::provider::{ProviderRequest, ProviderRunResult, ProviderRunner};
use clawform_core::{run_apply, ApplyRequest, RunHistoryRecord, RunStatus, SandboxMode};

const AGENT_RESULT_SUCCESS_JSON: &str = r#"{"status":"success","message":"done"}"#;
const AGENT_RESULT_PARTIAL_JSON: &str = r#"{"status":"partial","reason":"program_blocked","message":"could not run tests in this environment"}"#;

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
                turn_count: 0,
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
            turn_count: 0,
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
        &root.join(".clawform/config.json"),
        r#"{
  "clawform": {
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
) -> Result<clawform_core::ApplyResult> {
    run_with_variables(ws, runner, use_history_context, BTreeMap::new())
}

fn run_with_variables(
    ws: &TempDir,
    runner: &MockRunner,
    use_history_context: bool,
    program_variables: BTreeMap<String, String>,
) -> Result<clawform_core::ApplyResult> {
    run_apply(
        &ApplyRequest {
            workspace_root: ws.path().to_path_buf(),
            program_path: ws.path().join("program.md"),
            program_variables,
            confirm: false,
            debug: false,
            verbose_output: false,
            progress: false,
            render_progress: false,
            interactive_ui: false,
            show_intermediate_steps: false,
            use_history_context,
            sandbox_mode: SandboxMode::Auto,
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
                PathBuf::from(".clawform/agent_result.json"),
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
    assert!(result.prompt_artifact.is_none());
    assert!(result.plan_artifact.is_none());
    assert!(result.provider_stdout_artifact.is_none());
    assert!(result.provider_stderr_artifact.is_none());
    assert!(result.events_artifact.is_none());

    let history = read_history(ws.path())?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, RunStatus::Success);
    assert_eq!(history[0].program_id, "mock_program");
    Ok(())
}

#[test]
fn apply_with_program_variables_persists_variables_snapshot() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
variables:
  APP_NAME: {}
  APP_PORT:
    default: "8080"
---
## Instruction
Write ./out.txt using ${{ var.APP_NAME }} and ${{ var.APP_PORT }}.
"#,
    )?;
    let (runner, _) = make_runner(
        vec![
            (PathBuf::from("out.txt"), "OK\n"),
            (
                PathBuf::from(".clawform/agent_result.json"),
                AGENT_RESULT_SUCCESS_JSON,
            ),
        ],
        false,
    );

    run_with_variables(
        &ws,
        &runner,
        false,
        BTreeMap::from([("APP_NAME".to_string(), "calc".to_string())]),
    )?;

    let vars_path = ws
        .path()
        .join(".clawform/programs/mock_program/sessions/mock-session-ok/variables.json");
    let raw = fs::read_to_string(&vars_path)?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)?;
    assert_eq!(
        parsed,
        serde_json::json!({
            "APP_NAME": "calc",
            "APP_PORT": "8080"
        })
    );
    Ok(())
}

#[test]
fn apply_fails_when_required_program_variable_missing() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
variables:
  APP_NAME: {}
---
## Instruction
Write ./out.txt using ${{ var.APP_NAME }}.
"#,
    )?;
    let (runner, calls) = make_runner(
        vec![(
            PathBuf::from(".clawform/agent_result.json"),
            AGENT_RESULT_SUCCESS_JSON,
        )],
        false,
    );

    let err = run(&ws, &runner, false)
        .err()
        .context("expected missing required variable error")?;
    assert!(format!("{:#}", err).contains("missing required apply variable"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
fn apply_fails_when_program_references_undefined_variable() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
variables:
  APP_NAME:
    default: "calc"
---
## Instruction
Write ./out.txt using ${{ var.APP_PORT }}.
"#,
    )?;
    let (runner, calls) = make_runner(
        vec![(
            PathBuf::from(".clawform/agent_result.json"),
            AGENT_RESULT_SUCCESS_JSON,
        )],
        false,
    );

    let err = run(&ws, &runner, false)
        .err()
        .context("expected undefined variable reference error")?;
    assert!(format!("{:#}", err).contains("undefined variable"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
fn apply_fails_when_apply_variable_is_not_declared() -> Result<()> {
    let ws = setup_workspace(
        r#"---
id: mock_program
---
## Instruction
Write ./out.txt.
"#,
    )?;
    let (runner, calls) = make_runner(
        vec![(
            PathBuf::from(".clawform/agent_result.json"),
            AGENT_RESULT_SUCCESS_JSON,
        )],
        false,
    );

    let err = run_with_variables(
        &ws,
        &runner,
        false,
        BTreeMap::from([("APP_NAME".to_string(), "calc".to_string())]),
    )
    .err()
    .context("expected undeclared apply variable error")?;
    assert!(format!("{:#}", err).contains("is not defined"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
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
                PathBuf::from(".clawform/agent_output.md"),
                "Created out.txt with OK\n",
            ),
            (
                PathBuf::from(".clawform/agent_outputs.json"),
                "[\"out.txt\", \".clawform/agent_output.md\"]\n",
            ),
            (
                PathBuf::from(".clawform/agent_result.json"),
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
        .any(|f| f.path == ".clawform/agent_output.md"));
    assert!(!result
        .file_results
        .iter()
        .any(|f| f.path == ".clawform/agent_result.json"));
    Ok(())
}

#[test]
fn partial_agent_result_marks_apply_as_failure() -> Result<()> {
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
                PathBuf::from(".clawform/agent_result.json"),
                AGENT_RESULT_PARTIAL_JSON,
            ),
        ],
        false,
    );

    let err = run(&ws, &runner, false)
        .err()
        .context("expected apply failure for partial agent status")?;
    assert!(format!("{:#}", err).contains("partial completion"));
    let history = read_history(ws.path())?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, RunStatus::Failure);
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
                PathBuf::from(".clawform/agent_result.json"),
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
                PathBuf::from(".clawform/agent_output.md"),
                "first summary\n",
            ),
            (
                PathBuf::from(".clawform/agent_result.json"),
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
                PathBuf::from(".clawform/agent_result.json"),
                AGENT_RESULT_SUCCESS_JSON,
            ),
        ],
        false,
    );
    let result = run(&ws, &runner2, true)?;
    assert!(result.history_injected_success);

    let prompts = runner2.prompts.lock().expect("prompt mutex poisoned");
    let last = prompts.last().cloned().unwrap_or_default();
    assert!(last.contains("Clawform apply session contract"));
    assert!(last.contains("Last session details"));
    assert!(last.contains("Program changes since last session"));
    assert!(last.contains(".clawform/programs/mock_program/sessions/mock-session-ok/program.md"));
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
                PathBuf::from(".clawform/agent_result.json"),
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
    assert!(format!("{:#}", err).contains(".clawform/agent_result.json"));

    let history = read_history(ws.path())?;
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].status, RunStatus::Failure);
    assert_eq!(history[1].files_total, 0);

    let prompts = runner2.prompts.lock().expect("prompt mutex poisoned");
    let last = prompts.last().cloned().unwrap_or_default();
    assert!(last.contains("Program changes since last session"));
    assert!(last.contains("Program change summary:"));
    assert!(!last.contains("Clawform plan context"));
    Ok(())
}

fn read_history(workspace_root: &Path) -> Result<Vec<RunHistoryRecord>> {
    let path = workspace_root.join(".clawform/history/index.jsonl");
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
