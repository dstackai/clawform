use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::path_utils::to_slash_path;
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub workspace_root: PathBuf,
    pub artifacts_root: Option<PathBuf>,
    pub program_id: Option<String>,
    pub model: Option<String>,
    pub agent_result_rel: String,
    pub sandbox_mode: SandboxMode,
    pub prompt: String,
    pub progress: bool,
    pub render_progress: bool,
    pub verbose_events: bool,
    pub interactive_ui: bool,
    pub show_intermediate_steps: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderRunResult {
    pub session_id: Option<String>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub usage: ProviderUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    Auto,
    Sandboxed,
    Unsandboxed,
}

impl SandboxMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Sandboxed => "workspace-write",
            Self::Unsandboxed => "danger-full-access",
        }
    }
}

impl Default for SandboxMode {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub live_events: bool,
    pub partial_text: bool,
    pub tool_call_events: bool,
    pub file_change_events: bool,
    pub resume: bool,
    pub cancel: bool,
    pub approvals: bool,
}

impl ProviderCapabilities {
    pub fn minimal() -> Self {
        Self {
            live_events: false,
            partial_text: false,
            tool_call_events: false,
            file_change_events: false,
            resume: false,
            cancel: false,
            approvals: false,
        }
    }

    pub fn codex_v0() -> Self {
        Self {
            live_events: true,
            partial_text: false,
            tool_call_events: true,
            file_change_events: true,
            resume: true,
            cancel: false,
            approvals: false,
        }
    }
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self::minimal()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    RunStarted {
        run_id: Option<String>,
    },
    TurnStarted,
    TurnCompleted {
        usage: ProviderUsage,
    },
    TurnFailed {
        message: Option<String>,
    },
    ItemStarted {
        item_type: String,
        item_id: Option<String>,
        summary: Option<String>,
    },
    ItemUpdated {
        item_type: String,
        item_id: Option<String>,
        summary: Option<String>,
    },
    ItemCompleted {
        item_type: String,
        item_id: Option<String>,
        summary: Option<String>,
    },
    Error {
        message: String,
    },
    RawEvent {
        provider_event_type: String,
    },
    RawText {
        text: String,
    },
    Heartbeat {
        elapsed_secs: u64,
    },
}

pub trait ProviderRunner {
    fn run(&self, request: &ProviderRequest) -> Result<ProviderRunResult>;

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
}

#[derive(Debug, Default, Clone)]
pub struct CodexRunner;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexExecutionMode {
    Sandboxed,
    Unsandboxed,
}

impl CodexExecutionMode {
    fn label(self) -> &'static str {
        match self {
            Self::Sandboxed => "workspace-write",
            Self::Unsandboxed => "danger-full-access",
        }
    }
}

const PROVIDER_HEARTBEAT_SECS: u64 = 10;
const PROVIDER_INTERACTIVE_HEARTBEAT_MS: u64 = 800;
const PROVIDER_INTERACTIVE_POLL_MS: u64 = 250;
const PROVIDER_MAX_ATTEMPTS: usize = 2;
const PROVIDER_RETRY_BACKOFF_MS: u64 = 1_500;
const PROVIDER_CANCEL_POLL_MS: u64 = 100;
const AGENT_REASON_SANDBOX_NETWORK_BLOCKED: &str = "sandbox_network_blocked";
static INTERRUPT_REQUESTED: AtomicBool = AtomicBool::new(false);
static CTRL_C_HANDLER_INIT: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Debug, Clone)]
struct CommandOutputPayload {
    item_id: String,
    command: Option<String>,
    output: String,
}

#[derive(Debug, Clone)]
struct MessageOutputPayload {
    item_id: String,
    item_type: String,
    text: String,
}

#[derive(Debug, Clone)]
struct FileChangePayload {
    item_id: String,
    paths: Vec<String>,
}

#[derive(Debug, Clone)]
struct EarlyAutoRetryMonitor {
    agent_result_path: PathBuf,
    run_started_at: SystemTime,
}

#[derive(Debug, Clone, Serialize)]
struct CanonicalEventRecord {
    seq: u64,
    ts_unix_ms: u64,
    stream: String,
    event_type: String,
    raw: String,
}

#[derive(Debug)]
struct CommandOutputSink {
    root: Option<PathBuf>,
}

impl CommandOutputSink {
    fn new(root: Option<PathBuf>) -> Self {
        Self { root }
    }

    fn persist(
        &self,
        program_id: Option<&str>,
        session_id: Option<&str>,
        payload: &CommandOutputPayload,
    ) -> Result<Option<PathBuf>> {
        let mut body = String::new();
        if let Some(cmd) = payload.command.as_deref() {
            body.push_str("# command\n");
            body.push_str(cmd.trim());
            body.push_str("\n\n");
        }
        body.push_str("# output\n");
        body.push_str(payload.output.as_str());
        if !body.ends_with('\n') {
            body.push('\n');
        }

        self.persist_blob(
            program_id,
            session_id,
            "commands",
            &payload.item_id,
            "txt",
            body.as_bytes(),
        )
    }

    fn persist_message(
        &self,
        program_id: Option<&str>,
        session_id: Option<&str>,
        payload: &MessageOutputPayload,
    ) -> Result<Option<PathBuf>> {
        let mut body = String::new();
        body.push_str("# type\n");
        body.push_str(payload.item_type.trim());
        body.push_str("\n\n");
        body.push_str("# message\n");
        body.push_str(payload.text.trim());
        body.push('\n');

        self.persist_blob(
            program_id,
            session_id,
            "messages",
            &payload.item_id,
            "md",
            body.as_bytes(),
        )
    }

    fn persist_blob(
        &self,
        program_id: Option<&str>,
        session_id: Option<&str>,
        group: &str,
        item_id: &str,
        ext: &str,
        bytes: &[u8],
    ) -> Result<Option<PathBuf>> {
        let Some(root) = &self.root else {
            return Ok(None);
        };

        let item = sanitize_item_id(item_id);
        let out_path = session_base_dir(root, program_id, session_id.unwrap_or("unknown"))
            .join(group)
            .join(format!("{}.{}", item, ext));

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed creating artifact output directory '{}'",
                    parent.display()
                )
            })?;
        }

        std::fs::write(&out_path, bytes)
            .with_context(|| format!("failed writing artifact output '{}'", out_path.display()))?;
        Ok(Some(out_path))
    }
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn canonical_event_type_name(event: &ProviderEvent) -> String {
    match event {
        ProviderEvent::RunStarted { .. } => "thread.started".to_string(),
        ProviderEvent::TurnStarted => "turn.started".to_string(),
        ProviderEvent::TurnCompleted { .. } => "turn.completed".to_string(),
        ProviderEvent::TurnFailed { .. } => "turn.failed".to_string(),
        ProviderEvent::ItemStarted { .. } => "item.started".to_string(),
        ProviderEvent::ItemUpdated { .. } => "item.updated".to_string(),
        ProviderEvent::ItemCompleted { .. } => "item.completed".to_string(),
        ProviderEvent::Error { .. } => "error".to_string(),
        ProviderEvent::RawEvent {
            provider_event_type,
        } => provider_event_type.clone(),
        ProviderEvent::RawText { .. } => "raw_text".to_string(),
        ProviderEvent::Heartbeat { .. } => "heartbeat".to_string(),
    }
}

fn session_base_dir(root: &Path, program_id: Option<&str>, session_id: &str) -> PathBuf {
    let program = sanitize_program_id(program_id.unwrap_or("program"));
    let session = sanitize_session_id(session_id);
    root.join(".claudeform")
        .join("programs")
        .join(program)
        .join("sessions")
        .join(session)
}

fn persist_canonical_events(
    root: Option<&Path>,
    program_id: Option<&str>,
    session_id: &str,
    records: &[CanonicalEventRecord],
) -> Result<Option<PathBuf>> {
    let Some(root) = root else {
        return Ok(None);
    };
    if records.is_empty() {
        return Ok(None);
    }

    let out_path = session_base_dir(root, program_id, session_id).join("events.ndjson");
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating canonical events directory '{}'",
                parent.display()
            )
        })?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&out_path)
        .with_context(|| format!("failed opening canonical events '{}'", out_path.display()))?;
    for record in records {
        let line =
            serde_json::to_string(record).context("failed serializing canonical event record")?;
        file.write_all(line.as_bytes())
            .with_context(|| format!("failed writing canonical events '{}'", out_path.display()))?;
        file.write_all(b"\n").with_context(|| {
            format!(
                "failed finalizing canonical events '{}'",
                out_path.display()
            )
        })?;
    }

    Ok(Some(out_path))
}

impl ProviderRunner for CodexRunner {
    fn run(&self, request: &ProviderRequest) -> Result<ProviderRunResult> {
        preflight_codex_connectivity()?;

        match request.sandbox_mode {
            SandboxMode::Sandboxed => {
                return run_codex_with_retries(request, CodexExecutionMode::Sandboxed);
            }
            SandboxMode::Unsandboxed => {
                return run_codex_with_retries(request, CodexExecutionMode::Unsandboxed);
            }
            SandboxMode::Auto => {}
        }

        let sandbox_started_at = SystemTime::now();
        let sandboxed = run_codex_with_retries(request, CodexExecutionMode::Sandboxed)?;
        if sandboxed.exit_code == Some(0) {
            if should_retry_unsandboxed_after_success(request, sandbox_started_at) {
                print_auto_sandbox_retry_notice();
                return run_codex_with_retries(request, CodexExecutionMode::Unsandboxed);
            }
            return Ok(sandboxed);
        }

        if should_retry_unsandboxed_after_failure_with_agent_result(
            request,
            &sandboxed,
            sandbox_started_at,
        ) {
            print_auto_sandbox_retry_notice();
            return run_codex_with_retries(request, CodexExecutionMode::Unsandboxed);
        }

        Ok(sandboxed)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::codex_v0()
    }
}

fn print_auto_sandbox_retry_notice() {
    let line = "retry | sandbox: auto -> danger-full-access";
    if std::io::stdout().is_terminal() {
        println!("\x1b[2m{}\x1b[0m", line);
    } else {
        println!("{}", line);
    }
}

fn run_codex_with_retries(
    request: &ProviderRequest,
    mode: CodexExecutionMode,
) -> Result<ProviderRunResult> {
    for attempt in 1..=PROVIDER_MAX_ATTEMPTS {
        match run_codex_once(request, mode) {
            Ok(run) => {
                if run.exit_code == Some(0) {
                    return Ok(run);
                }

                if attempt < PROVIDER_MAX_ATTEMPTS && is_transient_codex_failure(&run) {
                    println!(
                        "provider_retry | attempt={}/{} | mode={} | reason=transient_transport_failure",
                        attempt + 1,
                        PROVIDER_MAX_ATTEMPTS,
                        mode.label()
                    );
                    thread::sleep(Duration::from_millis(PROVIDER_RETRY_BACKOFF_MS));
                    continue;
                }

                return Ok(run);
            }
            Err(err) => {
                if attempt < PROVIDER_MAX_ATTEMPTS && is_transient_codex_error(&err) {
                    println!(
                        "provider_retry | attempt={}/{} | mode={} | reason=transient_runtime_error",
                        attempt + 1,
                        PROVIDER_MAX_ATTEMPTS,
                        mode.label()
                    );
                    thread::sleep(Duration::from_millis(PROVIDER_RETRY_BACKOFF_MS));
                    continue;
                }

                return Err(err);
            }
        }
    }

    Err(anyhow!("provider failed after retry attempts"))
}

fn run_codex_once(
    request: &ProviderRequest,
    mode: CodexExecutionMode,
) -> Result<ProviderRunResult> {
    ensure_interrupt_handler()?;
    clear_interrupt_request();
    let run_started_at = SystemTime::now();
    let early_auto_retry_monitor =
        maybe_build_early_auto_retry_monitor(request, mode, run_started_at);

    let mut cmd = Command::new("codex");
    cmd.arg("exec")
        .arg("-c")
        .arg("model_reasoning_effort=\"high\"")
        .arg("--skip-git-repo-check")
        .arg("-")
        .current_dir(&request.workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match mode {
        CodexExecutionMode::Sandboxed => {
            cmd.arg("--full-auto");
        }
        CodexExecutionMode::Unsandboxed => {
            cmd.arg("--dangerously-bypass-approvals-and-sandbox");
        }
    }

    if request.progress {
        cmd.arg("--json");
    }

    if let Some(model) = &request.model {
        cmd.arg("--model").arg(model);
    }

    let mut child = cmd
        .spawn()
        .context("failed launching provider command 'codex'")?;

    {
        let mut stdin = child
            .stdin
            .take()
            .context("failed to open stdin for provider process")?;
        stdin
            .write_all(request.prompt.as_bytes())
            .context("failed writing prompt to provider stdin")?;
    }

    if request.progress {
        return collect_with_progress(
            child,
            request.render_progress,
            request.verbose_events,
            request.interactive_ui,
            request.show_intermediate_steps,
            request.artifacts_root.as_deref(),
            request.program_id.as_deref(),
            early_auto_retry_monitor,
        );
    }

    let output = wait_with_output_interruptible(child)?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let fallback_session_id = format!("local-{}", now_unix_millis());
    let mut seq = 0u64;
    let mut records = Vec::new();
    for line in stdout.lines() {
        records.push(CanonicalEventRecord {
            seq,
            ts_unix_ms: now_unix_millis(),
            stream: "stdout".to_string(),
            event_type: "stdout.line".to_string(),
            raw: line.to_string(),
        });
        seq = seq.saturating_add(1);
    }
    for line in stderr.lines() {
        records.push(CanonicalEventRecord {
            seq,
            ts_unix_ms: now_unix_millis(),
            stream: "stderr".to_string(),
            event_type: "stderr.line".to_string(),
            raw: line.to_string(),
        });
        seq = seq.saturating_add(1);
    }
    let _ = persist_canonical_events(
        request.artifacts_root.as_deref(),
        request.program_id.as_deref(),
        fallback_session_id.as_str(),
        &records,
    )?;

    return Ok(ProviderRunResult {
        session_id: Some(fallback_session_id),
        exit_code: output.status.code(),
        stdout,
        stderr,
        usage: ProviderUsage::default(),
    });
}

fn preflight_codex_connectivity() -> Result<()> {
    let resolved = ("api.openai.com", 443)
        .to_socket_addrs()
        .context("failed to resolve api.openai.com:443")?;

    if resolved.count() == 0 {
        return Err(anyhow!(
            "preflight failed: DNS resolution returned no addresses for api.openai.com"
        ));
    }

    Ok(())
}

fn is_transient_codex_failure(run: &ProviderRunResult) -> bool {
    if run.exit_code == Some(0) {
        return false;
    }

    let text = format!("{}\n{}", run.stdout, run.stderr).to_ascii_lowercase();
    transient_transport_markers()
        .iter()
        .any(|needle| text.contains(needle))
}

fn is_sandbox_restriction_failure(run: &ProviderRunResult) -> bool {
    if run.exit_code == Some(0) {
        return false;
    }

    let text = format!("{}\n{}", run.stdout, run.stderr).to_ascii_lowercase();
    let hard_markers = [
        "sandbox-exec: sandbox_apply",
        "not permitted by sandbox",
        "outside sandbox",
        "sandbox restriction",
        "approval required to run",
        "requires approval to run",
    ];
    if hard_markers.iter().any(|needle| text.contains(needle)) {
        return true;
    }

    let has_permission_error =
        text.contains("operation not permitted") || text.contains("permission denied");
    let has_sandbox_context = text.contains("sandbox");
    has_permission_error && has_sandbox_context
}

fn should_retry_unsandboxed_after_failure(run: &ProviderRunResult) -> bool {
    is_sandbox_restriction_failure(run)
        || is_network_restriction_failure(run)
        || output_reports_blocked_network(run)
}

fn should_retry_unsandboxed_after_failure_with_agent_result(
    request: &ProviderRequest,
    run: &ProviderRunResult,
    run_started_at: SystemTime,
) -> bool {
    should_retry_unsandboxed_after_failure(run)
        || agent_result_reports_blocked_network(
            &request.workspace_root,
            request.agent_result_rel.as_str(),
            run_started_at,
        )
}

fn should_retry_unsandboxed_after_success(
    request: &ProviderRequest,
    run_started_at: SystemTime,
) -> bool {
    // Success-path escalation should rely on structured agent_result status only.
    // Free-form stdout reasoning can mention "network restrictions" as context and
    // cause false positives (double-run behavior).
    agent_result_reports_blocked_network(
        &request.workspace_root,
        request.agent_result_rel.as_str(),
        run_started_at,
    )
}

fn is_network_restriction_failure(run: &ProviderRunResult) -> bool {
    if run.exit_code == Some(0) {
        return false;
    }

    let text = format!("{}\n{}", run.stdout, run.stderr).to_ascii_lowercase();
    output_has_network_restriction_markers(&text)
}

fn output_reports_blocked_network(run: &ProviderRunResult) -> bool {
    let text = format!("{}\n{}", run.stdout, run.stderr).to_ascii_lowercase();
    output_has_network_restriction_markers(&text) && output_has_blocked_markers(&text)
}

fn agent_result_reports_blocked_network(
    workspace_root: &Path,
    result_rel: &str,
    run_started_at: SystemTime,
) -> bool {
    if result_rel.trim().is_empty() {
        return false;
    }
    agent_result_path_reports_blocked_network(workspace_root.join(result_rel), run_started_at)
}

fn agent_result_path_reports_blocked_network(path: PathBuf, run_started_at: SystemTime) -> bool {
    let parsed = match read_recent_agent_result_value(path.as_path(), run_started_at) {
        Some(v) => v,
        None => return false,
    };
    agent_result_value_reports_blocked_network(&parsed)
}

fn detect_early_auto_retry_reason(monitor: Option<&EarlyAutoRetryMonitor>) -> Option<String> {
    let monitor = monitor?;
    let parsed = read_recent_agent_result_value(
        monitor.agent_result_path.as_path(),
        monitor.run_started_at,
    )?;
    if !agent_result_status_allows_retry(&parsed) {
        return None;
    }
    let reason = agent_result_reason_value(&parsed)?;
    if is_sandbox_network_blocked_reason(&reason) {
        return Some(reason);
    }
    None
}

fn read_recent_agent_result_value(path: &Path, run_started_at: SystemTime) -> Option<Value> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    if modified < run_started_at {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn agent_result_value_reports_blocked_network(parsed: &Value) -> bool {
    if !agent_result_status_allows_retry(parsed) {
        return false;
    }
    if let Some(reason) = agent_result_reason_value(parsed) {
        if is_sandbox_network_blocked_reason(&reason) {
            return true;
        }
    }
    let status = parsed
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let message = parsed
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let joined = format!("{} {}", status, message);
    output_has_network_restriction_markers(&joined)
}

fn agent_result_status_allows_retry(parsed: &Value) -> bool {
    let status = parsed
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    ["failure", "failed", "blocked", "partial"].contains(&status.as_str())
}

fn agent_result_reason_value(parsed: &Value) -> Option<String> {
    let reason = parsed.get("reason").and_then(Value::as_str)?.trim();
    if reason.is_empty() {
        return None;
    }
    Some(reason.to_ascii_lowercase())
}

fn is_sandbox_network_blocked_reason(reason: &str) -> bool {
    reason
        .trim()
        .eq_ignore_ascii_case(AGENT_REASON_SANDBOX_NETWORK_BLOCKED)
}

fn output_has_network_restriction_markers(text: &str) -> bool {
    let markers = [
        "blocked by network restrictions",
        "could not resolve host",
        "failed to lookup address information",
        "nodename nor servname provided",
        "dns error",
        "temporary failure in name resolution",
        "name or service not known",
        "network is unreachable",
        "no route to host",
        "connection timed out",
    ];
    markers.iter().any(|needle| text.contains(needle))
}

fn output_has_blocked_markers(text: &str) -> bool {
    let markers = [
        "blocked by network restrictions",
        "blocked",
        "unable to execute",
        "cannot download required",
        "required release note downloads unavailable",
    ];
    markers.iter().any(|needle| text.contains(needle))
}

fn is_transient_codex_error(err: &anyhow::Error) -> bool {
    let text = format!("{:#}", err).to_ascii_lowercase();
    transient_transport_markers()
        .iter()
        .any(|needle| text.contains(needle))
}

fn transient_transport_markers() -> &'static [&'static str] {
    &[
        "stream disconnected before completion",
        "failed to connect to websocket",
        "http error: 500 internal server error",
        "error sending request for url (https://api.openai.com/v1/responses)",
    ]
}

fn maybe_build_early_auto_retry_monitor(
    request: &ProviderRequest,
    mode: CodexExecutionMode,
    run_started_at: SystemTime,
) -> Option<EarlyAutoRetryMonitor> {
    if mode != CodexExecutionMode::Sandboxed || request.sandbox_mode != SandboxMode::Auto {
        return None;
    }
    let rel = request.agent_result_rel.trim();
    if rel.is_empty() {
        return None;
    }
    Some(EarlyAutoRetryMonitor {
        agent_result_path: request.workspace_root.join(rel),
        run_started_at,
    })
}

fn collect_with_progress(
    mut child: std::process::Child,
    render_progress: bool,
    verbose_events: bool,
    interactive_ui: bool,
    show_intermediate_steps: bool,
    artifacts_root: Option<&Path>,
    program_id: Option<&str>,
    early_auto_retry_monitor: Option<EarlyAutoRetryMonitor>,
) -> Result<ProviderRunResult> {
    let stdout = child
        .stdout
        .take()
        .context("failed to capture provider stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture provider stderr")?;

    let (tx, rx) = mpsc::channel::<StreamEvent>();
    let stdout_handle = spawn_stream_reader(stdout, StreamKind::Stdout, tx.clone());
    let stderr_handle = spawn_stream_reader(stderr, StreamKind::Stderr, tx);

    let mut raw_stdout = String::new();
    let mut raw_stderr = String::new();
    let mut emitted_progress_events = 0usize;
    let mut last_activity = String::new();
    let mut active_progress_items: Vec<(String, String)> = Vec::new();
    let mut last_agent_text_line: Option<String> = None;
    let mut canonical_records: Vec<CanonicalEventRecord> = Vec::new();
    let mut canonical_seq: u64 = 0;
    let mut item_started_at: HashMap<String, Instant> = HashMap::new();
    let mut usage_totals = ProviderUsage::default();
    let mut printer = ProgressPrinter::new(render_progress && interactive_ui);
    let mut last_heartbeat_at = Instant::now();
    let mut status = None;
    let mut channel_closed = false;
    let mut early_auto_retry_triggered = false;
    let mut early_auto_retry_reason: Option<String> = None;
    let mut turn_index: u64 = 0;
    let mut session_id: Option<String> = None;
    let heartbeat_interval = if printer.interactive_mode() {
        Duration::from_millis(PROVIDER_INTERACTIVE_HEARTBEAT_MS)
    } else {
        Duration::from_secs(PROVIDER_HEARTBEAT_SECS)
    };
    let poll_interval = if printer.interactive_mode() {
        Duration::from_millis(PROVIDER_INTERACTIVE_POLL_MS)
    } else {
        Duration::from_secs(1)
    };
    let sink = CommandOutputSink::new(artifacts_root.map(Path::to_path_buf));
    let supports_hyperlinks = supports_terminal_hyperlinks();
    let mut command_output_links: HashMap<String, PathBuf> = HashMap::new();
    let mut message_output_links: HashMap<String, PathBuf> = HashMap::new();
    let mut file_change_links: HashMap<String, PathBuf> = HashMap::new();

    while status.is_none() || !channel_closed {
        if interrupt_requested() {
            let _ = child.kill();
            let _ = child.wait();
            join_reader(stdout_handle, "stdout")?;
            join_reader(stderr_handle, "stderr")?;
            printer.finish();
            clear_interrupt_request();
            return Err(anyhow!("apply cancelled by user (Ctrl-C)"));
        }

        match rx.recv_timeout(poll_interval) {
            Ok(event) => {
                let (is_stdout, target, line) = match event {
                    StreamEvent::Stdout(line) => (true, &mut raw_stdout, line),
                    StreamEvent::Stderr(line) => (false, &mut raw_stderr, line),
                };
                target.push_str(&line);
                target.push('\n');

                let normalized = if is_stdout {
                    parse_codex_stream_line(&line)
                } else {
                    None
                };
                let event_type = match normalized.as_ref() {
                    Some(ev) => canonical_event_type_name(ev),
                    None => {
                        if is_stdout {
                            "stdout.line".to_string()
                        } else {
                            "stderr.line".to_string()
                        }
                    }
                };
                canonical_records.push(CanonicalEventRecord {
                    seq: canonical_seq,
                    ts_unix_ms: now_unix_millis(),
                    stream: if is_stdout {
                        "stdout".to_string()
                    } else {
                        "stderr".to_string()
                    },
                    event_type,
                    raw: line.clone(),
                });
                canonical_seq = canonical_seq.saturating_add(1);

                // Liveness/progress is driven only by Codex JSON stream on stdout.
                // Stderr can contain banners or transport noise, but we still surface useful startup hints.
                if is_stdout {
                    if let Some(normalized) = normalized {
                        match normalized {
                            ProviderEvent::RunStarted { ref run_id } => {
                                if let Some(id) = run_id.as_ref() {
                                    session_id = Some(id.clone());
                                }
                            }
                            ProviderEvent::TurnStarted => {
                                turn_index = turn_index.saturating_add(1);
                            }
                            ProviderEvent::ItemStarted {
                                ref item_id,
                                ref item_type,
                                ref summary,
                                ..
                            } => {
                                if let Some(id) = item_id.clone() {
                                    item_started_at.insert(id, Instant::now());
                                }
                                if should_count_item_progress(
                                    item_type,
                                    summary.as_deref(),
                                    show_intermediate_steps,
                                ) {
                                    let label =
                                        status_activity_label(item_type, summary.as_deref());
                                    if let Some(id) = item_id.as_ref() {
                                        active_progress_items
                                            .retain(|(active_id, _)| active_id != id);
                                        active_progress_items.push((id.clone(), label.clone()));
                                    }
                                    last_activity = label;
                                }
                            }
                            ProviderEvent::ItemCompleted {
                                ref item_id,
                                ref item_type,
                                ref summary,
                                ..
                            } => {
                                if should_count_item_progress(
                                    item_type,
                                    summary.as_deref(),
                                    show_intermediate_steps,
                                ) {
                                    if let Some(id) = item_id.as_ref() {
                                        active_progress_items
                                            .retain(|(active_id, _)| active_id != id);
                                    }
                                    last_activity = active_progress_items
                                        .last()
                                        .map(|(_, label)| label.clone())
                                        .unwrap_or_default();
                                }
                            }
                            ProviderEvent::TurnCompleted { ref usage } => {
                                merge_usage(&mut usage_totals, usage);
                            }
                            _ => {}
                        }
                        if !matches!(normalized, ProviderEvent::RawText { .. }) {
                            emitted_progress_events += 1;
                        }
                        if let Some(payload) = extract_command_output_payload(&line) {
                            if let Ok(Some(path)) =
                                sink.persist(program_id, session_id.as_deref(), &payload)
                            {
                                command_output_links.insert(payload.item_id, path);
                            }
                        }
                        if let Some(payload) = extract_message_output_payload(&line) {
                            if let Ok(Some(path)) =
                                sink.persist_message(program_id, session_id.as_deref(), &payload)
                            {
                                message_output_links.insert(payload.item_id, path);
                            }
                        }
                        if let Some(payload) = extract_file_change_payload(&line) {
                            if let Some(first_path) = payload.paths.first() {
                                let path = make_clickable_path(first_path, artifacts_root);
                                file_change_links.insert(payload.item_id, path);
                            }
                        }
                        let completion_duration =
                            item_completion_duration_label(&normalized, &mut item_started_at);
                        let mut progress_line = format_terminal_event(
                            &normalized,
                            verbose_events,
                            show_intermediate_steps,
                        );
                        if show_intermediate_steps && progress_line.is_none() {
                            if let ProviderEvent::TurnCompleted { ref usage } = normalized {
                                progress_line = format_turn_usage_line(turn_index, usage);
                            }
                        }
                        if let Some(progress_line) = progress_line {
                            let progress_line =
                                add_completion_duration_suffix(&progress_line, completion_duration);
                            let progress_line = add_command_output_link_suffix(
                                &normalized,
                                &progress_line,
                                &command_output_links,
                                supports_hyperlinks,
                            );
                            let progress_line = add_message_output_link_suffix(
                                &normalized,
                                &progress_line,
                                &message_output_links,
                                supports_hyperlinks,
                            );
                            let progress_line = add_file_change_link_suffix(
                                &normalized,
                                &progress_line,
                                &file_change_links,
                                supports_hyperlinks,
                            );
                            if is_text_event_line(&progress_line)
                                && last_agent_text_line.as_deref() == Some(progress_line.as_str())
                            {
                                continue;
                            }
                            if is_text_event_line(&progress_line) {
                                last_agent_text_line = Some(progress_line.clone());
                            }
                            if render_progress {
                                printer.print_event(&progress_line);
                            }
                        }
                    }
                } else if emitted_progress_events == 0 {
                    if let Some(hint) = extract_startup_stderr_hint(&line) {
                        if render_progress {
                            printer.print_event(&format!("startup_hint | {}", hint));
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                channel_closed = true;
            }
        }

        if status.is_none() {
            if !early_auto_retry_triggered {
                if let Some(reason) =
                    detect_early_auto_retry_reason(early_auto_retry_monitor.as_ref())
                {
                    early_auto_retry_triggered = true;
                    early_auto_retry_reason = Some(reason);
                    let _ = child.kill();
                }
            }
            if let Some(done) = child
                .try_wait()
                .context("failed while polling provider process")?
            {
                status = Some(done);
            } else {
                if render_progress
                    && show_intermediate_steps
                    && Instant::now().duration_since(last_heartbeat_at) >= heartbeat_interval
                {
                    printer.print_status(&format_status_line(&last_activity));
                    last_heartbeat_at = Instant::now();
                }
            }
        }
    }

    join_reader(stdout_handle, "stdout")?;
    join_reader(stderr_handle, "stderr")?;

    if render_progress && show_intermediate_steps && emitted_progress_events == 0 {
        printer.print_event("no_live_events");
    }
    printer.finish();

    if let Some(reason) = early_auto_retry_reason.as_deref() {
        if !raw_stderr.is_empty() && !raw_stderr.ends_with('\n') {
            raw_stderr.push('\n');
        }
        raw_stderr.push_str(
            "blocked by network restrictions (auto sandbox retry requested from agent_result reason: ",
        );
        raw_stderr.push_str(reason);
        raw_stderr.push_str(")\n");
    }

    let final_session_id = match session_id.as_deref() {
        Some(id) if !id.trim().is_empty() => sanitize_session_id(id),
        _ => format!("local-{}", now_unix_millis()),
    };
    let _ = persist_canonical_events(
        artifacts_root,
        program_id,
        final_session_id.as_str(),
        &canonical_records,
    )?;

    Ok(ProviderRunResult {
        session_id: Some(final_session_id),
        exit_code: status.and_then(|s| s.code()),
        stdout: raw_stdout,
        stderr: raw_stderr,
        usage: usage_totals,
    })
}

fn merge_usage(totals: &mut ProviderUsage, usage: &ProviderUsage) {
    if let Some(v) = usage.input_tokens {
        totals.input_tokens = Some(totals.input_tokens.unwrap_or(0) + v);
    }
    if let Some(v) = usage.cached_input_tokens {
        totals.cached_input_tokens = Some(totals.cached_input_tokens.unwrap_or(0) + v);
    }
    if let Some(v) = usage.output_tokens {
        totals.output_tokens = Some(totals.output_tokens.unwrap_or(0) + v);
    }
}

fn item_completion_duration_label(
    event: &ProviderEvent,
    started_at: &mut HashMap<String, Instant>,
) -> Option<String> {
    let (item_type, item_id) = match event {
        ProviderEvent::ItemCompleted {
            item_type, item_id, ..
        } => (item_type.as_str(), item_id.as_deref()?),
        _ => return None,
    };

    if item_type != "command_execution"
        && item_type != "file_change"
        && item_type != "mcp_tool_call"
        && item_type != "web_search"
    {
        return None;
    }

    let started = started_at.remove(item_id)?;
    Some(format_duration_short(started.elapsed()))
}

fn add_completion_duration_suffix(line: &str, duration: Option<String>) -> String {
    let Some(d) = duration else {
        return line.to_string();
    };
    if line.strip_prefix("✔ ").is_some() {
        return format!("{} | {}", line, d);
    }
    line.to_string()
}

fn add_command_output_link_suffix(
    event: &ProviderEvent,
    line: &str,
    command_output_links: &HashMap<String, PathBuf>,
    supports_hyperlinks: bool,
) -> String {
    let ProviderEvent::ItemCompleted {
        item_type, item_id, ..
    } = event
    else {
        return line.to_string();
    };

    if item_type != "command_execution" {
        return line.to_string();
    }

    let Some(item_id) = item_id.as_deref() else {
        return line.to_string();
    };
    let Some(path) = command_output_links.get(item_id) else {
        return line.to_string();
    };

    let rendered = if supports_hyperlinks {
        terminal_link(path, "out").unwrap_or_else(|| "out".to_string())
    } else {
        format!("out={}", to_slash_path(path))
    };

    format!("{} | {}", line, rendered)
}

fn add_message_output_link_suffix(
    event: &ProviderEvent,
    line: &str,
    message_output_links: &HashMap<String, PathBuf>,
    supports_hyperlinks: bool,
) -> String {
    let (item_type, item_id) = match event {
        ProviderEvent::ItemStarted {
            item_type, item_id, ..
        } => (item_type.as_str(), item_id.as_deref()),
        ProviderEvent::ItemUpdated {
            item_type, item_id, ..
        } => (item_type.as_str(), item_id.as_deref()),
        ProviderEvent::ItemCompleted {
            item_type, item_id, ..
        } => (item_type.as_str(), item_id.as_deref()),
        _ => return line.to_string(),
    };

    if !is_reasoning_item_type(item_type) && !is_agent_text_item_type(item_type) {
        return line.to_string();
    }
    let Some(item_id) = item_id else {
        return line.to_string();
    };
    let Some(path) = message_output_links.get(item_id) else {
        return line.to_string();
    };

    let rendered = if supports_hyperlinks {
        terminal_link(path, "msg").unwrap_or_else(|| "msg".to_string())
    } else {
        format!("msg={}", to_slash_path(path))
    };

    format!("{} | {}", line, rendered)
}

fn add_file_change_link_suffix(
    event: &ProviderEvent,
    line: &str,
    file_change_links: &HashMap<String, PathBuf>,
    supports_hyperlinks: bool,
) -> String {
    let ProviderEvent::ItemCompleted {
        item_type, item_id, ..
    } = event
    else {
        return line.to_string();
    };
    if item_type != "file_change" {
        return line.to_string();
    }
    let Some(item_id) = item_id.as_deref() else {
        return line.to_string();
    };
    let Some(path) = file_change_links.get(item_id) else {
        return line.to_string();
    };

    let rendered = if supports_hyperlinks {
        terminal_link(path, "file").unwrap_or_else(|| "file".to_string())
    } else {
        format!("file={}", to_slash_path(path))
    };
    format!("{} | {}", line, rendered)
}

fn make_clickable_path(raw_path: &str, workspace_root: Option<&Path>) -> PathBuf {
    let path = Path::new(raw_path);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    if let Some(root) = workspace_root {
        return root.join(path);
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
}

fn format_duration_short(duration: Duration) -> String {
    if duration.as_nanos() == 0 {
        return "<1ms".to_string();
    }
    if duration.as_millis() < 1_000 {
        let millis = duration.as_millis();
        let rounded = if millis == 0 { 1 } else { millis };
        return format!("{}ms", rounded);
    }
    if duration.as_secs() < 10 {
        return format!("{:.1}s", duration.as_secs_f64());
    }
    format!("{}s", duration.as_secs())
}

fn extract_command_output_payload(line: &str) -> Option<CommandOutputPayload> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("item.completed") {
        return None;
    }
    let item = value.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("command_execution") {
        return None;
    }
    let item_id = item.get("id").and_then(Value::as_str)?.trim().to_string();
    if item_id.is_empty() {
        return None;
    }
    let output = item
        .get("aggregated_output")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if output.trim().is_empty() {
        return None;
    }

    let command = item
        .get("command")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    Some(CommandOutputPayload {
        item_id,
        command,
        output,
    })
}

fn extract_message_output_payload(line: &str) -> Option<MessageOutputPayload> {
    let value: Value = serde_json::from_str(line).ok()?;
    let event_type = value.get("type").and_then(Value::as_str)?;
    if !matches!(
        event_type,
        "item.started" | "item.updated" | "item.completed"
    ) {
        return None;
    }

    let item = value.get("item")?;
    let item_type = item.get("type").and_then(Value::as_str)?.to_string();
    if !is_reasoning_item_type(&item_type) && !is_agent_text_item_type(&item_type) {
        return None;
    }

    let item_id = item.get("id").and_then(Value::as_str)?.trim().to_string();
    if item_id.is_empty() {
        return None;
    }

    let text = item
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if text.is_empty() {
        return None;
    }

    Some(MessageOutputPayload {
        item_id,
        item_type,
        text,
    })
}

fn extract_file_change_payload(line: &str) -> Option<FileChangePayload> {
    let value: Value = serde_json::from_str(line).ok()?;
    let event_type = value.get("type").and_then(Value::as_str)?;
    if !matches!(
        event_type,
        "item.started" | "item.updated" | "item.completed"
    ) {
        return None;
    }

    let item = value.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("file_change") {
        return None;
    }

    let item_id = item.get("id").and_then(Value::as_str)?.trim().to_string();
    if item_id.is_empty() {
        return None;
    }

    let mut paths: Vec<String> = Vec::new();
    if let Some(path) = item.get("path").and_then(Value::as_str) {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            paths.push(trimmed.to_string());
        }
    }
    if let Some(changes) = item.get("changes").and_then(Value::as_array) {
        for change in changes {
            let Some(path) = change.get("path").and_then(Value::as_str) else {
                continue;
            };
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                paths.push(trimmed.to_string());
            }
        }
    }

    if paths.is_empty() {
        return None;
    }
    paths.sort();
    paths.dedup();

    Some(FileChangePayload { item_id, paths })
}

fn sanitize_session_id(raw: &str) -> String {
    sanitize_token(raw, "session")
}

fn sanitize_program_id(raw: &str) -> String {
    sanitize_token(raw, "program")
}

fn sanitize_item_id(raw: &str) -> String {
    sanitize_token(raw, "item")
}

fn sanitize_token(raw: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn supports_terminal_hyperlinks() -> bool {
    if !std::io::stdout().is_terminal() {
        return false;
    }
    if env::var("CLAUDEFORM_NO_HYPERLINKS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return false;
    }
    match env::var("TERM") {
        Ok(term) if term.eq_ignore_ascii_case("dumb") => false,
        _ => true,
    }
}

fn terminal_link(path: &Path, label: &str) -> Option<String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let file_url = format!("file://{}", percent_encode_path(&abs));
    Some(format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", file_url, label))
}

fn percent_encode_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 8);
    for b in raw.as_bytes() {
        let c = *b as char;
        let safe = c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '~');
        if safe {
            out.push(c);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        }
    }
    out
}

fn wait_with_output_interruptible(mut child: std::process::Child) -> Result<std::process::Output> {
    loop {
        if interrupt_requested() {
            let _ = child.kill();
            let _ = child
                .wait_with_output()
                .context("failed while collecting provider output after cancellation")?;
            clear_interrupt_request();
            return Err(anyhow!("apply cancelled by user (Ctrl-C)"));
        }

        if child
            .try_wait()
            .context("failed while polling provider process")?
            .is_some()
        {
            return child
                .wait_with_output()
                .context("failed while waiting for provider process");
        }

        thread::sleep(Duration::from_millis(PROVIDER_CANCEL_POLL_MS));
    }
}

pub(crate) fn ensure_interrupt_handler() -> Result<()> {
    let init = CTRL_C_HANDLER_INIT.get_or_init(|| {
        ctrlc::set_handler(|| {
            INTERRUPT_REQUESTED.store(true, Ordering::SeqCst);
        })
        .map_err(|e| e.to_string())
    });

    match init {
        Ok(()) => Ok(()),
        Err(msg) => Err(anyhow!("failed installing Ctrl-C handler: {}", msg)),
    }
}

pub(crate) fn interrupt_requested() -> bool {
    INTERRUPT_REQUESTED.load(Ordering::SeqCst)
}

pub(crate) fn clear_interrupt_request() {
    INTERRUPT_REQUESTED.store(false, Ordering::SeqCst);
}

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug)]
enum StreamEvent {
    Stdout(String),
    Stderr(String),
}

fn spawn_stream_reader<R: std::io::Read + Send + 'static>(
    stream: R,
    kind: StreamKind,
    tx: mpsc::Sender<StreamEvent>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let line = line.context("failed reading provider output stream line")?;

            let event = match kind {
                StreamKind::Stdout => StreamEvent::Stdout(line),
                StreamKind::Stderr => StreamEvent::Stderr(line),
            };
            if tx.send(event).is_err() {
                break;
            }
        }
        Ok(())
    })
}

fn join_reader(handle: thread::JoinHandle<Result<()>>, stream_name: &str) -> Result<()> {
    handle
        .join()
        .map_err(|_| anyhow!("provider {} reader thread panicked", stream_name))?
        .with_context(|| format!("provider {} reader failed", stream_name))
}

fn parse_codex_stream_line(line: &str) -> Option<ProviderEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let value: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            return Some(ProviderEvent::RawText {
                text: trimmed.to_string(),
            });
        }
    };

    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    match event_type {
        "thread.started" => {
            let run_id = value
                .get("thread_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            Some(ProviderEvent::RunStarted { run_id })
        }
        "turn.started" => Some(ProviderEvent::TurnStarted),
        "turn.completed" => Some(ProviderEvent::TurnCompleted {
            usage: parse_usage(&value),
        }),
        "turn.failed" => Some(ProviderEvent::TurnFailed {
            message: extract_error_message(&value),
        }),
        "item.started" | "item.updated" | "item.completed" => {
            let item = value.get("item").unwrap_or(&Value::Null);
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let item_id = item
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let summary = summarize_item(item);

            match event_type {
                "item.started" => Some(ProviderEvent::ItemStarted {
                    item_type,
                    item_id,
                    summary,
                }),
                "item.updated" => Some(ProviderEvent::ItemUpdated {
                    item_type,
                    item_id,
                    summary,
                }),
                _ => Some(ProviderEvent::ItemCompleted {
                    item_type,
                    item_id,
                    summary,
                }),
            }
        }
        "error" => Some(ProviderEvent::Error {
            message: extract_error_message(&value).unwrap_or_else(|| "unknown error".to_string()),
        }),
        _ => Some(ProviderEvent::RawEvent {
            provider_event_type: event_type.to_string(),
        }),
    }
}

fn parse_usage(value: &Value) -> ProviderUsage {
    let usage = value.get("usage").unwrap_or(&Value::Null);

    ProviderUsage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        cached_input_tokens: usage.get("cached_input_tokens").and_then(Value::as_u64),
    }
}

fn extract_error_message(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(Value::as_object)
        .and_then(|o| o.get("message"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

fn summarize_item(item: &Value) -> Option<String> {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return Some(truncate_one_line(text, 120));
    }

    if let Some(command) = item.get("command").and_then(Value::as_str) {
        return Some(simplify_command_summary(command));
    }

    if item.get("type").and_then(Value::as_str) == Some("file_change") {
        if let Some(summary) = summarize_file_change_item(item) {
            return Some(summary);
        }
    }

    if let Some(path) = item.get("path").and_then(Value::as_str) {
        return Some(format!("path={}", path));
    }

    if let Some(tool_name) = item
        .get("tool_name")
        .and_then(Value::as_str)
        .or_else(|| item.get("name").and_then(Value::as_str))
    {
        return Some(format!("tool={}", tool_name));
    }

    None
}

fn summarize_file_change_item(item: &Value) -> Option<String> {
    let mut entries: Vec<String> = Vec::new();

    if let Some(path) = item.get("path").and_then(Value::as_str) {
        let summary = item
            .get("kind")
            .and_then(Value::as_str)
            .map(|k| format_file_change_entry(path, Some(k)))
            .unwrap_or_else(|| format!("path={}", path));
        entries.push(summary);
    }

    if let Some(changes) = item.get("changes").and_then(Value::as_array) {
        for change in changes {
            let Some(path) = change.get("path").and_then(Value::as_str) else {
                continue;
            };
            let kind = change.get("kind").and_then(Value::as_str);
            entries.push(format_file_change_entry(path, kind));
        }
    }

    if entries.is_empty() {
        return None;
    }

    entries.sort();
    entries.dedup();

    let first = entries.first()?.clone();
    let more = entries.len().saturating_sub(1);
    if more == 0 {
        Some(first)
    } else {
        Some(format!("{} (+{} more)", first, more))
    }
}

fn format_file_change_entry(path: &str, kind: Option<&str>) -> String {
    let normalized = to_slash_path(Path::new(path));
    match kind {
        Some(k) if !k.trim().is_empty() => format!("{} {}", k.trim(), normalized),
        _ => normalized,
    }
}

fn simplify_command_summary(command: &str) -> String {
    let mut cmd = command.trim();
    if let Some(rest) = cmd.strip_prefix("/bin/zsh -lc ") {
        cmd = rest.trim();
    }
    cmd = cmd.trim_matches('"').trim_matches('\'');

    if cmd.starts_with("cd ") && cmd.contains("&&") {
        if let Some((_, rhs)) = cmd.rsplit_once("&&") {
            cmd = rhs.trim();
        }
    }

    if let Some(path) = extract_heredoc_write_path(cmd) {
        return format!("write {}", path);
    }
    if let Some(path) = extract_redirect_write_path(cmd) {
        return format!("write {}", path);
    }

    // Keep command lines compact but long enough to preserve most real file paths
    // so terminal path detection remains useful.
    truncate_one_line(cmd, 240)
}

fn extract_heredoc_write_path(command: &str) -> Option<String> {
    let marker = "cat <<'EOF' > ";
    let idx = command.find(marker)?;
    let rest = &command[idx + marker.len()..];
    let path = rest
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches('"')
        .trim_matches('\'');
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

fn extract_redirect_write_path(command: &str) -> Option<String> {
    let redirect_idx = command.rfind('>')?;
    let lhs = command[..redirect_idx].trim();
    if !(lhs.starts_with("cat ")
        || lhs.starts_with("printf ")
        || lhs.starts_with("echo ")
        || lhs.starts_with("tee "))
    {
        return None;
    }

    let mut rhs = command[redirect_idx + 1..].trim();
    if let Some(stripped) = rhs.strip_prefix('>') {
        rhs = stripped.trim();
    }
    let path = rhs
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches('"')
        .trim_matches('\'');
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

fn truncate_one_line(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ").replace('\r', " ");
    let trimmed = one_line.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }

    let mut out = String::new();
    for c in trimmed.chars().take(max.saturating_sub(3)) {
        out.push(c);
    }
    out.push_str("...");
    out
}

fn format_terminal_event(
    event: &ProviderEvent,
    verbose_events: bool,
    show_intermediate_steps: bool,
) -> Option<String> {
    match event {
        ProviderEvent::RunStarted { run_id } => {
            if !show_intermediate_steps {
                return None;
            }
            Some(match run_id.as_deref() {
                Some(id) => format!("session {}", id),
                None => "session started".to_string(),
            })
        }
        ProviderEvent::TurnStarted => None,
        ProviderEvent::TurnCompleted { .. } => None,
        ProviderEvent::TurnFailed { message } => Some(format!(
            "turn.failed{}",
            message
                .as_ref()
                .map(|m| format!(" | {}", m))
                .unwrap_or_default()
        )),
        ProviderEvent::ItemStarted { .. } => None,
        ProviderEvent::ItemCompleted {
            item_type,
            item_id,
            summary,
        } => {
            if !verbose_events {
                return None;
            }
            format_item_event(
                "item.completed",
                item_type,
                item_id.as_deref(),
                summary.as_deref(),
                show_intermediate_steps,
            )
        }
        ProviderEvent::ItemUpdated { .. } => None,
        ProviderEvent::Error { message } => Some(format!("error | {}", message)),
        ProviderEvent::RawEvent {
            provider_event_type,
        } => {
            if verbose_events {
                Some(provider_event_type.clone())
            } else {
                None
            }
        }
        ProviderEvent::RawText { .. } => None,
        ProviderEvent::Heartbeat { elapsed_secs } => {
            if !show_intermediate_steps {
                return None;
            }
            Some(format!("running | elapsed={}s", elapsed_secs))
        }
    }
}

fn format_item_event(
    phase: &str,
    item_type: &str,
    item_id: Option<&str>,
    summary: Option<&str>,
    show_intermediate_steps: bool,
) -> Option<String> {
    if !show_intermediate_steps {
        return None;
    }

    if is_reasoning_item_type(item_type) {
        let text = summary?;
        if is_low_signal_note(text) {
            return None;
        }
        return Some(format!("💭 {}", truncate_one_line(text, 180)));
    }

    if is_agent_text_item_type(item_type) {
        let text = summary?;
        if is_low_signal_note(text) {
            return None;
        }
        return Some(format!("💬 {}", truncate_one_line(text, 180)));
    }

    let kind = match item_type {
        "command_execution" => "cmd",
        "file_change" => "file",
        "mcp_tool_call" => "tool",
        "web_search" => "search",
        "todo_list" => "todo",
        _ => return None,
    };

    let _ = phase;
    let _ = item_id;
    let summary = summary?.trim();
    if kind == "cmd" && is_claudeform_housekeeping_command(summary) {
        return None;
    }
    if kind == "cmd" {
        return Some(format!("✔ {}", summary));
    }

    Some(format!("✔ {} {}", kind, summary))
}

fn is_agent_text_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "assistant_message" | "agent_message" | "message" | "output_text" | "text"
    )
}

fn is_reasoning_item_type(item_type: &str) -> bool {
    item_type == "reasoning"
}

fn is_text_event_line(line: &str) -> bool {
    line.starts_with("💬 ") || line.starts_with("💭 ")
}

fn is_low_signal_command(summary: &str) -> bool {
    let s = summary.trim().to_ascii_lowercase();
    s == "pwd"
        || s == "ls"
        || s.starts_with("ls ")
        || s.starts_with("cat ")
        || s.starts_with("cat -")
        || s.starts_with("find ")
        || s.starts_with("rg ")
        || s.starts_with("git status")
}

fn is_claudeform_housekeeping_command(summary: &str) -> bool {
    let s = summary.trim().to_ascii_lowercase();
    s.starts_with("write .claudeform/agent_output.md")
        || s.starts_with("write .claudeform/agent_summary.md")
        || s.starts_with("write .claudeform/agent_outputs.json")
        || s.starts_with("write .claudeform/agent_result.json")
        || s.starts_with("cat .claudeform/agent_output.md")
        || s.starts_with("cat .claudeform/agent_summary.md")
        || s.starts_with("cat .claudeform/agent_outputs.json")
        || s.starts_with("cat .claudeform/agent_result.json")
        || {
            let is_read_write = s.starts_with("write ") || s.starts_with("cat ");
            is_read_write
                && s.contains(".claudeform/programs/")
                && (s.contains("/reports/agent_output")
                    || s.contains("/reports/agent_outputs")
                    || s.contains("/reports/agent_result"))
        }
}

fn is_low_signal_note(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("preparing final message")
        || t.contains("summarizing final response")
        || t.contains("summarizing final output")
        || t.contains("craft the final response")
        || t.contains("getting ready to craft")
        || t.contains("final response plan")
}

fn format_turn_usage_line(turn_index: u64, usage: &ProviderUsage) -> Option<String> {
    if usage.input_tokens.is_none()
        && usage.output_tokens.is_none()
        && usage.cached_input_tokens.is_none()
    {
        return None;
    }

    let input = usage
        .input_tokens
        .map(format_token_compact)
        .unwrap_or_else(|| "-".to_string());
    let output = usage
        .output_tokens
        .map(format_token_compact)
        .unwrap_or_else(|| "-".to_string());
    let cached = usage
        .cached_input_tokens
        .map(format_token_compact)
        .unwrap_or_else(|| "-".to_string());
    let turn = if turn_index == 0 { 1 } else { turn_index };
    Some(format!(
        "turn {} | tokens: in={} out={} cached={}",
        turn, input, output, cached
    ))
}

fn format_status_line(activity: &str) -> String {
    if activity.trim().is_empty() {
        "running".to_string()
    } else {
        format!("running: {}", activity)
    }
}

fn format_token_compact(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    if value < 1_000_000 {
        let k = value as f64 / 1_000.0;
        if k >= 100.0 {
            return format!("{:.0}k", k);
        }
        return format!("{:.1}k", k);
    }
    let m = value as f64 / 1_000_000.0;
    if m >= 100.0 {
        return format!("{:.0}m", m);
    }
    format!("{:.1}m", m)
}

fn status_activity_label(item_type: &str, summary: Option<&str>) -> String {
    match (item_type, summary) {
        ("command_execution", Some(summary)) => truncate_one_line(summary, 56),
        ("file_change", Some(summary)) => format!("file: {}", truncate_one_line(summary, 48)),
        ("mcp_tool_call", Some(summary)) => format!("tool: {}", truncate_one_line(summary, 48)),
        ("web_search", Some(summary)) => format!("search: {}", truncate_one_line(summary, 48)),
        ("todo_list", Some(summary)) => format!("todo: {}", truncate_one_line(summary, 48)),
        ("command_execution", None) => "command".to_string(),
        ("file_change", None) => "file change".to_string(),
        ("mcp_tool_call", None) => "tool call".to_string(),
        ("web_search", None) => "search".to_string(),
        ("todo_list", None) => "todo".to_string(),
        (_, Some(summary)) => truncate_one_line(summary, 56),
        (_, None) => "working".to_string(),
    }
}

fn should_count_item_progress(
    item_type: &str,
    summary: Option<&str>,
    show_intermediate_steps: bool,
) -> bool {
    if item_type != "command_execution" {
        return true;
    }
    if let Some(s) = summary {
        if is_claudeform_housekeeping_command(s) {
            return false;
        }
    }
    if show_intermediate_steps {
        return true;
    }
    match summary {
        Some(s) => !is_low_signal_command(s),
        None => true,
    }
}

#[derive(Debug)]
struct ProgressPrinter {
    interactive: bool,
    status_line: Option<String>,
    spinner_idx: usize,
    cursor_hidden: bool,
}

impl ProgressPrinter {
    fn new(prefer_interactive: bool) -> Self {
        Self {
            interactive: prefer_interactive && std::io::stdout().is_terminal(),
            status_line: None,
            spinner_idx: 0,
            cursor_hidden: false,
        }
    }

    fn interactive_mode(&self) -> bool {
        self.interactive
    }

    fn print_event(&mut self, line: &str) {
        let rendered = self.render_event_line(line);
        if self.interactive {
            self.clear_line();
            println!("{}", rendered);
            self.redraw_status();
        } else {
            println!("{}", rendered);
        }
    }

    fn print_status(&mut self, line: &str) {
        self.status_line = Some(line.to_string());
        if self.interactive {
            self.hide_cursor();
            self.clear_line();
            self.redraw_status();
        } else {
            println!("{}", line);
        }
    }

    fn finish(&mut self) {
        if self.interactive && self.status_line.is_some() {
            self.clear_line();
            self.status_line = None;
        }
        self.show_cursor();
    }

    fn redraw_status(&mut self) {
        if let Some(status) = self.status_line.clone() {
            print!(
                "\x1b[36m{}\x1b[0m {}",
                self.next_spinner(),
                self.render_status_line(&status)
            );
            let _ = std::io::stdout().flush();
        }
    }

    fn clear_line(&self) {
        print!("\r\x1b[2K");
        let _ = std::io::stdout().flush();
    }

    fn hide_cursor(&mut self) {
        if !self.interactive || self.cursor_hidden {
            return;
        }
        print!("\x1b[?25l");
        let _ = std::io::stdout().flush();
        self.cursor_hidden = true;
    }

    fn show_cursor(&mut self) {
        if !self.interactive || !self.cursor_hidden {
            return;
        }
        print!("\x1b[?25h");
        let _ = std::io::stdout().flush();
        self.cursor_hidden = false;
    }

    fn next_spinner(&mut self) -> char {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let frame = FRAMES[self.spinner_idx % FRAMES.len()];
        self.spinner_idx = self.spinner_idx.wrapping_add(1);
        frame
    }

    fn render_event_line(&self, line: &str) -> String {
        if !self.interactive {
            return line.to_string();
        }

        if let Some(rest) = line.strip_prefix("✔ ") {
            return format!("\x1b[32m✔\x1b[0m {}", colorize_done_payload(rest));
        }
        if let Some(rest) = line.strip_prefix("💬 ") {
            return format!("\x1b[35m💬\x1b[0m {}", colorize_paths(rest));
        }
        if let Some(rest) = line.strip_prefix("💭 ") {
            return format!("\x1b[34m💭\x1b[0m {}", colorize_paths(rest));
        }
        if line.starts_with("turn ") && line.contains(" | tokens: ") {
            return format!("\x1b[2m{}\x1b[0m", line);
        }
        if let Some(rest) = line.strip_prefix("session ") {
            return format!("\x1b[2msession\x1b[0m {}", colorize_paths(rest));
        }
        if line.starts_with("turn.failed") || line.starts_with("error |") {
            return format!("\x1b[31m{}\x1b[0m", line);
        }
        if line.starts_with("startup_hint") {
            return format!("\x1b[33m{}\x1b[0m", line);
        }

        colorize_paths(line)
    }

    fn render_status_line(&self, line: &str) -> String {
        if !self.interactive {
            return line.to_string();
        }

        if let Some(rest) = line.strip_prefix("running: ") {
            return format!("\x1b[2mrunning\x1b[0m: {}", colorize_paths(rest));
        }
        colorize_paths(line)
    }
}

impl Drop for ProgressPrinter {
    fn drop(&mut self) {
        self.show_cursor();
    }
}

fn colorize_done_payload(payload: &str) -> String {
    let trimmed = payload.trim();
    let segments = trimmed.split(" | ").collect::<Vec<_>>();
    if segments.len() > 1 {
        let mut out = Vec::with_capacity(segments.len());
        out.push(colorize_command_summary(segments[0]));
        for seg in segments.iter().skip(1) {
            out.push(colorize_done_segment(seg));
        }
        return out.join(" | ");
    }

    if let Some((head, tail)) = trimmed.rsplit_once(' ') {
        if looks_like_duration_label(tail) {
            return format!("{} \x1b[2m{}\x1b[0m", colorize_command_summary(head), tail);
        }
    }
    colorize_command_summary(trimmed)
}

fn colorize_done_segment(segment: &str) -> String {
    if looks_like_duration_label(segment) {
        format!("\x1b[2m{}\x1b[0m", segment)
    } else if let Some(colored) = colorize_link_segment(segment) {
        colored
    } else {
        colorize_paths(segment)
    }
}

fn colorize_link_segment(segment: &str) -> Option<String> {
    let trimmed = segment.trim();
    if trimmed.is_empty() {
        return None;
    }

    if matches!(trimmed, "out" | "msg" | "file") {
        return Some(format!("\x1b[95m{}\x1b[0m", segment));
    }

    for prefix in ["out=", "msg=", "file="] {
        if let Some(rest) = segment.strip_prefix(prefix) {
            return Some(format!("\x1b[95m{}\x1b[0m{}", prefix, colorize_paths(rest)));
        }
    }

    if segment.contains("\x1b]8;;") {
        return Some(format!("\x1b[95m{}\x1b[0m", segment));
    }

    None
}

fn looks_like_duration_label(s: &str) -> bool {
    if s == "<1ms" {
        return true;
    }

    let lower = s.to_ascii_lowercase();
    if let Some(ms) = lower.strip_suffix("ms") {
        return ms.chars().all(|c| c.is_ascii_digit());
    }
    if let Some(sec) = lower.strip_suffix('s') {
        return sec.chars().all(|c| c.is_ascii_digit() || c == '.');
    }
    false
}

fn colorize_command_summary(summary: &str) -> String {
    let mut parts = summary.splitn(2, ' ');
    let first = parts.next().unwrap_or_default();
    let rest = parts.next();
    if first.is_empty() {
        return colorize_paths(summary);
    }
    let first_colored = format!("\x1b[33m{}\x1b[0m", first);
    match rest {
        Some(rest) if !rest.is_empty() => format!("{} {}", first_colored, colorize_paths(rest)),
        _ => first_colored,
    }
}

fn colorize_paths(text: &str) -> String {
    let mut out = String::new();
    let mut token = String::new();

    for ch in text.chars() {
        if ch.is_whitespace() {
            if !token.is_empty() {
                out.push_str(&colorize_path_token(&token));
                token.clear();
            }
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    if !token.is_empty() {
        out.push_str(&colorize_path_token(&token));
    }
    out
}

fn colorize_path_token(token: &str) -> String {
    let (prefix, core, suffix) = strip_token_wrappers(token);
    if !looks_like_path(core) {
        return token.to_string();
    }
    format!("{}{}\x1b[0m{}", prefix, format!("\x1b[36m{}", core), suffix)
}

fn strip_token_wrappers(token: &str) -> (&str, &str, &str) {
    let mut start = 0usize;
    let chars: Vec<char> = token.chars().collect();
    let mut end = chars.len();

    if chars.is_empty() {
        return ("", "", "");
    }

    while start < end {
        let c = chars[start];
        if matches!(c, '`' | '\'' | '"' | '(' | '[' | '{') {
            start += 1;
        } else {
            break;
        }
    }
    while end > start {
        let c = chars[end - 1];
        if matches!(
            c,
            '`' | '\'' | '"' | ')' | ']' | '}' | ',' | '.' | ':' | ';'
        ) {
            end -= 1;
        } else {
            break;
        }
    }

    let prefix = &token[..token
        .char_indices()
        .nth(start)
        .map(|(i, _)| i)
        .unwrap_or(token.len())];
    let core_start = prefix.len();
    let core_end = token
        .char_indices()
        .nth(end)
        .map(|(i, _)| i)
        .unwrap_or(token.len());
    let core = &token[core_start..core_end];
    let suffix = &token[core_end..];
    (prefix, core, suffix)
}

fn looks_like_path(core: &str) -> bool {
    if core.is_empty() {
        return false;
    }
    if core.starts_with("--") {
        return false;
    }

    core.contains('/')
        || core.starts_with("./")
        || core.starts_with(".claudeform/")
        || core.starts_with("src/")
        || core.starts_with("crates/")
        || core.ends_with(".md")
        || core.ends_with(".rs")
        || core.ends_with(".json")
        || core.ends_with(".toml")
        || core.ends_with(".txt")
}

fn extract_startup_stderr_hint(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    let suspicious = [
        "failed to connect",
        "could not resolve host",
        "failed to lookup address information",
        "panicked",
        "error:",
    ];
    if !suspicious.iter().any(|needle| lower.contains(needle)) {
        return None;
    }

    Some(truncate_one_line(trimmed, 180))
}

impl ProviderRunResult {
    pub fn ensure_success(&self) -> Result<()> {
        if self.exit_code == Some(0) {
            return Ok(());
        }

        Err(anyhow!(
            "provider execution failed (exit={:?})\nstdout:\n{}\nstderr:\n{}",
            self.exit_code,
            self.stdout,
            self.stderr
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_thread_started_event() {
        let line = r#"{"type":"thread.started","thread_id":"abc"}"#;
        let ev = parse_codex_stream_line(line).expect("expected event");

        assert_eq!(
            ev,
            ProviderEvent::RunStarted {
                run_id: Some("abc".to_string())
            }
        );
    }

    #[test]
    fn run_started_event_renders_full_session_line() {
        let line = format_terminal_event(
            &ProviderEvent::RunStarted {
                run_id: Some("thread_123".to_string()),
            },
            true,
            true,
        )
        .expect("expected session line");
        assert_eq!(line, "session thread_123");
    }

    #[test]
    fn parses_item_completed_event() {
        let line =
            r#"{"type":"item.completed","item":{"id":"i1","type":"file_change","path":"out.txt"}}"#;
        let ev = parse_codex_stream_line(line).expect("expected event");

        assert_eq!(
            ev,
            ProviderEvent::ItemCompleted {
                item_type: "file_change".to_string(),
                item_id: Some("i1".to_string()),
                summary: Some("path=out.txt".to_string()),
            }
        );
    }

    #[test]
    fn parses_file_change_summary_from_changes_array() {
        let line = r#"{"type":"item.completed","item":{"id":"i2","type":"file_change","changes":[{"path":"src/main.rs","kind":"update"},{"path":"src/lib.rs","kind":"add"}]}}"#;
        let ev = parse_codex_stream_line(line).expect("expected event");
        let ProviderEvent::ItemCompleted { summary, .. } = ev else {
            panic!("expected item completed");
        };
        let summary = summary.unwrap_or_default();
        assert!(summary.contains("src/"));
        assert!(summary.contains("more"));
    }

    #[test]
    fn formats_turn_usage_line_with_turn_number() {
        let line = format_turn_usage_line(
            2,
            &ProviderUsage {
                input_tokens: Some(10),
                output_tokens: Some(3),
                cached_input_tokens: Some(2),
            },
        );
        assert_eq!(
            line.as_deref(),
            Some("turn 2 | tokens: in=10 out=3 cached=2")
        );
    }

    #[test]
    fn hides_turn_usage_line_when_usage_missing() {
        let line = format_turn_usage_line(1, &ProviderUsage::default());
        assert!(line.is_none());
    }

    #[test]
    fn codex_capabilities_exposed() {
        let caps = CodexRunner.capabilities();
        assert!(caps.live_events);
        assert!(caps.tool_call_events);
        assert!(caps.file_change_events);
        assert!(caps.resume);
    }

    #[test]
    fn sandbox_mode_labels_match_cli_values() {
        assert_eq!(SandboxMode::default(), SandboxMode::Auto);
        assert_eq!(SandboxMode::Auto.label(), "auto");
        assert_eq!(SandboxMode::Sandboxed.label(), "workspace-write");
        assert_eq!(SandboxMode::Unsandboxed.label(), "danger-full-access");
    }

    #[test]
    fn non_json_line_maps_to_raw_text_without_progress_line() {
        let ev = parse_codex_stream_line("OpenAI Codex v0.118.0").expect("expected raw text");
        assert!(matches!(ev, ProviderEvent::RawText { .. }));
        assert!(format_terminal_event(&ev, true, false).is_none());
    }

    #[test]
    fn extracts_stderr_connectivity_hint() {
        let hint = extract_startup_stderr_hint(
            "failed to lookup address information: nodename nor servname provided, or not known",
        );
        assert!(hint.is_some());
    }

    #[test]
    fn classifies_transient_failure_from_output() {
        let run = ProviderRunResult {
            session_id: None,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "HTTP error: 500 Internal Server Error".to_string(),
            usage: ProviderUsage::default(),
        };
        assert!(is_transient_codex_failure(&run));
    }

    #[test]
    fn classifies_transient_failure_from_error_message() {
        let err = anyhow!("stream disconnected before completion");
        assert!(is_transient_codex_error(&err));
    }

    #[test]
    fn classifies_sandbox_restriction_failure_from_output() {
        let run = ProviderRunResult {
            session_id: None,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "sandbox-exec: sandbox_apply: Operation not permitted".to_string(),
            usage: ProviderUsage::default(),
        };
        assert!(is_sandbox_restriction_failure(&run));
    }

    #[test]
    fn does_not_classify_generic_permission_denied_without_sandbox_context() {
        let run = ProviderRunResult {
            session_id: None,
            exit_code: Some(1),
            stdout: "Permission denied".to_string(),
            stderr: String::new(),
            usage: ProviderUsage::default(),
        };
        assert!(!is_sandbox_restriction_failure(&run));
    }

    #[test]
    fn classifies_network_restriction_failure_from_output() {
        let run = ProviderRunResult {
            session_id: None,
            exit_code: Some(1),
            stdout: "curl: (6) Could not resolve host: api.github.com".to_string(),
            stderr: String::new(),
            usage: ProviderUsage::default(),
        };
        assert!(is_network_restriction_failure(&run));
        assert!(should_retry_unsandboxed_after_failure(&run));
    }

    #[test]
    fn detects_blocked_network_message_in_successful_run_output() {
        let run = ProviderRunResult {
            session_id: None,
            exit_code: Some(0),
            stdout: "Blocked by network restrictions; required release note downloads unavailable. could not resolve host".to_string(),
            stderr: String::new(),
            usage: ProviderUsage::default(),
        };
        assert!(output_reports_blocked_network(&run));
    }

    #[test]
    fn does_not_detect_blocked_network_from_generic_restrictions_reasoning() {
        let run = ProviderRunResult {
            session_id: None,
            exit_code: Some(0),
            stdout: "I'm noticing network restrictions likely prevent direct Python or curl access to resources."
                .to_string(),
            stderr: String::new(),
            usage: ProviderUsage::default(),
        };
        assert!(!output_reports_blocked_network(&run));
    }

    #[test]
    fn reads_recent_agent_result_for_blocked_network_detection() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".claudeform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","message":"Blocked by network restrictions; could not resolve host"}"#,
        )
        .expect("write agent_result");

        assert!(agent_result_reports_blocked_network(
            dir.path(),
            ".claudeform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn reads_recent_agent_result_for_network_detection_without_blocked_keyword() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".claudeform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","message":"curl failed: Could not resolve host: example.com"}"#,
        )
        .expect("write agent_result");

        assert!(agent_result_reports_blocked_network(
            dir.path(),
            ".claudeform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn reads_recent_agent_result_for_reason_keyword_detection() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".claudeform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"sandbox_network_blocked","message":"blocked"}"#,
        )
        .expect("write agent_result");

        assert!(agent_result_reports_blocked_network(
            dir.path(),
            ".claudeform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn ignores_reason_keyword_when_status_is_success() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".claudeform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"success","reason":"sandbox_network_blocked","message":"done"}"#,
        )
        .expect("write agent_result");

        assert!(!agent_result_reports_blocked_network(
            dir.path(),
            ".claudeform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn detects_early_auto_retry_reason_from_agent_result() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".claudeform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"sandbox_network_blocked","message":"curl failed"}"#,
        )
        .expect("write agent_result");

        let monitor = EarlyAutoRetryMonitor {
            agent_result_path: path,
            run_started_at: started,
        };
        assert_eq!(
            detect_early_auto_retry_reason(Some(&monitor)).as_deref(),
            Some("sandbox_network_blocked")
        );
    }

    #[test]
    fn early_auto_retry_ignores_stale_agent_result() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".claudeform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"sandbox_network_blocked","message":"curl failed"}"#,
        )
        .expect("write agent_result");
        let started = std::time::SystemTime::now()
            .checked_add(std::time::Duration::from_millis(250))
            .expect("future ts");
        let monitor = EarlyAutoRetryMonitor {
            agent_result_path: path,
            run_started_at: started,
        };
        assert!(detect_early_auto_retry_reason(Some(&monitor)).is_none());
    }

    #[test]
    fn retries_unsandboxed_when_failed_run_has_blocked_network_in_agent_result() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace_root = dir.path().to_path_buf();
        let result_path =
            workspace_root.join(".claudeform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = result_path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &result_path,
            r#"{"status":"failure","message":"Blocked by network restrictions; required release note downloads unavailable"}"#,
        )
        .expect("write agent_result");

        let request = ProviderRequest {
            workspace_root,
            artifacts_root: None,
            program_id: Some("release-notes".to_string()),
            model: None,
            agent_result_rel: ".claudeform/programs/release-notes/reports/agent_result.json"
                .to_string(),
            sandbox_mode: SandboxMode::Auto,
            prompt: "x".to_string(),
            progress: true,
            render_progress: false,
            verbose_events: false,
            interactive_ui: false,
            show_intermediate_steps: false,
        };
        let run = ProviderRunResult {
            session_id: None,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: String::new(),
            usage: ProviderUsage::default(),
        };

        assert!(should_retry_unsandboxed_after_failure_with_agent_result(
            &request, &run, started
        ));
    }

    #[test]
    fn success_path_retries_when_agent_result_reports_network_restriction() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace_root = dir.path().to_path_buf();
        let result_rel = ".claudeform/programs/release-notes/reports/agent_result.json";
        let result_path = workspace_root.join(result_rel);
        if let Some(parent) = result_path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }

        let request = ProviderRequest {
            workspace_root: workspace_root.clone(),
            artifacts_root: None,
            program_id: Some("release-notes".to_string()),
            model: None,
            agent_result_rel: result_rel.to_string(),
            sandbox_mode: SandboxMode::Auto,
            prompt: "x".to_string(),
            progress: true,
            render_progress: false,
            verbose_events: false,
            interactive_ui: false,
            show_intermediate_steps: false,
        };

        let started_no_file = std::time::SystemTime::now();
        assert!(!should_retry_unsandboxed_after_success(
            &request,
            started_no_file
        ));

        let started_with_file = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &result_path,
            r#"{"status":"failure","message":"curl failed: Could not resolve host: example.com"}"#,
        )
        .expect("write agent_result");

        assert!(should_retry_unsandboxed_after_success(
            &request,
            started_with_file
        ));
    }

    #[test]
    fn hides_item_events_in_non_verbose_mode() {
        let line = format_terminal_event(
            &ProviderEvent::ItemStarted {
                item_type: "command_execution".to_string(),
                item_id: Some("x".to_string()),
                summary: Some("cmd=ls".to_string()),
            },
            false,
            false,
        );
        assert!(line.is_none());
    }

    #[test]
    fn hides_agent_text_events_by_default() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "assistant_message".to_string(),
                item_id: Some("m1".to_string()),
                summary: Some(
                    "Here is a compact update about the file changes I am applying.".to_string(),
                ),
            },
            true,
            false,
        );
        assert!(line.is_none());
    }

    #[test]
    fn shows_agent_text_when_intermediate_enabled() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "assistant_message".to_string(),
                item_id: Some("m1".to_string()),
                summary: Some("Updated `src/main.rs` with fix".to_string()),
            },
            true,
            true,
        )
        .expect("expected text line");
        assert!(line.starts_with("💬 "));
    }

    #[test]
    fn shows_reasoning_text_with_reasoning_symbol() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "reasoning".to_string(),
                item_id: Some("r1".to_string()),
                summary: Some("Planning approach for patch.".to_string()),
            },
            true,
            true,
        )
        .expect("expected reasoning line");
        assert!(line.starts_with("💭 "));
    }

    #[test]
    fn filters_low_signal_read_only_command() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "command_execution".to_string(),
                item_id: Some("x".to_string()),
                summary: Some("ls .claudeform".to_string()),
            },
            true,
            false,
        );
        assert!(line.is_none());
    }

    #[test]
    fn shows_intermediate_command_when_enabled() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "command_execution".to_string(),
                item_id: Some("x".to_string()),
                summary: Some("ls src".to_string()),
            },
            true,
            true,
        )
        .expect("expected command line");
        assert!(line.contains("ls src"));
    }

    #[test]
    fn hides_command_events_when_intermediate_disabled() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "command_execution".to_string(),
                item_id: Some("x".to_string()),
                summary: Some("cargo test -q".to_string()),
            },
            true,
            false,
        );
        assert!(line.is_none());
    }

    #[test]
    fn hides_session_line_when_intermediate_disabled() {
        let line = format_terminal_event(
            &ProviderEvent::RunStarted {
                run_id: Some("thread_123".to_string()),
            },
            true,
            false,
        );
        assert!(line.is_none());
    }

    #[test]
    fn hides_housekeeping_commands_even_when_intermediate_enabled() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "command_execution".to_string(),
                item_id: Some("x".to_string()),
                summary: Some(
                    "write .claudeform/programs/release-notes/reports/agent_result.json"
                        .to_string(),
                ),
            },
            true,
            true,
        );
        assert!(line.is_none());
    }

    #[test]
    fn housekeeping_commands_do_not_count_as_progress_activity() {
        assert!(!should_count_item_progress(
            "command_execution",
            Some("write .claudeform/programs/release-notes/reports/agent_outputs.json"),
            true
        ));
        assert!(!should_count_item_progress(
            "command_execution",
            Some("cat .claudeform/programs/release-notes/reports/agent_result.json"),
            false
        ));
    }

    #[test]
    fn simplify_command_summary_extracts_write_target() {
        let cmd = r#"/bin/zsh -lc "cd /tmp/work && cat <<'EOF' > example-data/output-smoke.txt SMOKE_OK EOF""#;
        let summary = simplify_command_summary(cmd);
        assert_eq!(summary, "write example-data/output-smoke.txt");
    }

    #[test]
    fn strip_token_wrappers_handles_unicode_without_panic() {
        let raw = "💬 `example-data/output-smoke.txt`";
        let (prefix, core, suffix) = strip_token_wrappers("💬 `example-data/output-smoke.txt`");
        assert_eq!(format!("{}{}{}", prefix, core, suffix), raw);
        assert!(core.contains("example-data/output-smoke.txt"));
    }

    #[test]
    fn colorize_paths_handles_unicode_prefix_without_panic() {
        let rendered = colorize_paths("💬 updated `example-data/output-smoke.txt`");
        assert!(rendered.contains("example-data/output-smoke.txt"));
    }

    #[test]
    fn extracts_command_output_payload_from_item_completed() {
        let line = r#"{"type":"item.completed","item":{"id":"item_7","type":"command_execution","command":"/bin/zsh -lc ls","aggregated_output":"a\nb\n","status":"completed","exit_code":0}}"#;
        let payload = extract_command_output_payload(line).expect("expected payload");
        assert_eq!(payload.item_id, "item_7");
        assert_eq!(payload.command.as_deref(), Some("/bin/zsh -lc ls"));
        assert_eq!(payload.output, "a\nb\n");
    }

    #[test]
    fn adds_plain_output_suffix_for_completed_command() {
        let event = ProviderEvent::ItemCompleted {
            item_type: "command_execution".to_string(),
            item_id: Some("item_9".to_string()),
            summary: Some("ls".to_string()),
        };
        let mut links = HashMap::new();
        links.insert(
            "item_9".to_string(),
            PathBuf::from("/tmp/claudeform/programs/smoke/sessions/session/commands/item_9.txt"),
        );
        let rendered = add_command_output_link_suffix(&event, "✔ ls", &links, false);
        assert!(rendered.contains(
            "✔ ls | out=/tmp/claudeform/programs/smoke/sessions/session/commands/item_9.txt"
        ));
    }

    #[test]
    fn extracts_message_output_payload_from_item_completed() {
        let line = r#"{"type":"item.completed","item":{"id":"item_4","type":"assistant_message","text":"Detailed final message with context.\nAnd second line."}}"#;
        let payload = extract_message_output_payload(line).expect("expected payload");
        assert_eq!(payload.item_id, "item_4");
        assert_eq!(payload.item_type, "assistant_message");
        assert!(payload.text.starts_with("Detailed final message"));
    }

    #[test]
    fn adds_plain_output_suffix_for_message_event() {
        let event = ProviderEvent::ItemCompleted {
            item_type: "assistant_message".to_string(),
            item_id: Some("item_4".to_string()),
            summary: Some("Some summary".to_string()),
        };
        let mut links = HashMap::new();
        links.insert(
            "item_4".to_string(),
            PathBuf::from("/tmp/claudeform/programs/smoke/sessions/session/messages/item_4.md"),
        );
        let rendered = add_message_output_link_suffix(&event, "💬 Some summary", &links, false);
        assert!(rendered.contains(
            "💬 Some summary | msg=/tmp/claudeform/programs/smoke/sessions/session/messages/item_4.md"
        ));
    }

    #[test]
    fn adds_plain_output_suffix_for_file_change_event() {
        let event = ProviderEvent::ItemCompleted {
            item_type: "file_change".to_string(),
            item_id: Some("item_5".to_string()),
            summary: Some("update src/main.rs".to_string()),
        };
        let mut links = HashMap::new();
        links.insert(
            "item_5".to_string(),
            PathBuf::from("/tmp/claudeform/src/main.rs"),
        );
        let rendered =
            add_file_change_link_suffix(&event, "✔ file update src/main.rs", &links, false);
        assert!(rendered.contains("✔ file update src/main.rs | file=/tmp/claudeform/src/main.rs"));
    }

    #[test]
    fn colorize_done_payload_dims_duration_segment_before_out_link() {
        let rendered = colorize_done_payload("write example-data/output-smoke.txt | 1ms | out");
        assert!(rendered.contains("\x1b[2m1ms\x1b[0m"));
    }

    #[test]
    fn colorize_done_payload_dims_trailing_duration_without_pipe() {
        let rendered = colorize_done_payload("write example-data/output-smoke.txt 1ms");
        assert!(rendered.contains("\x1b[2m1ms\x1b[0m"));
    }

    #[test]
    fn colorize_done_payload_highlights_out_link_segment() {
        let rendered = colorize_done_payload("write example-data/output-smoke.txt | 1ms | out");
        assert!(rendered.contains("\x1b[95mout\x1b[0m"));
    }
}
