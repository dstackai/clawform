use std::collections::HashMap;
use std::env;
use std::fs;
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

use crate::config::ProviderKind;
use crate::path_utils::to_slash_path;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

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
    pub debug_mode: bool,
    pub verbose_output: bool,
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
    pub turn_count: u64,
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

    pub fn claude_v0() -> Self {
        Self {
            live_events: true,
            partial_text: false,
            tool_call_events: true,
            file_change_events: false,
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
#[derive(Debug, Default, Clone)]
pub struct ClaudeRunner;
static CODEX_RUNNER: CodexRunner = CodexRunner;
static CLAUDE_RUNNER: ClaudeRunner = ClaudeRunner;

pub fn resolve_provider_runner(provider_type: ProviderKind) -> Result<&'static dyn ProviderRunner> {
    match provider_type {
        ProviderKind::Codex => Ok(&CODEX_RUNNER),
        ProviderKind::Claude => Ok(&CLAUDE_RUNNER),
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeExecutionMode {
    Sandboxed,
    Unsandboxed,
}

impl ClaudeExecutionMode {
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
const EARLY_AUTO_RETRY_REASON_SANDBOX_BLOCKED: &str = "sandbox_blocked";
static INTERRUPT_REQUESTED: AtomicBool = AtomicBool::new(false);
static CTRL_C_HANDLER_INIT: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AgentResultStatus {
    Success,
    Partial,
    Failure,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AgentResultReason {
    SandboxBlocked,
    ProgramBlocked,
}

#[derive(Debug, Clone, Deserialize)]
struct AgentResultProtocolFile {
    status: AgentResultStatus,
    #[serde(default)]
    reason: Option<AgentResultReason>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClaudeJsonResult {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    num_turns: u64,
    #[serde(default)]
    usage: ClaudeUsage,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

impl ClaudeUsage {
    fn into_provider_usage(self) -> ProviderUsage {
        ProviderUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cache_read_input_tokens,
        }
    }
}

#[derive(Debug, Clone)]
struct ClaudePendingTool {
    item_type: String,
    item_id: String,
    summary: Option<String>,
    command: Option<String>,
    paths: Vec<String>,
    emits_command_output: bool,
}

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

fn session_base_dir(root: &Path, program_id: Option<&str>, session_id: &str) -> PathBuf {
    let program = sanitize_program_id(program_id.unwrap_or("program"));
    let session = sanitize_session_id(session_id);
    root.join(".clawform")
        .join("programs")
        .join(program)
        .join("sessions")
        .join(session)
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
                print_auto_sandbox_retry_decision(request, sandbox_started_at, &sandboxed);
                return run_codex_with_retries(request, CodexExecutionMode::Unsandboxed);
            }
            print_auto_sandbox_turn_usage_line(&sandboxed);
            return Ok(sandboxed);
        }

        if should_retry_unsandboxed_after_failure_with_agent_result(request, sandbox_started_at) {
            print_auto_sandbox_retry_decision(request, sandbox_started_at, &sandboxed);
            return run_codex_with_retries(request, CodexExecutionMode::Unsandboxed);
        }

        print_auto_sandbox_turn_usage_line(&sandboxed);
        Ok(sandboxed)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::codex_v0()
    }
}

impl ProviderRunner for ClaudeRunner {
    fn run(&self, request: &ProviderRequest) -> Result<ProviderRunResult> {
        match request.sandbox_mode {
            SandboxMode::Sandboxed => {
                return run_claude_once(request, ClaudeExecutionMode::Sandboxed);
            }
            SandboxMode::Unsandboxed => {
                return run_claude_once(request, ClaudeExecutionMode::Unsandboxed);
            }
            SandboxMode::Auto => {}
        }

        let sandbox_started_at = SystemTime::now();
        let sandboxed = run_claude_once(request, ClaudeExecutionMode::Sandboxed)?;
        if sandboxed.exit_code == Some(0) {
            if should_retry_unsandboxed_after_success(request, sandbox_started_at) {
                print_auto_sandbox_retry_decision(request, sandbox_started_at, &sandboxed);
                return run_claude_once(request, ClaudeExecutionMode::Unsandboxed);
            }
            return Ok(sandboxed);
        }

        if should_retry_unsandboxed_after_failure_with_agent_result(request, sandbox_started_at) {
            print_auto_sandbox_retry_decision(request, sandbox_started_at, &sandboxed);
            return run_claude_once(request, ClaudeExecutionMode::Unsandboxed);
        }

        Ok(sandboxed)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::claude_v0()
    }
}

fn run_claude_once(
    request: &ProviderRequest,
    mode: ClaudeExecutionMode,
) -> Result<ProviderRunResult> {
    ensure_interrupt_handler()?;
    clear_interrupt_request();
    clear_agent_result_protocol_file(&request.workspace_root, request.agent_result_rel.as_str())?;

    let mut cmd = Command::new("claude");
    cmd.arg("-p")
        .arg("--input-format")
        .arg("text")
        .arg("--permission-mode")
        .arg("bypassPermissions")
        .current_dir(&request.workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if request.progress {
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");
    } else {
        cmd.arg("--output-format").arg("json");
    }

    if let Some(model) = &request.model {
        cmd.arg("--model").arg(model);
    }

    if let Some(settings) = claude_settings_json(mode) {
        cmd.arg("--settings").arg(settings);
    }

    let mut child = cmd
        .spawn()
        .context("failed launching provider command 'claude'")?;

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
        return collect_claude_with_progress(
            child,
            request.render_progress,
            request.verbose_output || request.debug_mode,
            request.verbose_output,
            request.verbose_events,
            request.interactive_ui,
            request.show_intermediate_steps,
            mode,
            &request.workspace_root,
            request.artifacts_root.as_deref(),
            request.program_id.as_deref(),
        );
    }

    let output = wait_with_output_interruptible(child)?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    parse_claude_json_result(stdout, stderr, output.status.code())
}

fn claude_settings_json(mode: ClaudeExecutionMode) -> Option<String> {
    match mode {
        ClaudeExecutionMode::Sandboxed => Some(
            json!({
                "sandbox": {
                    "enabled": true,
                    "autoAllowBashIfSandboxed": true,
                    "allowUnsandboxedCommands": false,
                    "failIfUnavailable": true
                }
            })
            .to_string(),
        ),
        ClaudeExecutionMode::Unsandboxed => None,
    }
}

fn parse_claude_json_result(
    stdout: String,
    mut stderr: String,
    process_exit_code: Option<i32>,
) -> Result<ProviderRunResult> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("claude returned empty stdout in json mode"));
    }

    let parsed: ClaudeJsonResult =
        serde_json::from_str(trimmed).context("failed parsing Claude JSON result")?;
    let mut exit_code = process_exit_code;

    if parsed.is_error {
        if stderr.trim().is_empty() && !parsed.result.trim().is_empty() {
            stderr = parsed.result.clone();
        } else if !parsed.result.trim().is_empty() && !stderr.contains(parsed.result.as_str()) {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(parsed.result.as_str());
        }
        if exit_code == Some(0) {
            exit_code = Some(1);
        }
    }

    let session_id = parsed
        .session_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .map(sanitize_session_id)
        .unwrap_or_else(|| format!("local-{}", now_unix_millis()));

    Ok(ProviderRunResult {
        session_id: Some(session_id),
        exit_code,
        stdout,
        stderr,
        usage: parsed.usage.into_provider_usage(),
        turn_count: parsed.num_turns,
    })
}

fn collect_claude_with_progress(
    mut child: std::process::Child,
    render_progress: bool,
    show_housekeeping: bool,
    verbose_output: bool,
    verbose_events: bool,
    interactive_ui: bool,
    show_intermediate_steps: bool,
    execution_mode: ClaudeExecutionMode,
    workspace_root: &Path,
    artifacts_root: Option<&Path>,
    program_id: Option<&str>,
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
    let mut item_started_at: HashMap<String, Instant> = HashMap::new();
    let mut usage_totals = ProviderUsage::default();
    let mut printer = ProgressPrinter::new(render_progress && interactive_ui);
    let mut last_heartbeat_at = Instant::now();
    let mut status = None;
    let mut channel_closed = false;
    let mut turn_index: u64 = 0;
    let mut session_id: Option<String> = None;
    let mut last_visible_turn_message_id: Option<String> = None;
    let mut final_result_line: Option<String> = None;
    let mut pending_tools: HashMap<String, ClaudePendingTool> = HashMap::new();
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

    macro_rules! emit_event {
        ($event:expr, $command_payload:expr, $message_payload:expr, $file_payload:expr $(,)?) => {
            handle_progress_event(
                &$event,
                $command_payload,
                $message_payload,
                $file_payload,
                render_progress,
                show_housekeeping,
                verbose_output,
                verbose_events,
                show_intermediate_steps,
                execution_mode.label(),
                turn_index,
                program_id,
                &mut session_id,
                &mut emitted_progress_events,
                &mut last_activity,
                &mut active_progress_items,
                &mut last_agent_text_line,
                &mut item_started_at,
                &mut usage_totals,
                &mut printer,
                &sink,
                &mut command_output_links,
                &mut message_output_links,
                &mut file_change_links,
                supports_hyperlinks,
                artifacts_root,
            )?
        };
    }

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

                if is_stdout {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    match value.get("type").and_then(Value::as_str) {
                        Some("system")
                            if value.get("subtype").and_then(Value::as_str) == Some("init") =>
                        {
                            let run_id = value
                                .get("session_id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                            emit_event!(ProviderEvent::RunStarted { run_id }, None, None, None);
                        }
                        Some("assistant") => {
                            let message = value.get("message").unwrap_or(&Value::Null);
                            if let Some(message_id) = claude_message_id(message) {
                                if last_visible_turn_message_id.as_deref()
                                    != Some(message_id.as_str())
                                {
                                    last_visible_turn_message_id = Some(message_id);
                                    turn_index = turn_index.saturating_add(1);
                                    emit_event!(
                                        ProviderEvent::TurnCompleted {
                                            usage: claude_message_usage(message),
                                        },
                                        None,
                                        None,
                                        None,
                                    );
                                }
                            }

                            if let Some(contents) = message.get("content").and_then(Value::as_array)
                            {
                                for (idx, content) in contents.iter().enumerate() {
                                    match content.get("type").and_then(Value::as_str) {
                                        Some("thinking") => {
                                            let text = content
                                                .get("thinking")
                                                .and_then(Value::as_str)
                                                .map(str::trim)
                                                .unwrap_or_default();
                                            if text.is_empty() {
                                                continue;
                                            }
                                            let item_id = claude_stream_event_item_id(
                                                &value, idx, "thinking",
                                            );
                                            let message_payload = MessageOutputPayload {
                                                item_id: item_id.clone(),
                                                item_type: "reasoning".to_string(),
                                                text: text.to_string(),
                                            };
                                            emit_event!(
                                                ProviderEvent::ItemCompleted {
                                                    item_type: "reasoning".to_string(),
                                                    item_id: Some(item_id),
                                                    summary: Some(truncate_one_line(text, 180)),
                                                },
                                                None,
                                                Some(&message_payload),
                                                None,
                                            );
                                        }
                                        Some("text") => {
                                            let text = content
                                                .get("text")
                                                .and_then(Value::as_str)
                                                .map(str::trim)
                                                .unwrap_or_default();
                                            if text.is_empty() {
                                                continue;
                                            }
                                            let item_id =
                                                claude_stream_event_item_id(&value, idx, "text");
                                            let message_payload = MessageOutputPayload {
                                                item_id: item_id.clone(),
                                                item_type: "assistant_message".to_string(),
                                                text: text.to_string(),
                                            };
                                            emit_event!(
                                                ProviderEvent::ItemCompleted {
                                                    item_type: "assistant_message".to_string(),
                                                    item_id: Some(item_id),
                                                    summary: Some(truncate_one_line(text, 180)),
                                                },
                                                None,
                                                Some(&message_payload),
                                                None,
                                            );
                                        }
                                        Some("tool_use") => {
                                            let Some(tool) =
                                                claude_pending_tool(content, workspace_root)
                                            else {
                                                continue;
                                            };
                                            emit_event!(
                                                ProviderEvent::ItemStarted {
                                                    item_type: tool.item_type.clone(),
                                                    item_id: Some(tool.item_id.clone()),
                                                    summary: tool.summary.clone(),
                                                },
                                                None,
                                                None,
                                                None,
                                            );
                                            pending_tools.insert(tool.item_id.clone(), tool);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        Some("user") => {
                            let Some(content_items) = value
                                .get("message")
                                .and_then(|m| m.get("content"))
                                .and_then(Value::as_array)
                            else {
                                continue;
                            };
                            for content in content_items {
                                if content.get("type").and_then(Value::as_str)
                                    != Some("tool_result")
                                {
                                    continue;
                                }
                                let Some(tool_use_id) =
                                    content.get("tool_use_id").and_then(Value::as_str)
                                else {
                                    continue;
                                };
                                let Some(tool) = pending_tools.remove(tool_use_id) else {
                                    continue;
                                };

                                let command_payload = if tool.emits_command_output {
                                    claude_command_output_payload(
                                        &tool,
                                        content,
                                        value.get("tool_use_result"),
                                    )
                                } else {
                                    None
                                };
                                let file_payload = if tool.paths.is_empty() {
                                    None
                                } else {
                                    Some(FileChangePayload {
                                        item_id: tool.item_id.clone(),
                                        paths: tool.paths.clone(),
                                    })
                                };

                                emit_event!(
                                    ProviderEvent::ItemCompleted {
                                        item_type: tool.item_type,
                                        item_id: Some(tool.item_id),
                                        summary: tool.summary,
                                    },
                                    command_payload.as_ref(),
                                    None,
                                    file_payload.as_ref(),
                                );
                            }
                        }
                        Some("result") => {
                            final_result_line = Some(line);
                        }
                        _ => {}
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
            if let Some(done) = child
                .try_wait()
                .context("failed while polling provider process")?
            {
                status = Some(done);
            } else if render_progress
                && show_intermediate_steps
                && Instant::now().duration_since(last_heartbeat_at) >= heartbeat_interval
            {
                printer.print_status(&format_status_line(&last_activity));
                last_heartbeat_at = Instant::now();
            }
        }
    }

    join_reader(stdout_handle, "stdout")?;
    join_reader(stderr_handle, "stderr")?;

    if render_progress && show_intermediate_steps && emitted_progress_events == 0 {
        printer.print_event("no_live_events");
    }
    printer.finish();

    let Some(final_result_line) = final_result_line else {
        return Err(anyhow!(
            "claude stream-json run did not emit a final result event"
        ));
    };

    let mut parsed = parse_claude_json_result(
        final_result_line,
        raw_stderr.clone(),
        status.and_then(|s| s.code()),
    )?;
    if parsed
        .session_id
        .as_deref()
        .map(|id| id.starts_with("local-"))
        .unwrap_or(false)
    {
        if let Some(seen) = session_id {
            parsed.session_id = Some(sanitize_session_id(&seen));
        }
    }
    parsed.stdout = raw_stdout;
    parsed.stderr = if parsed.stderr.is_empty() {
        raw_stderr
    } else {
        parsed.stderr
    };
    if parsed.turn_count == 0 {
        parsed.turn_count = turn_index;
    }
    if parsed.usage.input_tokens.is_none()
        && parsed.usage.output_tokens.is_none()
        && parsed.usage.cached_input_tokens.is_none()
    {
        parsed.usage = usage_totals;
    }

    Ok(parsed)
}

#[allow(clippy::too_many_arguments)]
fn handle_progress_event(
    normalized: &ProviderEvent,
    command_payload: Option<&CommandOutputPayload>,
    message_payload: Option<&MessageOutputPayload>,
    file_payload: Option<&FileChangePayload>,
    render_progress: bool,
    show_housekeeping: bool,
    verbose_output: bool,
    verbose_events: bool,
    show_intermediate_steps: bool,
    execution_mode_label: &str,
    turn_index: u64,
    program_id: Option<&str>,
    session_id: &mut Option<String>,
    emitted_progress_events: &mut usize,
    last_activity: &mut String,
    active_progress_items: &mut Vec<(String, String)>,
    last_agent_text_line: &mut Option<String>,
    item_started_at: &mut HashMap<String, Instant>,
    usage_totals: &mut ProviderUsage,
    printer: &mut ProgressPrinter,
    sink: &CommandOutputSink,
    command_output_links: &mut HashMap<String, PathBuf>,
    message_output_links: &mut HashMap<String, PathBuf>,
    file_change_links: &mut HashMap<String, PathBuf>,
    supports_hyperlinks: bool,
    artifacts_root: Option<&Path>,
) -> Result<()> {
    match normalized {
        ProviderEvent::RunStarted { run_id } => {
            if let Some(id) = run_id.as_ref() {
                *session_id = Some(id.clone());
            }
        }
        ProviderEvent::TurnStarted => {}
        ProviderEvent::ItemStarted {
            item_id,
            item_type,
            summary,
        } => {
            if let Some(id) = item_id.clone() {
                item_started_at.insert(id, Instant::now());
            }
            if should_count_item_progress(
                item_type,
                summary.as_deref(),
                show_housekeeping,
                show_intermediate_steps,
            ) {
                let label = status_activity_label(item_type, summary.as_deref());
                if let Some(id) = item_id.as_ref() {
                    active_progress_items.retain(|(active_id, _)| active_id != id);
                    active_progress_items.push((id.clone(), label.clone()));
                }
                *last_activity = label;
            }
        }
        ProviderEvent::ItemCompleted {
            item_id,
            item_type,
            summary,
        } => {
            if should_count_item_progress(
                item_type,
                summary.as_deref(),
                show_housekeeping,
                show_intermediate_steps,
            ) {
                if let Some(id) = item_id.as_ref() {
                    active_progress_items.retain(|(active_id, _)| active_id != id);
                }
                *last_activity = active_progress_items
                    .last()
                    .map(|(_, label)| label.clone())
                    .unwrap_or_default();
            }
        }
        ProviderEvent::TurnCompleted { usage } => {
            merge_usage(usage_totals, usage);
        }
        _ => {}
    }

    if !matches!(normalized, ProviderEvent::RawText { .. }) {
        *emitted_progress_events += 1;
    }

    if let Some(payload) = command_payload {
        if let Ok(Some(path)) = sink.persist(program_id, session_id.as_deref(), payload) {
            command_output_links.insert(payload.item_id.clone(), path);
        }
    }
    if let Some(payload) = message_payload {
        if let Ok(Some(path)) = sink.persist_message(program_id, session_id.as_deref(), payload) {
            message_output_links.insert(payload.item_id.clone(), path);
        }
    }
    if let Some(payload) = file_payload {
        if let Some(first_path) = payload.paths.first() {
            let path = make_clickable_path(first_path, artifacts_root);
            file_change_links.insert(payload.item_id.clone(), path);
        }
    }

    let completion_duration = item_completion_duration_label(normalized, item_started_at);
    let mut progress_line = format_terminal_event(
        normalized,
        verbose_events,
        show_housekeeping,
        show_intermediate_steps,
    );
    if show_intermediate_steps && progress_line.is_none() {
        if let ProviderEvent::TurnCompleted { usage } = normalized {
            progress_line = format_turn_usage_line(turn_index, usage);
        }
    }

    if let Some(progress_line) = progress_line {
        let progress_line = if matches!(normalized, ProviderEvent::RunStarted { .. }) {
            format!("{} | {}", progress_line, execution_mode_label)
        } else {
            progress_line
        };
        let progress_line = add_completion_duration_suffix(&progress_line, completion_duration);
        let progress_line = if verbose_output {
            expand_verbose_progress_line(
                normalized,
                &progress_line,
                command_payload,
                message_payload,
            )
        } else {
            let progress_line = add_command_output_link_suffix(
                normalized,
                &progress_line,
                command_output_links,
                supports_hyperlinks,
            );
            add_message_output_link_suffix(
                normalized,
                &progress_line,
                message_output_links,
                supports_hyperlinks,
            )
        };
        let progress_line = add_file_change_link_suffix(
            normalized,
            &progress_line,
            file_change_links,
            supports_hyperlinks,
        );
        if is_text_event_line(&progress_line)
            && last_agent_text_line.as_deref() == Some(progress_line.as_str())
        {
            return Ok(());
        }
        if is_text_event_line(&progress_line) {
            *last_agent_text_line = Some(progress_line.clone());
        }
        if render_progress {
            printer.print_event(&progress_line);
        }
    }

    Ok(())
}

fn claude_message_id(message: &Value) -> Option<String> {
    message
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
}

fn claude_message_usage(message: &Value) -> ProviderUsage {
    let usage = message.get("usage").unwrap_or(&Value::Null);
    ProviderUsage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        cached_input_tokens: usage.get("cache_read_input_tokens").and_then(Value::as_u64),
    }
}

fn claude_stream_event_item_id(value: &Value, idx: usize, fallback: &str) -> String {
    let base = value
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(sanitize_item_id)
        .unwrap_or_else(|| format!("{}-{}", fallback, now_unix_millis()));
    format!("{}-{}", base, idx)
}

fn claude_pending_tool(content: &Value, workspace_root: &Path) -> Option<ClaudePendingTool> {
    let name = content.get("name").and_then(Value::as_str)?.trim();
    let item_id = content.get("id").and_then(Value::as_str)?.trim();
    if item_id.is_empty() {
        return None;
    }
    let input = content.get("input").unwrap_or(&Value::Null);

    let (item_type, summary, command, paths, emits_command_output) = match name {
        "Bash" => {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let summary = command
                .as_deref()
                .map(simplify_command_summary)
                .or_else(|| {
                    input
                        .get("description")
                        .and_then(Value::as_str)
                        .map(|desc| truncate_one_line(desc, 180))
                });
            (
                "command_execution".to_string(),
                summary,
                command,
                Vec::new(),
                true,
            )
        }
        "Read" => {
            let path = input.get("file_path").and_then(Value::as_str)?;
            (
                "command_execution".to_string(),
                Some(format!(
                    "read {}",
                    display_path_in_workspace(path, workspace_root)
                )),
                None,
                Vec::new(),
                false,
            )
        }
        "Write" => {
            let path = input.get("file_path").and_then(Value::as_str)?;
            let path = path.trim().to_string();
            (
                "file_change".to_string(),
                Some(format!(
                    "write {}",
                    display_path_in_workspace(path.as_str(), workspace_root)
                )),
                None,
                vec![path],
                false,
            )
        }
        "Edit" | "NotebookEdit" => {
            let path = input
                .get("file_path")
                .and_then(Value::as_str)
                .or_else(|| input.get("notebook_path").and_then(Value::as_str))?;
            let path = path.trim().to_string();
            (
                "file_change".to_string(),
                Some(format!(
                    "edit {}",
                    display_path_in_workspace(path.as_str(), workspace_root)
                )),
                None,
                vec![path],
                false,
            )
        }
        "Glob" => (
            "command_execution".to_string(),
            input
                .get("pattern")
                .and_then(Value::as_str)
                .map(|pattern| format!("glob {}", truncate_one_line(pattern, 180))),
            None,
            Vec::new(),
            false,
        ),
        "Grep" => (
            "command_execution".to_string(),
            input
                .get("pattern")
                .and_then(Value::as_str)
                .map(|pattern| format!("grep {}", truncate_one_line(pattern, 180))),
            None,
            Vec::new(),
            false,
        ),
        "ToolSearch" | "WebSearch" | "WebFetch" => (
            "web_search".to_string(),
            input
                .get("query")
                .and_then(Value::as_str)
                .or_else(|| input.get("url").and_then(Value::as_str))
                .map(|value| truncate_one_line(value, 180)),
            None,
            Vec::new(),
            false,
        ),
        _ => (
            "mcp_tool_call".to_string(),
            Some(format!("tool={}", name)),
            None,
            Vec::new(),
            false,
        ),
    };

    Some(ClaudePendingTool {
        item_type,
        item_id: item_id.to_string(),
        summary,
        command,
        paths,
        emits_command_output,
    })
}

fn claude_command_output_payload(
    tool: &ClaudePendingTool,
    content: &Value,
    tool_use_result: Option<&Value>,
) -> Option<CommandOutputPayload> {
    let output = claude_tool_result_text(content, tool_use_result)?;
    Some(CommandOutputPayload {
        item_id: tool.item_id.clone(),
        command: tool.command.clone(),
        output,
    })
}

fn claude_tool_result_text(content: &Value, tool_use_result: Option<&Value>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(obj) = tool_use_result.and_then(Value::as_object) {
        if let Some(stdout) = obj
            .get("stdout")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            parts.push(stdout.to_string());
        }
        if let Some(stderr) = obj
            .get("stderr")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            parts.push(stderr.to_string());
        }
    }

    if let Some(text) = content
        .get("content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !is_low_signal_claude_tool_result_text(text)
            && parts.iter().all(|part| part.trim() != text)
        {
            parts.push(text.to_string());
        }
    }

    if parts.is_empty() {
        return None;
    }

    let output = parts.join("\n");
    if output.trim().is_empty() {
        None
    } else {
        Some(output)
    }
}

fn is_low_signal_claude_tool_result_text(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty() || (trimmed.starts_with("[rerun:") && trimmed.ends_with(']'))
}

fn display_path_in_workspace(raw_path: &str, workspace_root: &Path) -> String {
    let path = Path::new(raw_path.trim());
    if let Ok(relative) = path.strip_prefix(workspace_root) {
        return to_slash_path(relative);
    }
    to_slash_path(path)
}

fn print_auto_sandbox_turn_usage_line(run: &ProviderRunResult) {
    let Some(line) = format_turn_usage_line(run.turn_count, &run.usage) else {
        return;
    };
    let use_color = std::io::stdout().is_terminal();
    if use_color {
        println!("{}", format_turn_usage_event_line(&line, true));
    } else {
        println!("{}", format_turn_usage_event_line(&line, false));
    }
}

fn print_auto_sandbox_retry_decision(
    request: &ProviderRequest,
    run_started_at: SystemTime,
    run: &ProviderRunResult,
) {
    let result_path = agent_result_path(request);
    let parsed = result_path
        .as_ref()
        .and_then(|path| read_recent_agent_result_value(path.as_path(), run_started_at));
    let agent_result_suggests_sandbox_block = parsed
        .as_ref()
        .map(agent_result_reports_sandbox_block)
        .unwrap_or(false);
    let use_color = std::io::stdout().is_terminal();
    let line = format_auto_sandbox_retry_decision_line(
        run,
        parsed.as_ref(),
        agent_result_suggests_sandbox_block,
        result_path.as_deref(),
        supports_terminal_hyperlinks(),
        use_color,
    );
    let Some(line) = line else {
        return;
    };
    println!("{}", line);
}

fn format_auto_sandbox_retry_decision_line(
    run: &ProviderRunResult,
    agent_result: Option<&AgentResultProtocolFile>,
    agent_result_suggests_sandbox_block: bool,
    result_path: Option<&Path>,
    supports_hyperlinks: bool,
    use_color: bool,
) -> Option<String> {
    if let Some(parsed) = agent_result.filter(|_| agent_result_suggests_sandbox_block) {
        let sep = if use_color {
            " \x1b[2m|\x1b[0m "
        } else {
            " | "
        };
        let turn_line = format_turn_usage_line(run.turn_count, &run.usage).unwrap_or_else(|| {
            format!(
                "turn {}",
                if run.turn_count == 0 {
                    1
                } else {
                    run.turn_count
                }
            )
        });
        let turn_segment = if use_color {
            format!("\x1b[2m{}\x1b[0m", turn_line)
        } else {
            turn_line
        };
        let status = style_retry_decision_value(
            agent_result_status_label(parsed.status),
            retry_status_color(parsed.status),
            result_path,
            supports_hyperlinks,
            use_color,
        );
        let mut line = String::new();
        line.push_str(&turn_segment);
        match (parsed.status, parsed.reason) {
            (AgentResultStatus::Failure, Some(reason)) => {
                line.push_str(sep);
                line.push_str(
                    style_retry_decision_value(
                        agent_result_reason_label(reason),
                        retry_reason_color(reason),
                        result_path,
                        supports_hyperlinks,
                        use_color,
                    )
                    .as_str(),
                );
            }
            (_, maybe_reason) => {
                line.push_str(sep);
                line.push_str(&status);
                if let Some(reason) = maybe_reason {
                    line.push_str(sep);
                    line.push_str(
                        style_retry_decision_value(
                            agent_result_reason_label(reason),
                            retry_reason_color(reason),
                            result_path,
                            supports_hyperlinks,
                            use_color,
                        )
                        .as_str(),
                    );
                }
            }
        }
        if result_path.is_some() {
            line.push_str(sep);
            line.push_str(
                style_retry_decision_value(
                    "file",
                    "95",
                    result_path,
                    supports_hyperlinks,
                    use_color,
                )
                .as_str(),
            );
        }
        return Some(line);
    }

    None
}

fn style_retry_decision_value(
    raw: &str,
    color_code: &'static str,
    result_path: Option<&Path>,
    supports_hyperlinks: bool,
    use_color: bool,
) -> String {
    let linked = if supports_hyperlinks {
        result_path
            .and_then(|path| terminal_link(path, raw))
            .unwrap_or_else(|| raw.to_string())
    } else {
        raw.to_string()
    };
    if use_color {
        format!("\x1b[{}m{}\x1b[0m", color_code, linked)
    } else {
        linked
    }
}

fn retry_status_color(status: AgentResultStatus) -> &'static str {
    match status {
        AgentResultStatus::Success => "32",
        AgentResultStatus::Partial => "33",
        AgentResultStatus::Failure => "31",
    }
}

fn retry_reason_color(reason: AgentResultReason) -> &'static str {
    match reason {
        AgentResultReason::SandboxBlocked => "33",
        AgentResultReason::ProgramBlocked => "31",
    }
}

fn agent_result_path(request: &ProviderRequest) -> Option<PathBuf> {
    let rel = request.agent_result_rel.trim();
    if rel.is_empty() {
        return None;
    }
    Some(request.workspace_root.join(rel))
}

fn agent_result_status_label(status: AgentResultStatus) -> &'static str {
    match status {
        AgentResultStatus::Success => "success",
        AgentResultStatus::Partial => "partial",
        AgentResultStatus::Failure => "failure",
    }
}

fn agent_result_reason_label(reason: AgentResultReason) -> &'static str {
    match reason {
        AgentResultReason::SandboxBlocked => "sandbox_blocked",
        AgentResultReason::ProgramBlocked => "program_blocked",
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
    clear_agent_result_protocol_file(&request.workspace_root, request.agent_result_rel.as_str())?;
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
        let suppress_turn_usage_lines =
            mode == CodexExecutionMode::Sandboxed && request.sandbox_mode == SandboxMode::Auto;
        return collect_with_progress(
            child,
            request.render_progress,
            request.verbose_output || request.debug_mode,
            request.verbose_output,
            request.verbose_events,
            request.interactive_ui,
            request.show_intermediate_steps,
            suppress_turn_usage_lines,
            mode,
            request.artifacts_root.as_deref(),
            request.program_id.as_deref(),
            early_auto_retry_monitor,
        );
    }

    let output = wait_with_output_interruptible(child)?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let fallback_session_id = format!("local-{}", now_unix_millis());

    return Ok(ProviderRunResult {
        session_id: Some(fallback_session_id),
        exit_code: output.status.code(),
        stdout,
        stderr,
        usage: ProviderUsage::default(),
        turn_count: 0,
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

fn should_retry_unsandboxed_after_failure_with_agent_result(
    request: &ProviderRequest,
    run_started_at: SystemTime,
) -> bool {
    agent_result_reports_blocked_sandbox(
        &request.workspace_root,
        request.agent_result_rel.as_str(),
        run_started_at,
    )
}

fn should_retry_unsandboxed_after_success(
    request: &ProviderRequest,
    run_started_at: SystemTime,
) -> bool {
    let rel = request.agent_result_rel.trim();
    if !rel.is_empty() {
        let result_path = request.workspace_root.join(rel);
        if let Some(parsed) = read_recent_agent_result_value(result_path.as_path(), run_started_at)
        {
            if agent_result_reports_sandbox_block(&parsed) {
                return true;
            }
            if !agent_result_status_allows_retry(&parsed) {
                return false;
            }
        }
    }
    false
}

fn agent_result_reports_blocked_sandbox(
    workspace_root: &Path,
    result_rel: &str,
    run_started_at: SystemTime,
) -> bool {
    if result_rel.trim().is_empty() {
        return false;
    }
    agent_result_path_reports_blocked_sandbox(workspace_root.join(result_rel), run_started_at)
}

fn agent_result_path_reports_blocked_sandbox(path: PathBuf, run_started_at: SystemTime) -> bool {
    let parsed = match read_recent_agent_result_value(path.as_path(), run_started_at) {
        Some(v) => v,
        None => return false,
    };
    agent_result_reports_sandbox_block(&parsed)
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
    if agent_result_reports_sandbox_block(&parsed) {
        return Some(EARLY_AUTO_RETRY_REASON_SANDBOX_BLOCKED.to_string());
    }
    None
}

fn clear_agent_result_protocol_file(workspace_root: &Path, result_rel: &str) -> Result<()> {
    let rel = result_rel.trim();
    if rel.is_empty() {
        return Ok(());
    }
    let path = workspace_root.join(rel);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed clearing previous agent result protocol file '{}'",
                path.display()
            )
        }),
    }
}

fn read_recent_agent_result_value(
    path: &Path,
    run_started_at: SystemTime,
) -> Option<AgentResultProtocolFile> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let run_started_sec = run_started_at
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())?;
    let modified_sec = modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())?;
    if modified_sec < run_started_sec {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn agent_result_reports_sandbox_block(parsed: &AgentResultProtocolFile) -> bool {
    if !agent_result_status_allows_retry(parsed) {
        return false;
    }
    matches!(parsed.reason, Some(AgentResultReason::SandboxBlocked))
}

fn agent_result_status_allows_retry(parsed: &AgentResultProtocolFile) -> bool {
    match parsed.status {
        AgentResultStatus::Success => false,
        AgentResultStatus::Partial | AgentResultStatus::Failure => parsed.reason.is_some(),
    }
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
    show_housekeeping: bool,
    verbose_output: bool,
    verbose_events: bool,
    interactive_ui: bool,
    show_intermediate_steps: bool,
    suppress_turn_usage_lines: bool,
    execution_mode: CodexExecutionMode,
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

                // Liveness/progress is driven only by Codex JSON stream on stdout.
                // Stderr can contain banners or transport noise, but we still surface useful startup hints.
                if is_stdout {
                    if let Some(normalized) = normalized {
                        let command_payload = extract_command_output_payload(&line);
                        let message_payload = extract_message_output_payload(&line);

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
                                    show_housekeeping,
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
                                    show_housekeeping,
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
                        if let Some(payload) = command_payload.as_ref() {
                            if let Ok(Some(path)) =
                                sink.persist(program_id, session_id.as_deref(), payload)
                            {
                                command_output_links.insert(payload.item_id.clone(), path);
                            }
                        }
                        if let Some(payload) = message_payload.as_ref() {
                            if let Ok(Some(path)) =
                                sink.persist_message(program_id, session_id.as_deref(), payload)
                            {
                                message_output_links.insert(payload.item_id.clone(), path);
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
                            show_housekeeping,
                            show_intermediate_steps,
                        );
                        if show_intermediate_steps
                            && !suppress_turn_usage_lines
                            && progress_line.is_none()
                        {
                            if let ProviderEvent::TurnCompleted { ref usage } = normalized {
                                progress_line = format_turn_usage_line(turn_index, usage);
                            }
                        }
                        if let Some(progress_line) = progress_line {
                            let progress_line =
                                if matches!(normalized, ProviderEvent::RunStarted { .. }) {
                                    format!("{} | {}", progress_line, execution_mode.label())
                                } else {
                                    progress_line
                                };
                            let progress_line =
                                add_completion_duration_suffix(&progress_line, completion_duration);
                            let progress_line = if verbose_output {
                                expand_verbose_progress_line(
                                    &normalized,
                                    &progress_line,
                                    command_payload.as_ref(),
                                    message_payload.as_ref(),
                                )
                            } else {
                                let progress_line = add_command_output_link_suffix(
                                    &normalized,
                                    &progress_line,
                                    &command_output_links,
                                    supports_hyperlinks,
                                );
                                add_message_output_link_suffix(
                                    &normalized,
                                    &progress_line,
                                    &message_output_links,
                                    supports_hyperlinks,
                                )
                            };
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
            "blocked by sandbox restrictions (auto sandbox retry requested from agent_result reason: ",
        );
        raw_stderr.push_str(reason);
        raw_stderr.push_str(")\n");
    }

    let final_session_id = match session_id.as_deref() {
        Some(id) if !id.trim().is_empty() => sanitize_session_id(id),
        _ => format!("local-{}", now_unix_millis()),
    };

    Ok(ProviderRunResult {
        session_id: Some(final_session_id),
        exit_code: status.and_then(|s| s.code()),
        stdout: raw_stdout,
        stderr: raw_stderr,
        usage: usage_totals,
        turn_count: turn_index,
    })
}

fn expand_verbose_progress_line(
    event: &ProviderEvent,
    line: &str,
    command_payload: Option<&CommandOutputPayload>,
    message_payload: Option<&MessageOutputPayload>,
) -> String {
    if let Some(payload) = command_payload {
        if matches!(
            event,
            ProviderEvent::ItemCompleted { item_type, .. } if item_type == "command_execution"
        ) {
            let output = payload.output.trim_end();
            if output.is_empty() {
                return line.to_string();
            }
            return format!("{}\n{}", line, output);
        }
    }

    if let Some(payload) = message_payload {
        if matches!(
            event,
            ProviderEvent::ItemCompleted { item_type, .. }
                if is_reasoning_item_type(item_type) || is_agent_text_item_type(item_type)
        ) {
            let text = payload.text.trim_end();
            if text.is_empty() {
                return line.to_string();
            }
            let prefix = if is_reasoning_item_type(payload.item_type.as_str()) {
                "💭"
            } else {
                "💬"
            };
            return format!("{} {}", prefix, text);
        }
    }

    line.to_string()
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
    if env::var("CLAWFORM_NO_HYPERLINKS")
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
    show_housekeeping: bool,
    show_intermediate_steps: bool,
) -> Option<String> {
    match event {
        ProviderEvent::RunStarted { run_id } => {
            if !show_intermediate_steps {
                return None;
            }
            Some(match run_id.as_deref() {
                Some(id) => format!("🧵 {}", id),
                None => "🧵 started".to_string(),
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
                show_housekeeping,
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
    show_housekeeping: bool,
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
    if !show_housekeeping
        && (kind == "cmd" || kind == "file")
        && is_clawform_housekeeping_summary(summary)
    {
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

fn is_clawform_housekeeping_summary(summary: &str) -> bool {
    let s = summary.trim().to_ascii_lowercase();
    s.starts_with("write .clawform/agent_output.md")
        || s.starts_with("write .clawform/agent_outputs.json")
        || s.starts_with("write .clawform/agent_result.json")
        || s.starts_with("read .clawform/agent_output.md")
        || s.starts_with("read .clawform/agent_outputs.json")
        || s.starts_with("read .clawform/agent_result.json")
        || s.starts_with("read .clawform/agent_variables.json")
        || s.starts_with("cat .clawform/agent_output.md")
        || s.starts_with("cat .clawform/agent_outputs.json")
        || s.starts_with("cat .clawform/agent_result.json")
        || {
            let is_protocol_mkdir = s.starts_with("mkdir -p ")
                && (s.ends_with(" .clawform")
                    || s.ends_with("/.clawform")
                    || s.contains(" /.clawform "));
            is_protocol_mkdir
        }
        || {
            let is_protocol_io =
                s.starts_with("write ") || s.starts_with("read ") || s.starts_with("cat ");
            is_protocol_io
                && s.contains(".clawform/programs/")
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
    show_housekeeping: bool,
    show_intermediate_steps: bool,
) -> bool {
    if let Some(s) = summary {
        if !show_housekeeping && is_clawform_housekeeping_summary(s) {
            return false;
        }
    }
    if item_type != "command_execution" {
        return true;
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
            return format_turn_usage_event_line(line, true);
        }
        if let Some(rest) = line.strip_prefix("🧵 session ") {
            return format!("\x1b[36m🧵\x1b[0m {}", colorize_session_payload(rest));
        }
        if let Some(rest) = line.strip_prefix("🧵 ") {
            return format!("\x1b[36m🧵\x1b[0m {}", colorize_session_payload(rest));
        }
        if let Some(rest) = line.strip_prefix("session ") {
            return format!("\x1b[2msession\x1b[0m {}", colorize_session_payload(rest));
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
        return out.join(" \x1b[2m|\x1b[0m ");
    }

    if let Some((head, tail)) = trimmed.rsplit_once(' ') {
        if looks_like_duration_label(tail) {
            return format!("{} \x1b[2m{}\x1b[0m", colorize_command_summary(head), tail);
        }
    }
    colorize_command_summary(trimmed)
}

fn format_turn_usage_event_line(line: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[2m{}\x1b[0m", line)
    } else {
        line.to_string()
    }
}

fn colorize_session_payload(payload: &str) -> String {
    let segments = payload.split(" | ").collect::<Vec<_>>();
    if segments.len() <= 1 {
        return colorize_paths(payload);
    }

    let mut out = String::new();
    out.push_str(&colorize_paths(segments[0]));
    for segment in segments.iter().skip(1) {
        out.push_str(" \x1b[2m|\x1b[0m ");
        out.push_str(&colorize_session_segment(segment));
    }
    out
}

fn colorize_session_segment(segment: &str) -> String {
    let trimmed = segment.trim();
    match trimmed {
        "workspace-write" => return "\x1b[34mworkspace-write\x1b[0m".to_string(),
        "danger-full-access" => return "\x1b[33mdanger-full-access\x1b[0m".to_string(),
        _ => {}
    }
    if let Some((key, value)) = trimmed.split_once('=') {
        let code = match (key, value) {
            ("sandbox", "workspace-write") => Some("34"),
            ("sandbox", "danger-full-access") => Some("33"),
            _ => None,
        };
        if let Some(code) = code {
            return format!("\x1b[2m{}=\x1b[0m\x1b[{}m{}\x1b[0m", key, code, value);
        }
    }

    colorize_paths(segment)
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

    if let Some((start, label, end)) = split_terminal_hyperlink(segment) {
        return Some(format!("{}\x1b[95m{}\x1b[0m{}", start, label, end));
    }

    if segment.contains("\x1b]8;;") {
        return Some(format!("\x1b[95m{}\x1b[0m", segment));
    }

    None
}

fn split_terminal_hyperlink(segment: &str) -> Option<(&str, &str, &str)> {
    if !segment.starts_with("\x1b]8;;") {
        return None;
    }

    let open_end = segment.find("\x1b\\")? + "\x1b\\".len();
    let close_seq = "\x1b]8;;\x1b\\";
    let close_start = segment[open_end..].find(close_seq)? + open_end;
    Some((
        &segment[..open_end],
        &segment[open_end..close_start],
        &segment[close_start..],
    ))
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
        || core.starts_with(".clawform/")
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
    fn retry_decision_line_uses_agent_result_fields() {
        let parsed = AgentResultProtocolFile {
            status: AgentResultStatus::Failure,
            reason: Some(AgentResultReason::ProgramBlocked),
        };
        let run = ProviderRunResult {
            session_id: Some("s1".to_string()),
            exit_code: Some(1),
            stdout: String::new(),
            stderr: String::new(),
            usage: ProviderUsage {
                input_tokens: Some(10),
                output_tokens: Some(3),
                cached_input_tokens: Some(2),
            },
            turn_count: 1,
        };
        let line = format_auto_sandbox_retry_decision_line(
            &run,
            Some(&parsed),
            true,
            Some(Path::new("/tmp/clawform/.clawform/agent_result.json")),
            false,
            false,
        )
        .expect("expected retry decision line");
        assert!(line.contains("turn 1 | tokens: in=10 out=3 cached=2"));
        assert!(line.contains("program_blocked"));
        assert!(!line.contains("failure"));
        assert!(line.contains("file"));
        assert!(!line.contains("source=agent_result"));
        assert!(!line.contains("result="));
    }

    #[test]
    fn retry_decision_line_absent_without_agent_result_signal() {
        let run = ProviderRunResult {
            session_id: Some("s1".to_string()),
            exit_code: Some(1),
            stdout: String::new(),
            stderr: String::new(),
            usage: ProviderUsage::default(),
            turn_count: 1,
        };
        let line = format_auto_sandbox_retry_decision_line(
            &run,
            None,
            false,
            Some(Path::new("/tmp/clawform/.clawform/agent_result.json")),
            false,
            false,
        );
        assert!(line.is_none());
    }

    #[test]
    fn retry_decision_line_absent_when_no_sandbox_signal() {
        let run = ProviderRunResult {
            session_id: Some("s1".to_string()),
            exit_code: Some(1),
            stdout: String::new(),
            stderr: String::new(),
            usage: ProviderUsage::default(),
            turn_count: 1,
        };
        let line = format_auto_sandbox_retry_decision_line(&run, None, false, None, false, false);
        assert!(line.is_none());
    }

    #[test]
    fn retry_decision_line_absent_when_agent_result_is_non_blocking() {
        let parsed = AgentResultProtocolFile {
            status: AgentResultStatus::Failure,
            reason: Some(AgentResultReason::ProgramBlocked),
        };
        let run = ProviderRunResult {
            session_id: Some("s1".to_string()),
            exit_code: Some(1),
            stdout: String::new(),
            stderr: String::new(),
            usage: ProviderUsage::default(),
            turn_count: 1,
        };
        let line = format_auto_sandbox_retry_decision_line(
            &run,
            Some(&parsed),
            false,
            Some(Path::new("/tmp/clawform/.clawform/agent_result.json")),
            false,
            false,
        );
        assert!(line.is_none());
    }

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
            false,
            true,
        )
        .expect("expected session line");
        assert_eq!(line, "🧵 thread_123");
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
    fn claude_capabilities_exposed() {
        let caps = ClaudeRunner.capabilities();
        assert!(caps.live_events);
        assert!(caps.tool_call_events);
        assert!(!caps.file_change_events);
        assert!(caps.resume);
    }

    #[test]
    fn provider_runner_factory_returns_codex_runner() {
        let runner = resolve_provider_runner(ProviderKind::Codex).expect("codex runner");
        let caps = runner.capabilities();
        assert!(caps.live_events);
        assert!(caps.resume);
    }

    #[test]
    fn provider_runner_factory_returns_claude_runner() {
        let runner = resolve_provider_runner(ProviderKind::Claude).expect("claude runner");
        let caps = runner.capabilities();
        assert!(caps.live_events);
        assert!(caps.resume);
    }

    #[test]
    fn claude_pending_tool_summarizes_bash_write_target() {
        let content: Value = serde_json::from_str(
            r#"{"id":"toolu_1","name":"Bash","input":{"command":"echo SMOKE_OK > example-data/output-smoke.txt"}}"#,
        )
        .expect("json");

        let tool = claude_pending_tool(&content, Path::new("/tmp/work")).expect("tool");
        assert_eq!(tool.item_type, "command_execution");
        assert_eq!(
            tool.summary.as_deref(),
            Some("write example-data/output-smoke.txt")
        );
        assert!(tool.emits_command_output);
    }

    #[test]
    fn claude_pending_tool_relativizes_workspace_write_path() {
        let content: Value = serde_json::from_str(
            r#"{"id":"toolu_2","name":"Write","input":{"file_path":"/tmp/work/example-data/output-smoke.txt"}}"#,
        )
        .expect("json");

        let tool = claude_pending_tool(&content, Path::new("/tmp/work")).expect("tool");
        assert_eq!(tool.item_type, "file_change");
        assert_eq!(
            tool.summary.as_deref(),
            Some("write example-data/output-smoke.txt")
        );
        assert_eq!(
            tool.paths,
            vec!["/tmp/work/example-data/output-smoke.txt".to_string()]
        );
    }

    #[test]
    fn claude_tool_result_text_ignores_rerun_marker_without_output() {
        let content: Value =
            serde_json::from_str(r#"{"type":"tool_result","content":"[rerun: b1]"}"#)
                .expect("json");
        let tool_use_result: Value = serde_json::from_str(
            r#"{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":true}"#,
        )
        .expect("json");

        assert!(claude_tool_result_text(&content, Some(&tool_use_result)).is_none());
    }

    #[test]
    fn parses_claude_json_success_result() {
        let result = parse_claude_json_result(
            r#"{"type":"result","is_error":false,"result":"ok","session_id":"claude-session","num_turns":6,"usage":{"input_tokens":5,"output_tokens":7,"cache_read_input_tokens":11}}"#
                .to_string(),
            String::new(),
            Some(0),
        )
        .expect("claude json result");

        assert_eq!(result.session_id.as_deref(), Some("claude-session"));
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.turn_count, 6);
        assert_eq!(result.usage.input_tokens, Some(5));
        assert_eq!(result.usage.output_tokens, Some(7));
        assert_eq!(result.usage.cached_input_tokens, Some(11));
    }

    #[test]
    fn promotes_claude_json_error_even_when_process_exit_is_zero() {
        let result = parse_claude_json_result(
            r#"{"type":"result","is_error":true,"result":"auth missing","session_id":"claude-session","num_turns":1,"usage":{}}"#
                .to_string(),
            String::new(),
            Some(0),
        )
        .expect("claude json result");

        assert_eq!(result.exit_code, Some(1));
        assert_eq!(result.stderr, "auth missing");
    }

    #[test]
    fn claude_sandbox_settings_enable_guardrails() {
        let raw = claude_settings_json(ClaudeExecutionMode::Sandboxed).expect("settings");
        let parsed: Value = serde_json::from_str(&raw).expect("json");

        assert_eq!(parsed["sandbox"]["enabled"], Value::Bool(true));
        assert_eq!(
            parsed["sandbox"]["autoAllowBashIfSandboxed"],
            Value::Bool(true)
        );
        assert_eq!(
            parsed["sandbox"]["allowUnsandboxedCommands"],
            Value::Bool(false)
        );
        assert_eq!(parsed["sandbox"]["failIfUnavailable"], Value::Bool(true));
        assert!(claude_settings_json(ClaudeExecutionMode::Unsandboxed).is_none());
    }

    #[test]
    fn sandbox_mode_labels_match_cli_values() {
        assert_eq!(SandboxMode::default(), SandboxMode::Auto);
        assert_eq!(SandboxMode::Auto.label(), "auto");
        assert_eq!(SandboxMode::Sandboxed.label(), "workspace-write");
        assert_eq!(SandboxMode::Unsandboxed.label(), "danger-full-access");
    }

    #[test]
    fn session_lines_include_execution_mode_values() {
        let sandboxed = colorize_session_payload("019d-session | workspace-write");
        assert!(sandboxed.contains("workspace-write"));

        let unsandboxed = colorize_session_payload("019d-session | danger-full-access");
        assert!(unsandboxed.contains("danger-full-access"));
    }

    #[test]
    fn session_payload_uses_readable_sandbox_value() {
        let rendered = colorize_session_payload("019d-session | workspace-write");
        assert!(rendered.contains("019d-session"));
        assert!(rendered.contains("\x1b[34mworkspace-write\x1b[0m"));
    }

    #[test]
    fn session_event_line_uses_readable_sandbox_value() {
        let printer = ProgressPrinter {
            interactive: true,
            status_line: None,
            spinner_idx: 0,
            cursor_hidden: false,
        };
        let rendered = printer.render_event_line("🧵 019d-session | danger-full-access");
        assert!(rendered.contains("🧵"));
        assert!(!rendered.contains("🧵 session "));
        assert!(rendered.contains("\x1b[33mdanger-full-access\x1b[0m"));
    }

    #[test]
    fn non_json_line_maps_to_raw_text_without_progress_line() {
        let ev = parse_codex_stream_line("OpenAI Codex v0.118.0").expect("expected raw text");
        assert!(matches!(ev, ProviderEvent::RawText { .. }));
        assert!(format_terminal_event(&ev, true, false, false).is_none());
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
            turn_count: 0,
        };
        assert!(is_transient_codex_failure(&run));
    }

    #[test]
    fn classifies_transient_failure_from_error_message() {
        let err = anyhow!("stream disconnected before completion");
        assert!(is_transient_codex_error(&err));
    }

    #[test]
    fn does_not_retry_without_agent_reason() {
        let dir = tempfile::tempdir().expect("temp dir");
        let request = ProviderRequest {
            workspace_root: dir.path().to_path_buf(),
            artifacts_root: None,
            program_id: Some("release-notes".to_string()),
            model: None,
            agent_result_rel: ".clawform/programs/release-notes/reports/agent_result.json"
                .to_string(),
            sandbox_mode: SandboxMode::Auto,
            prompt: "x".to_string(),
            progress: true,
            render_progress: false,
            debug_mode: false,
            verbose_output: false,
            verbose_events: false,
            interactive_ui: false,
            show_intermediate_steps: false,
        };
        assert!(!should_retry_unsandboxed_after_failure_with_agent_result(
            &request,
            std::time::SystemTime::now(),
        ));
    }

    #[test]
    fn does_not_retry_unsandboxed_from_output_heuristics_permission_or_connectivity() {
        let dir = tempfile::tempdir().expect("temp dir");
        let request = ProviderRequest {
            workspace_root: dir.path().to_path_buf(),
            artifacts_root: None,
            program_id: Some("hello-dstack".to_string()),
            model: None,
            agent_result_rel: ".clawform/programs/hello-dstack/reports/agent_result.json"
                .to_string(),
            sandbox_mode: SandboxMode::Auto,
            prompt: "x".to_string(),
            progress: true,
            render_progress: false,
            debug_mode: false,
            verbose_output: false,
            verbose_events: false,
            interactive_ui: false,
            show_intermediate_steps: false,
        };
        assert!(!should_retry_unsandboxed_after_failure_with_agent_result(
            &request,
            std::time::SystemTime::now(),
        ));
    }

    #[test]
    fn does_not_retry_unsandboxed_from_output_heuristics_dns_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let request = ProviderRequest {
            workspace_root: dir.path().to_path_buf(),
            artifacts_root: None,
            program_id: Some("release-notes".to_string()),
            model: None,
            agent_result_rel: ".clawform/programs/release-notes/reports/agent_result.json"
                .to_string(),
            sandbox_mode: SandboxMode::Auto,
            prompt: "x".to_string(),
            progress: true,
            render_progress: false,
            debug_mode: false,
            verbose_output: false,
            verbose_events: false,
            interactive_ui: false,
            show_intermediate_steps: false,
        };
        assert!(!should_retry_unsandboxed_after_failure_with_agent_result(
            &request,
            std::time::SystemTime::now(),
        ));
    }

    #[test]
    fn does_not_treat_message_only_agent_result_as_sandbox_blocked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
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

        assert!(!agent_result_reports_blocked_sandbox(
            dir.path(),
            ".clawform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn does_not_treat_dns_message_without_reason_as_sandbox_blocked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
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

        assert!(!agent_result_reports_blocked_sandbox(
            dir.path(),
            ".clawform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn does_not_treat_legacy_reason_value_as_agent_result_signal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"service_unreachable","message":"legacy reason"}"#,
        )
        .expect("write agent_result");

        assert!(!agent_result_reports_blocked_sandbox(
            dir.path(),
            ".clawform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn reads_recent_agent_result_for_reason_keyword_detection() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"sandbox_blocked","message":"blocked"}"#,
        )
        .expect("write agent_result");

        assert!(agent_result_reports_blocked_sandbox(
            dir.path(),
            ".clawform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn reads_recent_agent_result_for_generic_sandbox_reason_detection() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"sandbox_blocked","message":"sandbox prevented required operation"}"#,
        )
        .expect("write agent_result");

        assert!(agent_result_reports_blocked_sandbox(
            dir.path(),
            ".clawform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn does_not_treat_program_blocked_reason_as_sandbox_blocked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"program_blocked","message":"permission denied on ~/.dstack/logs and server unreachable"}"#,
        )
        .expect("write agent_result");

        assert!(!agent_result_reports_blocked_sandbox(
            dir.path(),
            ".clawform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn does_not_treat_program_blocked_reason_with_sandboxy_message_as_sandbox_blocked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"program_blocked","message":"permission denied and failed to connect"}"#,
        )
        .expect("write agent_result");

        assert!(!agent_result_reports_blocked_sandbox(
            dir.path(),
            ".clawform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn does_not_treat_unknown_sandbox_reason_as_blocked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"sandbox_service_blocked","message":"sandbox prevented service bootstrap"}"#,
        )
        .expect("write agent_result");

        assert!(!agent_result_reports_blocked_sandbox(
            dir.path(),
            ".clawform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn ignores_reason_keyword_when_status_is_success() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"success","reason":"sandbox_blocked","message":"done"}"#,
        )
        .expect("write agent_result");

        assert!(!agent_result_reports_blocked_sandbox(
            dir.path(),
            ".clawform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn ignores_sandbox_blocked_flag_when_status_is_success() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"success","reason":"program_blocked","message":"done"}"#,
        )
        .expect("write agent_result");

        assert!(!agent_result_reports_blocked_sandbox(
            dir.path(),
            ".clawform/programs/release-notes/reports/agent_result.json",
            started
        ));
    }

    #[test]
    fn detects_early_auto_retry_reason_from_agent_result() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"sandbox_blocked","message":"curl failed"}"#,
        )
        .expect("write agent_result");

        let monitor = EarlyAutoRetryMonitor {
            agent_result_path: path,
            run_started_at: started,
        };
        assert_eq!(
            detect_early_auto_retry_reason(Some(&monitor)).as_deref(),
            Some("sandbox_blocked")
        );
    }

    #[test]
    fn does_not_detect_early_auto_retry_reason_for_program_blocked_reason() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"program_blocked","message":"permission denied and failed to connect"}"#,
        )
        .expect("write agent_result");

        let monitor = EarlyAutoRetryMonitor {
            agent_result_path: path,
            run_started_at: started,
        };
        assert!(detect_early_auto_retry_reason(Some(&monitor)).is_none());
    }

    #[test]
    fn does_not_detect_early_auto_retry_reason_for_program_blocked_reason_with_sandboxy_message() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"program_blocked","message":"permission denied and failed to connect"}"#,
        )
        .expect("write agent_result");

        let monitor = EarlyAutoRetryMonitor {
            agent_result_path: path,
            run_started_at: started,
        };
        assert!(detect_early_auto_retry_reason(Some(&monitor)).is_none());
    }

    #[test]
    fn does_not_trigger_early_auto_retry_for_unknown_sandbox_reason() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"sandbox_service_blocked","message":"sandbox blocked required service bootstrap"}"#,
        )
        .expect("write agent_result");

        let monitor = EarlyAutoRetryMonitor {
            agent_result_path: path,
            run_started_at: started,
        };
        assert!(detect_early_auto_retry_reason(Some(&monitor)).is_none());
    }

    #[test]
    fn early_auto_retry_ignores_stale_agent_result() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir
            .path()
            .join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        std::fs::write(
            &path,
            r#"{"status":"failure","reason":"sandbox_blocked","message":"curl failed"}"#,
        )
        .expect("write agent_result");
        let started = std::time::SystemTime::now()
            .checked_add(std::time::Duration::from_secs(2))
            .expect("future ts");
        let monitor = EarlyAutoRetryMonitor {
            agent_result_path: path,
            run_started_at: started,
        };
        assert!(detect_early_auto_retry_reason(Some(&monitor)).is_none());
    }

    #[test]
    fn retries_unsandboxed_when_failed_run_has_sandbox_blocked_reason() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace_root = dir.path().to_path_buf();
        let result_path =
            workspace_root.join(".clawform/programs/release-notes/reports/agent_result.json");
        if let Some(parent) = result_path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }
        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &result_path,
            r#"{"status":"failure","reason":"sandbox_blocked","message":"Blocked by network restrictions; required release note downloads unavailable"}"#,
        )
        .expect("write agent_result");

        let request = ProviderRequest {
            workspace_root,
            artifacts_root: None,
            program_id: Some("release-notes".to_string()),
            model: None,
            agent_result_rel: ".clawform/programs/release-notes/reports/agent_result.json"
                .to_string(),
            sandbox_mode: SandboxMode::Auto,
            prompt: "x".to_string(),
            progress: true,
            render_progress: false,
            debug_mode: false,
            verbose_output: false,
            verbose_events: false,
            interactive_ui: false,
            show_intermediate_steps: false,
        };
        assert!(should_retry_unsandboxed_after_failure_with_agent_result(
            &request, started
        ));
    }

    #[test]
    fn success_path_retries_when_agent_result_reports_sandbox_blocked_reason() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace_root = dir.path().to_path_buf();
        let result_rel = ".clawform/programs/release-notes/reports/agent_result.json";
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
            debug_mode: false,
            verbose_output: false,
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
            r#"{"status":"failure","reason":"sandbox_blocked","message":"curl failed: Could not resolve host: example.com"}"#,
        )
        .expect("write agent_result");

        assert!(should_retry_unsandboxed_after_success(
            &request,
            started_with_file
        ));
    }

    #[test]
    fn success_path_does_not_retry_when_reason_is_program_blocked_even_if_output_looks_sandboxy() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace_root = dir.path().to_path_buf();
        let result_rel = ".clawform/programs/hello-dstack/reports/agent_result.json";
        let result_path = workspace_root.join(result_rel);
        if let Some(parent) = result_path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }

        let request = ProviderRequest {
            workspace_root: workspace_root.clone(),
            artifacts_root: None,
            program_id: Some("hello-dstack".to_string()),
            model: None,
            agent_result_rel: result_rel.to_string(),
            sandbox_mode: SandboxMode::Auto,
            prompt: "x".to_string(),
            progress: true,
            render_progress: false,
            debug_mode: false,
            verbose_output: false,
            verbose_events: false,
            interactive_ui: false,
            show_intermediate_steps: false,
        };

        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            &result_path,
            r#"{"status":"failure","reason":"program_blocked","message":"failed to reach local dstack server"}"#,
        )
        .expect("write agent_result");

        assert!(!should_retry_unsandboxed_after_success(&request, started));
    }

    #[test]
    fn success_path_does_not_retry_when_agent_result_is_success() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace_root = dir.path().to_path_buf();
        let result_rel = ".clawform/programs/hello-dstack/reports/agent_result.json";
        let result_path = workspace_root.join(result_rel);
        if let Some(parent) = result_path.parent() {
            std::fs::create_dir_all(parent).expect("create reports dir");
        }

        let request = ProviderRequest {
            workspace_root: workspace_root.clone(),
            artifacts_root: None,
            program_id: Some("hello-dstack".to_string()),
            model: None,
            agent_result_rel: result_rel.to_string(),
            sandbox_mode: SandboxMode::Auto,
            prompt: "x".to_string(),
            progress: true,
            render_progress: false,
            debug_mode: false,
            verbose_output: false,
            verbose_events: false,
            interactive_ui: false,
            show_intermediate_steps: false,
        };

        let started = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&result_path, r#"{"status":"success","message":"all good"}"#)
            .expect("write agent_result");

        assert!(!should_retry_unsandboxed_after_success(&request, started));
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
            false,
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
            false,
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
                summary: Some("ls .clawform".to_string()),
            },
            true,
            false,
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
            false,
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
                    "write .clawform/programs/release-notes/reports/agent_result.json".to_string(),
                ),
            },
            true,
            false,
            true,
        );
        assert!(line.is_none());
    }

    #[test]
    fn hides_housekeeping_file_changes_even_when_intermediate_enabled() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "file_change".to_string(),
                item_id: Some("x".to_string()),
                summary: Some("write .clawform/agent_outputs.json".to_string()),
            },
            true,
            false,
            true,
        );
        assert!(line.is_none());
    }

    #[test]
    fn hides_housekeeping_reads_even_when_intermediate_enabled() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "command_execution".to_string(),
                item_id: Some("x".to_string()),
                summary: Some("read .clawform/agent_variables.json".to_string()),
            },
            true,
            false,
            true,
        );
        assert!(line.is_none());
    }

    #[test]
    fn hides_housekeeping_mkdir_even_when_intermediate_enabled() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "command_execution".to_string(),
                item_id: Some("x".to_string()),
                summary: Some("mkdir -p /Users/dstack/clawform/.clawform".to_string()),
            },
            true,
            false,
            true,
        );
        assert!(line.is_none());
    }

    #[test]
    fn shows_housekeeping_commands_when_internal_visibility_enabled() {
        let line = format_terminal_event(
            &ProviderEvent::ItemCompleted {
                item_type: "command_execution".to_string(),
                item_id: Some("x".to_string()),
                summary: Some("read .clawform/agent_variables.json".to_string()),
            },
            true,
            true,
            true,
        )
        .expect("expected line");
        assert!(line.contains("read .clawform/agent_variables.json"));
    }

    #[test]
    fn housekeeping_commands_do_not_count_as_progress_activity() {
        assert!(!should_count_item_progress(
            "command_execution",
            Some("write .clawform/programs/release-notes/reports/agent_outputs.json"),
            false,
            true
        ));
        assert!(!should_count_item_progress(
            "command_execution",
            Some("cat .clawform/programs/release-notes/reports/agent_result.json"),
            false,
            false
        ));
        assert!(!should_count_item_progress(
            "file_change",
            Some("write .clawform/agent_result.json"),
            false,
            true
        ));
        assert!(!should_count_item_progress(
            "command_execution",
            Some("read .clawform/agent_variables.json"),
            false,
            true
        ));
        assert!(!should_count_item_progress(
            "command_execution",
            Some("mkdir -p /Users/dstack/clawform/.clawform"),
            false,
            true
        ));
        assert!(should_count_item_progress(
            "command_execution",
            Some("read .clawform/agent_variables.json"),
            true,
            true
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
            PathBuf::from("/tmp/clawform/programs/smoke/sessions/session/commands/item_9.txt"),
        );
        let rendered = add_command_output_link_suffix(&event, "✔ ls", &links, false);
        assert!(rendered.contains(
            "✔ ls | out=/tmp/clawform/programs/smoke/sessions/session/commands/item_9.txt"
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
            PathBuf::from("/tmp/clawform/programs/smoke/sessions/session/messages/item_4.md"),
        );
        let rendered = add_message_output_link_suffix(&event, "💬 Some summary", &links, false);
        assert!(rendered.contains(
            "💬 Some summary | msg=/tmp/clawform/programs/smoke/sessions/session/messages/item_4.md"
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
            PathBuf::from("/tmp/clawform/src/main.rs"),
        );
        let rendered =
            add_file_change_link_suffix(&event, "✔ file update src/main.rs", &links, false);
        assert!(rendered.contains("✔ file update src/main.rs | file=/tmp/clawform/src/main.rs"));
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

    #[test]
    fn colorize_link_segment_highlights_hyperlink_label_text() {
        let segment = "\x1b]8;;file:///tmp/clawform/commands/item_9.txt\x1b\\out\x1b]8;;\x1b\\";
        let rendered = colorize_link_segment(segment).expect("expected colored hyperlink segment");
        assert_eq!(
            rendered,
            "\x1b]8;;file:///tmp/clawform/commands/item_9.txt\x1b\\\x1b[95mout\x1b[0m\x1b]8;;\x1b\\"
        );
    }
}
