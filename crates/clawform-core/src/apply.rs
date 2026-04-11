use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

use crate::config::load_config;
use crate::fingerprint::hash_file_or_missing;
use crate::history::{
    append_history_record, load_program_history_context, now_unix_secs, ProgramHistoryContext,
    RunHistoryRecord, RunStatus,
};
use crate::path_utils::to_slash_path;
use crate::program::load_program;
use crate::provider::clear_interrupt_request;
use crate::provider::ensure_interrupt_handler;
use crate::provider::interrupt_requested;
use crate::provider::{ProviderRequest, ProviderRunResult, ProviderRunner, SandboxMode};

const AGENT_OUTPUT_MANIFEST_REL: &str = ".clawform/agent_outputs.json";
const AGENT_HUMAN_OUTPUT_REL: &str = ".clawform/agent_output.md";
const AGENT_RESULT_REL: &str = ".clawform/agent_result.json";
const RUNTIME_VARIABLES_INPUT_REL: &str = ".clawform/agent_variables.json";
const MAX_HISTORY_TEXT_CHARS: usize = 180;
const MAX_HISTORY_FILE_SAMPLE: usize = 3;
const SNAPSHOT_TEXT_LIMIT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct ApplyRequest {
    pub workspace_root: PathBuf,
    pub program_path: PathBuf,
    pub provider_name: Option<String>,
    pub program_variables: BTreeMap<String, String>,
    pub confirm: bool,
    pub debug: bool,
    pub verbose_output: bool,
    pub progress: bool,
    pub render_progress: bool,
    pub interactive_ui: bool,
    pub show_intermediate_steps: bool,
    pub use_history_context: bool,
    pub sandbox_mode: SandboxMode,
}

#[derive(Debug, Clone)]
pub struct ApplyResult {
    pub provider_result: Option<ProviderRunResult>,
    pub promoted_files: Vec<String>,
    pub agent_reported_files: Vec<String>,
    pub agent_human_summary: Option<String>,
    pub agent_human_summary_artifact: Option<String>,
    pub agent_result: Option<AgentResult>,
    pub file_results: Vec<FileResult>,
    pub prompt_artifact: Option<String>,
    pub plan_artifact: Option<String>,
    pub provider_stdout_artifact: Option<String>,
    pub provider_stderr_artifact: Option<String>,
    pub events_artifact: Option<String>,
    pub history_injected_success: bool,
    pub history_injected_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Success,
    Partial,
    Failure,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentReason {
    SandboxBlocked,
    ProgramBlocked,
}

impl AgentReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SandboxBlocked => "sandbox_blocked",
            Self::ProgramBlocked => "program_blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResult {
    pub status: AgentStatus,
    pub reason: Option<AgentReason>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentResultFile {
    status: AgentStatus,
    #[serde(default)]
    reason: Option<AgentReason>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum AgentOutputManifestEntry {
    Path(String),
    Record(AgentOutputManifestRecord),
}

#[derive(Debug, Clone, Deserialize)]
struct AgentOutputManifestRecord {
    path: String,
    #[serde(default)]
    change: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FileResult {
    pub path: String,
    pub changed: bool,
    pub reported: bool,
    pub lines_changed: usize,
    pub lines_added: usize,
    pub lines_deleted: usize,
}

#[derive(Debug, Clone)]
struct ApplyContext {
    program_key: String,
    program_file: String,
    resolved_model: Option<String>,
    program_raw: String,
    program_variables: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
struct LineStats {
    changed: usize,
    added: usize,
    deleted: usize,
}

#[derive(Debug, Clone)]
struct WorkspaceFileSnapshot {
    hash: String,
    text: Option<String>,
}

#[derive(Debug, Clone)]
struct DerivedSummary {
    text: String,
}

#[derive(Debug, Clone, Serialize)]
struct SharedPlanData {
    program_id: String,
    program_file: String,
    model: Option<String>,
    program_variables: Option<PlanProgramVariables>,
    program_variables_diff_vs_last_session: Option<PlanProgramVariablesDiff>,
    program_diff_vs_last_session: PlanProgramDiff,
    last_session: Option<PlanLastSession>,
}

#[derive(Debug, Clone, Serialize)]
struct PlanProgramVariables {
    values_total: usize,
    current_session_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_session_file: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PlanProgramVariablesDiff {
    status: String,
    values_changed: usize,
    values_added: usize,
    values_removed: usize,
}

#[derive(Debug, Clone, Serialize)]
struct PlanProgramDiff {
    status: String,
    file: String,
    lines_changed: usize,
    lines_added: usize,
    lines_deleted: usize,
}

#[derive(Debug, Clone, Serialize)]
struct PlanLastSession {
    session_id: Option<String>,
    ts_unix: u64,
    status: RunStatus,
    model: Option<String>,
    summary_short: Option<String>,
    files_total: usize,
    files_sample: Vec<String>,
    insertions: usize,
    deletions: usize,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    error_short: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionOutcome {
    program_id: String,
    session_id: String,
    status: String,
    agent_status: Option<String>,
    agent_message: Option<String>,
    model: Option<String>,
    error: Option<String>,
    files_total: usize,
    insertions: usize,
    deletions: usize,
    files_changed: Vec<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
}

pub fn run_apply<R: ProviderRunner + ?Sized>(
    request: &ApplyRequest,
    runner: &R,
) -> Result<ApplyResult> {
    let context = build_context(request)?;
    let history_context = if request.use_history_context {
        load_program_history_context(&request.workspace_root, &context.program_key)?
    } else {
        ProgramHistoryContext::default()
    };
    let plan_data = build_shared_plan_data(&request.workspace_root, &context, &history_context);

    if request.debug || request.confirm {
        print_plan_preview(&plan_data, request.debug, &request.workspace_root);
    }

    if request.confirm && !confirm_interactive()? {
        return Ok(ApplyResult {
            provider_result: None,
            promoted_files: Vec::new(),
            agent_reported_files: Vec::new(),
            agent_human_summary: None,
            agent_human_summary_artifact: None,
            agent_result: None,
            file_results: Vec::new(),
            prompt_artifact: None,
            plan_artifact: None,
            provider_stdout_artifact: None,
            provider_stderr_artifact: None,
            events_artifact: None,
            history_injected_success: false,
            history_injected_failure: false,
        });
    }

    execute_apply(request, context, history_context, plan_data, runner)
}

fn build_context(request: &ApplyRequest) -> Result<ApplyContext> {
    let workspace_root = &request.workspace_root;

    let config = load_config(workspace_root)?;
    let provider = config.resolve_provider(request.provider_name.as_deref())?;

    let program = load_program(&request.program_path)?;
    let program_key = program.program_key()?;
    let resolved_model = program.resolved_model(provider.default_model.as_deref());
    let program_variables = program.resolve_variables(&request.program_variables)?;
    let program_file = display_program_file(&request.workspace_root, &request.program_path);

    Ok(ApplyContext {
        program_key,
        program_file,
        resolved_model,
        program_raw: program.raw_markdown,
        program_variables,
    })
}

fn execute_apply<R: ProviderRunner + ?Sized>(
    request: &ApplyRequest,
    context: ApplyContext,
    history_context: ProgramHistoryContext,
    plan_data: SharedPlanData,
    runner: &R,
) -> Result<ApplyResult> {
    clear_runtime_protocol_outputs(&request.workspace_root)?;
    let before_state = if request.debug {
        Some(snapshot_workspace_state(&request.workspace_root)?)
    } else {
        None
    };

    let history_injected_success = history_context.last_success.is_some();
    let history_injected_failure = history_context.last_failure.is_some();

    sync_runtime_variables_input(&request.workspace_root, &context.program_variables)?;
    let prompt = build_runtime_prompt(&context.program_raw, &plan_data, request.sandbox_mode)?;

    let run_result = match runner.run(&ProviderRequest {
        workspace_root: request.workspace_root.clone(),
        artifacts_root: Some(request.workspace_root.clone()),
        program_id: Some(context.program_key.clone()),
        model: context.resolved_model.clone(),
        agent_result_rel: AGENT_RESULT_REL.to_string(),
        sandbox_mode: request.sandbox_mode,
        prompt,
        progress: request.progress,
        render_progress: request.render_progress,
        debug_mode: request.debug,
        verbose_output: request.verbose_output,
        verbose_events: true,
        interactive_ui: request.interactive_ui,
        show_intermediate_steps: request.show_intermediate_steps,
    }) {
        Ok(run) => run,
        Err(err) => {
            let failure_session_id = derive_session_key(None);
            let _ = persist_session_outcome(
                &request.workspace_root,
                &context.program_key,
                &SessionOutcome {
                    program_id: context.program_key.clone(),
                    session_id: failure_session_id.clone(),
                    status: "failure".to_string(),
                    agent_status: None,
                    agent_message: None,
                    model: context.resolved_model.clone(),
                    error: Some(truncate_chars(
                        &format!("{:#}", err),
                        MAX_HISTORY_TEXT_CHARS,
                    )),
                    files_total: 0,
                    insertions: 0,
                    deletions: 0,
                    files_changed: Vec::new(),
                    input_tokens: None,
                    output_tokens: None,
                    cached_input_tokens: None,
                },
            );
            let _ = append_history_record(
                &request.workspace_root,
                &build_failure_history_record(
                    &context.program_key,
                    Some(failure_session_id.as_str()),
                    context.resolved_model.as_deref(),
                    Some(&format!("{:#}", err)),
                    None,
                ),
            );
            return Err(err);
        }
    };

    if let Err(err) = run_result.ensure_success() {
        let failure_session_id = derive_session_key(run_result.session_id.as_deref());
        let _ = persist_session_outcome(
            &request.workspace_root,
            &context.program_key,
            &SessionOutcome {
                program_id: context.program_key.clone(),
                session_id: failure_session_id.clone(),
                status: "failure".to_string(),
                agent_status: None,
                agent_message: None,
                model: context.resolved_model.clone(),
                error: Some(truncate_chars(
                    &format!("{:#}", err),
                    MAX_HISTORY_TEXT_CHARS,
                )),
                files_total: 0,
                insertions: 0,
                deletions: 0,
                files_changed: Vec::new(),
                input_tokens: run_result.usage.input_tokens,
                output_tokens: run_result.usage.output_tokens,
                cached_input_tokens: run_result.usage.cached_input_tokens,
            },
        );
        let _ = append_history_record(
            &request.workspace_root,
            &build_failure_history_record(
                &context.program_key,
                Some(failure_session_id.as_str()),
                context.resolved_model.as_deref(),
                Some(&format!("{:#}", err)),
                Some(&run_result),
            ),
        );
        return Err(err);
    }

    let success_session_id = derive_session_key(run_result.session_id.as_deref());
    let prompt_artifact = None;
    let plan_artifact = None;
    let provider_stdout_artifact = None;
    let provider_stderr_artifact = None;
    let events_artifact = None;
    let agent_reported_files =
        read_agent_reported_files(&request.workspace_root).unwrap_or_default();
    let agent_human_summary_explicit = read_agent_human_summary(&request.workspace_root)
        .ok()
        .flatten();
    let agent_result = read_agent_result(&request.workspace_root)?;
    let derived_summary = read_derived_agent_summary(
        &request.workspace_root,
        &context.program_key,
        &success_session_id,
    )
    .ok()
    .flatten();
    let agent_human_summary = agent_human_summary_explicit
        .clone()
        .or_else(|| derived_summary.as_ref().map(|d| d.text.clone()))
        .or_else(|| agent_result.as_ref().and_then(|r| r.message.clone()));
    let agent_human_summary_artifact = match agent_human_summary.as_deref() {
        Some(summary) => persist_agent_summary_artifact(
            &request.workspace_root,
            &context.program_key,
            &success_session_id,
            summary,
        )
        .ok(),
        None => None,
    };

    let (promoted_files, file_results) = if request.debug {
        let before_state = before_state
            .as_ref()
            .expect("before state must exist in debug mode");
        let after_state = snapshot_workspace_state(&request.workspace_root)?;
        let changed_files = filter_git_ignored_paths(
            &request.workspace_root,
            collect_changed_files_from_snapshots(before_state, &after_state),
        );
        let line_stats = compute_changed_line_stats(&changed_files, before_state, &after_state);
        let promoted_files = changed_files.clone();
        let file_results = build_file_results(&promoted_files, &agent_reported_files, &line_stats);
        (promoted_files, file_results)
    } else {
        (
            Vec::new(),
            build_reported_only_file_results(&agent_reported_files),
        )
    };
    let (files_total, insertions, deletions, files_changed) =
        summarize_reported_files(&file_results);

    if let Err(err) = validate_agent_completion(&agent_result) {
        let _ = persist_session_outcome(
            &request.workspace_root,
            &context.program_key,
            &SessionOutcome {
                program_id: context.program_key.clone(),
                session_id: success_session_id.clone(),
                status: "failure".to_string(),
                agent_status: agent_result.as_ref().map(|r| r.status.as_str().to_string()),
                agent_message: agent_result.as_ref().and_then(|r| r.message.clone()),
                model: context.resolved_model.clone(),
                error: Some(truncate_chars(
                    &format!("{:#}", err),
                    MAX_HISTORY_TEXT_CHARS,
                )),
                files_total,
                insertions,
                deletions,
                files_changed: files_changed.clone(),
                input_tokens: run_result.usage.input_tokens,
                output_tokens: run_result.usage.output_tokens,
                cached_input_tokens: run_result.usage.cached_input_tokens,
            },
        );
        let _ = append_history_record(
            &request.workspace_root,
            &build_failure_history_record(
                &context.program_key,
                Some(success_session_id.as_str()),
                context.resolved_model.as_deref(),
                Some(&format!("{:#}", err)),
                Some(&run_result),
            ),
        );
        return Err(err);
    }

    let _ = persist_program_snapshot(
        &request.workspace_root,
        &context.program_key,
        &success_session_id,
        &context.program_raw,
    );
    let _ = persist_program_variables_snapshot(
        &request.workspace_root,
        &context.program_key,
        &success_session_id,
        &context.program_variables,
    );
    let _ = persist_session_outcome(
        &request.workspace_root,
        &context.program_key,
        &SessionOutcome {
            program_id: context.program_key.clone(),
            session_id: success_session_id.clone(),
            status: "success".to_string(),
            agent_status: agent_result.as_ref().map(|r| r.status.as_str().to_string()),
            agent_message: agent_result.as_ref().and_then(|r| r.message.clone()),
            model: context.resolved_model.clone(),
            error: None,
            files_total,
            insertions,
            deletions,
            files_changed,
            input_tokens: run_result.usage.input_tokens,
            output_tokens: run_result.usage.output_tokens,
            cached_input_tokens: run_result.usage.cached_input_tokens,
        },
    );
    let _ = append_history_record(
        &request.workspace_root,
        &build_success_history_record(
            &context.program_key,
            Some(success_session_id.as_str()),
            context.resolved_model.as_deref(),
            agent_human_summary.as_deref(),
            &file_results,
            &run_result,
        ),
    );

    Ok(ApplyResult {
        provider_result: Some(run_result),
        promoted_files,
        agent_reported_files,
        agent_human_summary,
        agent_human_summary_artifact,
        agent_result,
        file_results,
        prompt_artifact,
        plan_artifact,
        provider_stdout_artifact,
        provider_stderr_artifact,
        events_artifact,
        history_injected_success,
        history_injected_failure,
    })
}

fn compute_changed_line_stats(
    changed_files: &[String],
    before_state: &BTreeMap<String, WorkspaceFileSnapshot>,
    after_state: &BTreeMap<String, WorkspaceFileSnapshot>,
) -> BTreeMap<String, LineStats> {
    let mut stats = BTreeMap::new();
    for rel in changed_files {
        let line_stats = match (before_state.get(rel), after_state.get(rel)) {
            (Some(before), Some(after)) => match (before.text.as_deref(), after.text.as_deref()) {
                (Some(before_text), Some(after_text)) => {
                    compute_line_stats_text(before_text, after_text)
                }
                _ => LineStats::default(),
            },
            (Some(before), None) => before
                .text
                .as_deref()
                .map(|before_text| compute_line_stats_text(before_text, ""))
                .unwrap_or_default(),
            (None, Some(after)) => after
                .text
                .as_deref()
                .map(|after_text| compute_line_stats_text("", after_text))
                .unwrap_or_default(),
            (None, None) => LineStats::default(),
        };
        stats.insert(rel.clone(), line_stats);
    }
    stats
}

fn compute_line_stats_text(before_text: &str, after_text: &str) -> LineStats {
    let diff = TextDiff::from_lines(before_text, after_text);
    let mut inserted = 0usize;
    let mut deleted = 0usize;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => inserted += 1,
            ChangeTag::Delete => deleted += 1,
            ChangeTag::Equal => {}
        }
    }

    let changed = inserted.min(deleted);
    LineStats {
        changed,
        added: inserted.saturating_sub(changed),
        deleted: deleted.saturating_sub(changed),
    }
}

fn build_file_results(
    promoted_files: &[String],
    agent_reported_files: &[String],
    line_stats: &BTreeMap<String, LineStats>,
) -> Vec<FileResult> {
    let promoted: BTreeSet<&str> = promoted_files.iter().map(String::as_str).collect();
    let reported: BTreeSet<&str> = agent_reported_files.iter().map(String::as_str).collect();
    let all: BTreeSet<&str> = promoted.union(&reported).copied().collect();

    let mut out = Vec::with_capacity(all.len());
    for path in all {
        let stats = line_stats.get(path).cloned().unwrap_or_default();
        out.push(FileResult {
            path: path.to_string(),
            changed: promoted.contains(path),
            reported: reported.contains(path),
            lines_changed: stats.changed,
            lines_added: stats.added,
            lines_deleted: stats.deleted,
        });
    }
    out
}

fn build_reported_only_file_results(agent_reported_files: &[String]) -> Vec<FileResult> {
    let mut files = agent_reported_files
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|path| FileResult {
            path,
            changed: false,
            reported: true,
            lines_changed: 0,
            lines_added: 0,
            lines_deleted: 0,
        })
        .collect::<Vec<_>>();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn summarize_reported_files(file_results: &[FileResult]) -> (usize, usize, usize, Vec<String>) {
    let mut files_total = 0usize;
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    let mut files_changed = Vec::new();

    for file in file_results {
        if !file.reported {
            continue;
        }
        files_total += 1;
        insertions += file.lines_added;
        deletions += file.lines_deleted;
        files_changed.push(file.path.clone());
    }
    files_changed.sort();

    (files_total, insertions, deletions, files_changed)
}

fn validate_agent_completion(agent_result: &Option<AgentResult>) -> Result<()> {
    let result = agent_result
        .as_ref()
        .ok_or_else(|| anyhow!("missing required '{}'", AGENT_RESULT_REL))?;

    match result.status {
        AgentStatus::Success => {
            if result.reason.is_some() {
                return Err(anyhow!(
                    "invalid '{}' content: status=success must omit reason",
                    AGENT_RESULT_REL
                ));
            }
            return Ok(());
        }
        AgentStatus::Partial | AgentStatus::Failure => {}
    }

    let reason = result.reason.ok_or_else(|| {
        anyhow!(
            "invalid '{}' content: status={} requires reason",
            AGENT_RESULT_REL,
            result.status.as_str()
        )
    })?;
    let mut details = String::new();
    details.push_str("reason=");
    details.push_str(reason.as_str());
    details.push_str("; ");
    details.push_str(
        result
            .message
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or("no message provided"),
    );

    match result.status {
        AgentStatus::Partial => Err(anyhow!(
            "agent reported partial completion in '{}': {}",
            AGENT_RESULT_REL,
            details
        )),
        AgentStatus::Failure => Err(anyhow!(
            "agent reported failure in '{}': {}",
            AGENT_RESULT_REL,
            details
        )),
        AgentStatus::Success => Ok(()),
    }
}

fn sync_runtime_variables_input(
    workspace_root: &Path,
    variables: &BTreeMap<String, String>,
) -> Result<()> {
    let path = workspace_root.join(RUNTIME_VARIABLES_INPUT_REL);
    if variables.is_empty() {
        if path.exists() {
            fs::remove_file(&path).with_context(|| {
                format!(
                    "failed removing runtime variables file '{}'",
                    path.display()
                )
            })?;
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating runtime variables directory '{}'",
                parent.display()
            )
        })?;
    }
    let body =
        serde_json::to_string_pretty(variables).context("failed serializing runtime variables")?;
    fs::write(&path, format!("{}\n", body))
        .with_context(|| format!("failed writing runtime variables file '{}'", path.display()))?;
    Ok(())
}

fn build_shared_plan_data(
    workspace_root: &Path,
    context: &ApplyContext,
    history: &ProgramHistoryContext,
) -> SharedPlanData {
    let last_session_vars_rel = history
        .last_session
        .as_ref()
        .and_then(|rec| rec.session_id.as_deref())
        .and_then(|sid| {
            let abs = program_variables_snapshot_path(workspace_root, &context.program_key, sid);
            if !abs.exists() {
                return None;
            }
            Some(
                abs.strip_prefix(workspace_root)
                    .map(to_slash_path)
                    .unwrap_or_else(|_| to_slash_path(&abs)),
            )
        });

    SharedPlanData {
        program_id: context.program_key.clone(),
        program_file: context.program_file.clone(),
        model: context.resolved_model.clone(),
        program_variables: if context.program_variables.is_empty() {
            None
        } else {
            Some(PlanProgramVariables {
                values_total: context.program_variables.len(),
                current_session_file: RUNTIME_VARIABLES_INPUT_REL.to_string(),
                last_session_file: last_session_vars_rel.clone(),
            })
        },
        program_variables_diff_vs_last_session: compute_program_variables_diff_vs_last_session(
            workspace_root,
            &context.program_key,
            &context.program_variables,
            history.last_session.as_ref(),
        ),
        program_diff_vs_last_session: compute_program_diff_vs_last_session(
            workspace_root,
            &context.program_key,
            &context.program_file,
            &context.program_raw,
            history.last_session.as_ref(),
        ),
        last_session: history.last_session.as_ref().map(|rec| PlanLastSession {
            session_id: rec.session_id.clone(),
            ts_unix: rec.ts_unix,
            status: rec.status.clone(),
            model: rec.model.clone(),
            summary_short: rec.summary_short.clone(),
            files_total: rec.files_total,
            files_sample: rec.files_sample.clone(),
            insertions: rec.insertions,
            deletions: rec.deletions,
            input_tokens: rec.input_tokens,
            output_tokens: rec.output_tokens,
            cached_input_tokens: rec.cached_input_tokens,
            error_short: rec.error_short.clone(),
        }),
    }
}

fn compute_program_variables_diff_vs_last_session(
    workspace_root: &Path,
    program_id: &str,
    current_values: &BTreeMap<String, String>,
    last_session: Option<&RunHistoryRecord>,
) -> Option<PlanProgramVariablesDiff> {
    let Some(last_session) = last_session else {
        if current_values.is_empty() {
            return None;
        }
        return Some(PlanProgramVariablesDiff {
            status: "first_apply".to_string(),
            values_changed: 0,
            values_added: current_values.len(),
            values_removed: 0,
        });
    };

    let Some(session_id) = last_session.session_id.as_deref() else {
        if current_values.is_empty() {
            return None;
        }
        return Some(PlanProgramVariablesDiff {
            status: "unavailable".to_string(),
            values_changed: 0,
            values_added: 0,
            values_removed: 0,
        });
    };

    let snapshot_path = program_variables_snapshot_path(workspace_root, program_id, session_id);
    let previous_values = match fs::read_to_string(&snapshot_path) {
        Ok(raw) => match serde_json::from_str::<BTreeMap<String, String>>(&raw) {
            Ok(values) => values,
            Err(_) => {
                if current_values.is_empty() {
                    return None;
                }
                return Some(PlanProgramVariablesDiff {
                    status: "unavailable".to_string(),
                    values_changed: 0,
                    values_added: 0,
                    values_removed: 0,
                });
            }
        },
        Err(_) => {
            if current_values.is_empty() {
                return None;
            }
            return Some(PlanProgramVariablesDiff {
                status: "unavailable".to_string(),
                values_changed: 0,
                values_added: 0,
                values_removed: 0,
            });
        }
    };

    if current_values.is_empty() && previous_values.is_empty() {
        return None;
    }

    let keys: BTreeSet<&str> = current_values
        .keys()
        .map(String::as_str)
        .chain(previous_values.keys().map(String::as_str))
        .collect();

    let mut values_changed = 0usize;
    let mut values_added = 0usize;
    let mut values_removed = 0usize;
    for key in keys {
        match (current_values.get(key), previous_values.get(key)) {
            (Some(current), Some(previous)) if current != previous => values_changed += 1,
            (Some(_), None) => values_added += 1,
            (None, Some(_)) => values_removed += 1,
            _ => {}
        }
    }

    let status = if values_changed == 0 && values_added == 0 && values_removed == 0 {
        "unchanged"
    } else {
        "changed"
    };

    Some(PlanProgramVariablesDiff {
        status: status.to_string(),
        values_changed,
        values_added,
        values_removed,
    })
}

fn compute_program_diff_vs_last_session(
    workspace_root: &Path,
    program_id: &str,
    program_file: &str,
    current_program_raw: &str,
    last_session: Option<&RunHistoryRecord>,
) -> PlanProgramDiff {
    let Some(last_session) = last_session else {
        return PlanProgramDiff {
            status: "first_apply".to_string(),
            file: program_file.to_string(),
            lines_changed: 0,
            lines_added: 0,
            lines_deleted: 0,
        };
    };

    let Some(session_id) = last_session.session_id.as_deref() else {
        return PlanProgramDiff {
            status: "unavailable".to_string(),
            file: program_file.to_string(),
            lines_changed: 0,
            lines_added: 0,
            lines_deleted: 0,
        };
    };

    let snapshot_path = program_snapshot_path(workspace_root, program_id, session_id);
    let Ok(previous_program_raw) = fs::read_to_string(&snapshot_path) else {
        return PlanProgramDiff {
            status: "unavailable".to_string(),
            file: program_file.to_string(),
            lines_changed: 0,
            lines_added: 0,
            lines_deleted: 0,
        };
    };

    let line_stats = compute_line_stats_text(&previous_program_raw, current_program_raw);
    let status = if line_stats.changed == 0 && line_stats.added == 0 && line_stats.deleted == 0 {
        "unchanged"
    } else {
        "changed"
    };

    PlanProgramDiff {
        status: status.to_string(),
        file: program_file.to_string(),
        lines_changed: line_stats.changed,
        lines_added: line_stats.added,
        lines_deleted: line_stats.deleted,
    }
}

fn run_status_str(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Success => "success",
        RunStatus::Failure => "failure",
    }
}

fn build_success_history_record(
    program_id: &str,
    session_id: Option<&str>,
    model: Option<&str>,
    summary: Option<&str>,
    file_results: &[FileResult],
    run: &ProviderRunResult,
) -> RunHistoryRecord {
    let mut changed_paths = Vec::new();
    let mut files_total = 0usize;
    let mut insertions = 0usize;
    let mut deletions = 0usize;

    for file in file_results {
        if !file.reported {
            continue;
        }
        files_total += 1;
        insertions += file.lines_added;
        deletions += file.lines_deleted;
        changed_paths.push(file.path.clone());
    }
    changed_paths.sort();

    RunHistoryRecord {
        ts_unix: now_unix_secs(),
        program_id: program_id.to_string(),
        session_id: session_id.map(ToOwned::to_owned),
        status: RunStatus::Success,
        model: model.map(ToOwned::to_owned),
        summary_short: summary.map(|s| truncate_chars(s, MAX_HISTORY_TEXT_CHARS)),
        files_total,
        insertions,
        deletions,
        files_sample: changed_paths
            .into_iter()
            .take(MAX_HISTORY_FILE_SAMPLE)
            .collect(),
        error_short: None,
        input_tokens: run.usage.input_tokens,
        output_tokens: run.usage.output_tokens,
        cached_input_tokens: run.usage.cached_input_tokens,
    }
}

fn build_failure_history_record(
    program_id: &str,
    session_id: Option<&str>,
    model: Option<&str>,
    error: Option<&str>,
    run: Option<&ProviderRunResult>,
) -> RunHistoryRecord {
    RunHistoryRecord {
        ts_unix: now_unix_secs(),
        program_id: program_id.to_string(),
        session_id: session_id.map(ToOwned::to_owned),
        status: RunStatus::Failure,
        model: model.map(ToOwned::to_owned),
        summary_short: None,
        files_total: 0,
        insertions: 0,
        deletions: 0,
        files_sample: Vec::new(),
        error_short: error.map(|e| truncate_chars(e, MAX_HISTORY_TEXT_CHARS)),
        input_tokens: run.and_then(|r| r.usage.input_tokens),
        output_tokens: run.and_then(|r| r.usage.output_tokens),
        cached_input_tokens: run.and_then(|r| r.usage.cached_input_tokens),
    }
}

fn truncate_chars(raw: &str, max: usize) -> String {
    let trimmed = raw.replace('\n', " ").replace('\r', " ");
    let trimmed = trimmed.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }

    let mut out = String::new();
    for ch in trimmed.chars().take(max.saturating_sub(3)) {
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn build_runtime_prompt(
    _program_raw: &str,
    plan_data: &SharedPlanData,
    sandbox_mode: SandboxMode,
) -> Result<String> {
    let mut block = String::new();
    let program_file = plan_data.program_file.as_str();
    let program_id = plan_data.program_id.as_str();
    let program_dir = sanitize_storage_token(program_id, "program");
    let program_variables = plan_data.program_variables.as_ref();
    let sandbox_verdict_guidance_enabled =
        matches!(sandbox_mode, SandboxMode::Sandboxed | SandboxMode::Auto);
    let sandbox_auto_retry_enabled = matches!(sandbox_mode, SandboxMode::Auto);

    if let Some(last) = &plan_data.last_session {
        let session_id_raw = last
            .session_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let session_id = sanitize_storage_token(session_id_raw.as_str(), "session");
        let session_root = format!(".clawform/programs/{program_dir}/sessions/{session_id}");
        let last_program_file = format!("{session_root}/program.md");
        let last_output_file = format!("{session_root}/output.md");
        let history_path = format!(".clawform/programs/{program_dir}/sessions/");
        let change_summary = match plan_data.program_diff_vs_last_session.status.as_str() {
            "unavailable" => "unavailable".to_string(),
            _ => format_program_diff_totals(
                plan_data.program_diff_vs_last_session.lines_changed,
                plan_data.program_diff_vs_last_session.lines_added,
                plan_data.program_diff_vs_last_session.lines_deleted,
            ),
        };
        let compare_from = if plan_data.program_diff_vs_last_session.status == "unavailable" {
            "not available".to_string()
        } else {
            last_program_file.clone()
        };

        block.push_str("Clawform apply session contract\n\n");
        block.push_str("You are running the \"current session\".\n\n");
        block.push_str("Fixed terms used in this prompt:\n");
        block.push_str("- \"program\": the new program version for this session, stored at `");
        block.push_str(program_file);
        block.push_str("`\n");
        block.push_str("- \"current session\": this session\n");
        block.push_str("- \"last session\": the most recent finished session for this program\n\n");
        block.push_str("What is expected in this \"current session\":\n");
        block.push_str("- Complete the \"program\".\n");
        block.push_str(
            "- Use files and tools in the workspace as needed to complete the \"program\".\n",
        );
        block.push_str("- Use \"last session\" details to understand what was already done.\n");
        block.push_str(
            "- Keep correct work from \"last session\"; do not redo work without a clear reason.\n",
        );
        block.push_str("- If program changes require updates, apply only the updates required by those changes.\n");
        if program_variables.is_some() {
            block.push_str("- Use the resolved program variable values provided in this prompt as the source of truth for `${{ var.NAME }}` references.\n");
        }
        block.push_str("- If verification shows issues, fix them in this \"current session\".\n");
        block.push_str("- Continue until the program result is correct, or stop only when there is no practical way forward.\n");
        block.push_str("- You may change workspace files, but only files needed to complete the \"program\".\n\n");
        block.push_str("Required execution order:\n");
        block.push_str("1) Read the new program version: `");
        block.push_str(program_file);
        block.push_str("`.\n");
        block.push_str("2) Read \"last session\" files:\n");
        block.push_str("   `");
        block.push_str(last_program_file.as_str());
        block.push_str("`\n");
        block.push_str("   and\n");
        block.push_str("   `");
        block.push_str(last_output_file.as_str());
        block.push_str("`.\n");
        block.push_str("3) Read program changes between:\n");
        block.push_str("   `");
        block.push_str(compare_from.as_str());
        block.push_str("`\n");
        block.push_str("   and\n");
        block.push_str("   the new program version (`");
        block.push_str(program_file);
        block.push_str("`).\n");
        block.push_str("4) Execute the \"program\" for this \"current session\".\n");
        block.push_str("5) Before finishing, write both required report files:\n");
        block.push_str("   `./");
        block.push_str(AGENT_OUTPUT_MANIFEST_REL);
        block.push_str("`\n");
        block.push_str("   and\n");
        block.push_str("   `./");
        block.push_str(AGENT_RESULT_REL);
        block.push_str("`.\n\n");
        block.push_str("Program\n\n");
        block.push_str("- Program ID: `");
        block.push_str(program_id);
        block.push_str("`\n");
        block.push_str("- New program version: `");
        block.push_str(program_file);
        block.push_str("`\n");
        if let Some(vars) = program_variables {
            block.push_str("- Resolved program variables: `");
            block.push_str(vars.values_total.to_string().as_str());
            block.push_str("`\n");
            block.push_str("- Resolved program variables file: `./");
            block.push_str(vars.current_session_file.as_str());
            block.push_str("`\n");
        }
        block.push('\n');

        if let Some(vars) = program_variables {
            block.push_str("Resolved program variables for this \"current session\"\n\n");
            block.push_str("- Read this file for `${{ var.NAME }}` values:\n");
            block.push_str("  `./");
            block.push_str(vars.current_session_file.as_str());
            block.push_str("`\n");
            block.push_str("- The file contains `");
            block.push_str(vars.values_total.to_string().as_str());
            block.push_str("` resolved variable value(s) for this run.\n\n---\n\n");
        } else {
            block.push_str("---\n\n");
        }
        block.push_str("Last session details\n\n");
        block.push_str("- last_session_id: `");
        block.push_str(session_id_raw.as_str());
        block.push_str("`\n");
        block.push_str("- last_session_status: `");
        block.push_str(run_status_str(&last.status));
        block.push_str("`\n");
        block.push_str("- last_session_time_unix: `");
        block.push_str(last.ts_unix.to_string().as_str());
        block.push_str("`\n");
        block.push_str("- last_session_program_file: `");
        block.push_str(last_program_file.as_str());
        block.push_str("`\n");
        block.push_str("- last_session_output_file: `");
        block.push_str(last_output_file.as_str());
        block.push_str("`\n");
        if let Some(last_vars_file) = program_variables.and_then(|v| v.last_session_file.as_deref())
        {
            block.push_str("- last_session_variables_file: `");
            block.push_str(last_vars_file);
            block.push_str("`\n");
        }
        block.push_str("- session_history_path (open only if needed): `");
        block.push_str(history_path.as_str());
        block.push_str("`\n\n");
        block.push_str("How to use \"last session\" details in this \"current session\":\n");
        block.push_str("- Understand what was completed in \"last session\".\n");
        block.push_str("- Verify whether that result is still correct for the \"program\".\n");
        if program_variables.is_some() {
            block.push_str("- Treat current session variable values in this prompt as the source of truth for this run.\n");
        }
        block.push_str("- If \"last session\" work is still correct and program changes do not require more edits, keep that work.\n");
        block.push_str("- If \"last session\" work is incorrect or incomplete for the \"program\", update it.\n\n---\n\n");
        block.push_str("Program changes since last session\n\n");
        block.push_str("- Last session program file to compare from:\n");
        block.push_str("  `");
        block.push_str(compare_from.as_str());
        block.push_str("`\n");
        block.push_str("- Program file for the \"current session\" to compare to:\n");
        block.push_str("  `");
        block.push_str(program_file);
        block.push_str("`\n");
        block.push_str("- Program change summary:\n");
        block.push_str("  `");
        block.push_str(change_summary.as_str());
        block.push_str("`\n\n");
        block.push_str("How to apply program changes in this \"current session\":\n");
        block.push_str("- Treat the new program version as what you must implement now.\n");
        block.push_str("- Use the program change summary in this prompt to understand what changed since \"last session\".\n");
        block.push_str("- Apply only the edits needed to satisfy the changed program.\n");
        block.push_str("- If no meaningful program change exists, first verify the result is still correct; only edit files if verification finds a real gap.\n\n---\n\n");
        block.push_str("Execution and stop rules for this \"current session\"\n\n");
        block.push_str("- Keep working until the new program version is satisfied.\n");
        block.push_str(
            "- Stop only if there is no practical way to complete the \"program\" in this environment.\n",
        );
        block.push_str("- If blocked, report that in the required status file.\n");
        if sandbox_verdict_guidance_enabled {
            block.push_str(
                "- Verdict gate (required): after the first restriction symptom, stop normal task work and classify the block cause.\n",
            );
            block.push_str(
                "- Restriction symptom means any sandbox-denial, file-access/permission error, or network/connection restriction signal.\n",
            );
            block.push_str(
                "- Do not grade severity: even non-fatal warnings are sandbox evidence.\n",
            );
            block.push_str(
                "- After a restriction symptom, you may run at most one read-only check that does not require the blocked capability.\n",
            );
            block.push_str(
                "- If any sandbox evidence remains, or evidence is mixed/uncertain, write `./",
            );
            block.push_str(AGENT_RESULT_REL);
            block.push_str("` with `status: failure` and `reason: sandbox_blocked`, then stop.\n");
            block.push_str("- Use `reason: program_blocked` only when zero restriction symptoms appeared in failing required commands and one read-only check confirms an independent non-sandbox cause.\n");
            block.push_str("- Mandatory self-check before writing `program_blocked`: \"Did any restriction symptom appear in this run?\" If yes, change reason to `sandbox_blocked`.\n");
            block.push_str("- No workaround/fallback commands before verdict.\n");
            if sandbox_auto_retry_enabled {
                block.push_str("- Auto mode only: if blocked by sandbox, set `reason: sandbox_blocked`; this triggers one unsandboxed retry.\n");
            }
        }
        block.push_str("- Keep edits within program scope:\n");
        block.push_str("  files required to satisfy the \"program\".\n");
        block.push_str("- Do not make unrelated edits.\n\n---\n\n");
        block.push_str("Required report files for this \"current session\" (must write both)\n\n");
        block.push_str("1) `./");
        block.push_str(AGENT_OUTPUT_MANIFEST_REL);
        block.push_str("`\n\n");
        block.push_str("Exact format:\n");
        block.push_str("```json\n[\n  { \"path\": \"relative/path.ext\", \"change\": \"created|modified|deleted\" }\n]\n```\n\n");
        block.push_str("Rules:\n");
        block.push_str("- Include files created/modified/deleted in this \"current session\".\n");
        block.push_str("- Use repo-relative paths.\n");
        block.push_str("- Exclude `.clawform/*` bookkeeping files.\n");
        block.push_str("- Deduplicate entries.\n\n");
        block.push_str("2) `./");
        block.push_str(AGENT_RESULT_REL);
        block.push_str("`\n\n");
        block.push_str("Exact format:\n");
        block.push_str("```json\n{\n  \"status\": \"success|partial|failure\",\n  \"reason\": \"sandbox_blocked|program_blocked\",\n  \"message\": \"short human-readable summary\"\n}\n```\n\n");
        block.push_str("Rules:\n");
        block.push_str("- `success`: the \"program\" is complete and correct.\n");
        block.push_str("- `partial`: useful progress was made, but program is not complete.\n");
        block.push_str("- `failure`: program could not be completed.\n");
        block.push_str("- For `partial` or `failure`, set `reason`.\n");
        block.push_str("- For `success`, omit `reason`.\n");
        block.push_str("- Reason precedence: use `sandbox_blocked` if any restriction symptom appears in a failing required command, or evidence is mixed/uncertain; use `program_blocked` only when zero restriction symptoms appeared and an independent non-sandbox cause is confirmed.\n");
        if sandbox_verdict_guidance_enabled {
            block.push_str(
                "- Write this verdict before any fallback/workaround/mutating commands.\n",
            );
        }
        block.push_str(
            "- `message`: one short sentence about this \"current session\" result.\n\n---\n\n",
        );
        block.push_str("User-facing message rule for this \"current session\"\n\n");
        block.push_str("- In user-facing text, describe program results only.\n");
        block.push_str(
            "- Do not mention `.clawform/*` bookkeeping files unless user explicitly asks.\n",
        );
        return Ok(block);
    } else {
        block.push_str("Clawform apply session contract\n\n");
        block.push_str("Current session\n");
        block.push_str("- Program ID: `");
        block.push_str(program_id);
        block.push_str("`\n");
        block.push_str("- Program: `");
        block.push_str(program_file);
        block.push_str("`\n\n");
        block.push_str("Session context\n");
        block.push_str(
            "- This program is being performed for the first time (no previous sessions).\n\n",
        );
        block.push_str("What to do in this session\n");
        block.push_str("- Read and implement `");
        block.push_str(program_file);
        block.push_str("`.\n");
        if program_variables.is_some() {
            block.push_str("- Use the resolved program variable values provided in this prompt as the source of truth for `${{ var.NAME }}` references.\n");
        }
        block.push_str("- Use workspace files and tools as needed.\n");
        block.push_str("- Continue until the program result is correct, or stop only when there is no practical way forward.\n");
        if sandbox_verdict_guidance_enabled {
            block.push_str(
                "- Verdict gate (required): after the first restriction symptom, stop normal task work and classify the block cause.\n",
            );
            block.push_str(
                "- Restriction symptom means any sandbox-denial, file-access/permission error, or network/connection restriction signal.\n",
            );
            block.push_str(
                "- Do not grade severity: even non-fatal warnings are sandbox evidence.\n",
            );
            block.push_str(
                "- After a restriction symptom, you may run at most one read-only check that does not require the blocked capability.\n",
            );
            block.push_str(
                "- If any sandbox evidence remains, or evidence is mixed/uncertain, write `./",
            );
            block.push_str(AGENT_RESULT_REL);
            block.push_str("` with `status: failure` and `reason: sandbox_blocked`, then stop.\n");
            block.push_str("- Use `reason: program_blocked` only when zero restriction symptoms appeared in failing required commands and one read-only check confirms an independent non-sandbox cause.\n");
            block.push_str("- Mandatory self-check before writing `program_blocked`: \"Did any restriction symptom appear in this run?\" If yes, change reason to `sandbox_blocked`.\n");
            block.push_str("- No workaround/fallback commands before verdict.\n");
            if sandbox_auto_retry_enabled {
                block.push_str("- Auto mode only: if blocked by sandbox, set `reason: sandbox_blocked`; this triggers one unsandboxed retry.\n");
            }
        }
        block.push_str("- Keep edits scoped to files needed for this program.\n");
        block.push_str("- Do not make unrelated edits.\n\n");
        if let Some(vars) = program_variables {
            block.push_str("Resolved program variables for this session\n");
            block.push_str("- Read `${{ var.NAME }}` values from `./");
            block.push_str(vars.current_session_file.as_str());
            block.push_str("` for `");
            block.push_str(program_file);
            block.push_str("`.\n");
            block.push_str("- Resolved variable count: `");
            block.push_str(vars.values_total.to_string().as_str());
            block.push_str("`.\n\n");
        }
        block.push_str("Before finishing this session (required)\n");
        block.push_str("- Write `./");
        block.push_str(AGENT_OUTPUT_MANIFEST_REL);
        block.push_str("`.\n");
        block.push_str("- Write `./");
        block.push_str(AGENT_RESULT_REL);
        block.push_str("`.\n\n");
    }

    block.push_str("Required report files\n\n");
    block.push_str("1) `./");
    block.push_str(AGENT_OUTPUT_MANIFEST_REL);
    block.push_str("`\n\n");
    block.push_str("Exact format:\n");
    block.push_str("```json\n[\n  { \"path\": \"relative/path.ext\", \"change\": \"created|modified|deleted\" }\n]\n```\n\n");
    block.push_str("Rules:\n");
    block.push_str("- Include files created/modified/deleted in this session.\n");
    block.push_str("- Use repo-relative paths.\n");
    block.push_str("- Exclude `.clawform/*` bookkeeping files.\n");
    block.push_str("- Deduplicate entries.\n\n");
    block.push_str("2) `./");
    block.push_str(AGENT_RESULT_REL);
    block.push_str("`\n\n");
    block.push_str("Exact format:\n");
    block.push_str("```json\n{\n  \"status\": \"success|partial|failure\",\n  \"reason\": \"sandbox_blocked|program_blocked\",\n  \"message\": \"short human-readable summary\"\n}\n```\n\n");
    block.push_str("Rules:\n");
    block.push_str("- `success`: program is complete and correct.\n");
    block.push_str("- `partial`: useful progress was made, but program is not complete.\n");
    block.push_str("- `failure`: program could not be completed.\n");
    block.push_str("- For `partial` or `failure`, set `reason`.\n");
    block.push_str("- For `success`, omit `reason`.\n");
    block.push_str("- Reason precedence: use `sandbox_blocked` if any restriction symptom appears in a failing required command, or evidence is mixed/uncertain; use `program_blocked` only when zero restriction symptoms appeared and an independent non-sandbox cause is confirmed.\n");
    if sandbox_verdict_guidance_enabled {
        block.push_str("- Write this verdict before any fallback/workaround/mutating commands.\n");
    }
    block.push_str("- `message`: one short sentence about this session result.\n\n");
    block.push_str("User-facing message rule\n");
    block.push_str("- In user-facing text, describe program results only.\n");
    block.push_str("- Do not mention `.clawform/*` bookkeeping files unless explicitly asked.\n");

    Ok(block)
}

fn read_agent_reported_files(workspace_root: &Path) -> Result<Vec<String>> {
    let path = workspace_root.join(AGENT_OUTPUT_MANIFEST_REL);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed reading agent output manifest '{}'", path.display()))?;
    let parsed: Vec<AgentOutputManifestEntry> = serde_json::from_str(&raw)
        .with_context(|| format!("invalid JSON in agent output manifest '{}'", path.display()))?;

    let mut out = Vec::new();
    for item in parsed {
        let path_str = match item {
            AgentOutputManifestEntry::Path(path) => path,
            AgentOutputManifestEntry::Record(record) => {
                if let Some(change) = record.change.as_deref() {
                    let normalized_change = change.trim().to_ascii_lowercase();
                    if !matches!(
                        normalized_change.as_str(),
                        "created" | "modified" | "deleted"
                    ) {
                        continue;
                    }
                }
                record.path
            }
        };
        if let Some(normalized) = normalize_reported_rel_path(&path_str) {
            if is_internal_reported_path(normalized.as_str()) {
                continue;
            }
            out.push(normalized);
        }
    }

    out.sort();
    out.dedup();
    Ok(out)
}

fn is_internal_reported_path(path: &str) -> bool {
    path == AGENT_OUTPUT_MANIFEST_REL
        || path == AGENT_HUMAN_OUTPUT_REL
        || path == AGENT_RESULT_REL
        || path == RUNTIME_VARIABLES_INPUT_REL
}

fn read_agent_human_summary(workspace_root: &Path) -> Result<Option<String>> {
    let path = workspace_root.join(AGENT_HUMAN_OUTPUT_REL);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read(&path)
        .with_context(|| format!("failed reading agent output note '{}'", path.display()))?;
    let text = String::from_utf8_lossy(&raw).replace("\r\n", "\n");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    Ok(Some(trimmed.to_string()))
}

fn read_agent_result(workspace_root: &Path) -> Result<Option<AgentResult>> {
    let path = workspace_root.join(AGENT_RESULT_REL);
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed reading agent result '{}'", path.display()))?;
    let parsed: AgentResultFile = serde_json::from_str(&raw)
        .with_context(|| format!("invalid JSON in agent result '{}'", path.display()))?;
    let message = parsed.message.and_then(|m| {
        let trimmed = m.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    Ok(Some(AgentResult {
        status: parsed.status,
        reason: parsed.reason,
        message,
    }))
}

fn clear_runtime_protocol_outputs(workspace_root: &Path) -> Result<()> {
    for rel in [
        AGENT_OUTPUT_MANIFEST_REL,
        AGENT_HUMAN_OUTPUT_REL,
        AGENT_RESULT_REL,
    ] {
        let path = workspace_root.join(rel);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed clearing protocol output file '{}'", path.display())
                });
            }
        }
    }
    Ok(())
}

fn read_derived_agent_summary(
    workspace_root: &Path,
    program_key: &str,
    session_id: &str,
) -> Result<Option<DerivedSummary>> {
    for sid in [session_id, "unknown"] {
        let messages_dir = program_session_dir(workspace_root, program_key, sid).join("messages");
        if !messages_dir.exists() {
            continue;
        }
        let Some(summary) = pick_summary_from_messages_dir(&messages_dir)? else {
            continue;
        };
        return Ok(Some(DerivedSummary { text: summary }));
    }
    Ok(None)
}

fn pick_summary_from_messages_dir(messages_dir: &Path) -> Result<Option<String>> {
    let mut candidates: Vec<(u64, String, String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(messages_dir).with_context(|| {
        format!(
            "failed reading session messages directory '{}'",
            messages_dir.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed reading message artifact '{}'", path.display()))?;
        let Some((item_type, message)) = parse_message_artifact(&raw) else {
            continue;
        };
        if !is_agent_text_item_type_str(&item_type) {
            continue;
        }
        if message.trim().is_empty() {
            continue;
        }
        let order = parse_item_sequence_from_path(&path).unwrap_or(0);
        candidates.push((order, item_type, message, path));
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let mut fallback: Option<String> = None;
    for (_, _, message, _path) in candidates.into_iter().rev() {
        if fallback.is_none() {
            fallback = Some(message.clone());
        }
        if !is_low_signal_note_text(&message) {
            return Ok(Some(message));
        }
    }
    Ok(fallback)
}

fn parse_message_artifact(raw: &str) -> Option<(String, String)> {
    let normalized = raw.replace("\r\n", "\n");
    let after_type = normalized.strip_prefix("# type\n")?;
    let (item_type, rest) = after_type.split_once("\n\n# message\n")?;
    let item_type = item_type.trim().to_string();
    if item_type.is_empty() {
        return None;
    }
    let message = rest.trim().to_string();
    if message.is_empty() {
        return None;
    }
    Some((item_type, message))
}

fn parse_item_sequence_from_path(path: &Path) -> Option<u64> {
    let stem = path.file_stem()?.to_str()?;
    let suffix = stem.strip_prefix("item_")?;
    suffix.parse::<u64>().ok()
}

fn is_agent_text_item_type_str(item_type: &str) -> bool {
    matches!(
        item_type,
        "assistant_message" | "agent_message" | "message" | "output_text" | "text"
    )
}

fn is_low_signal_note_text(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("preparing final message")
        || t.contains("summarizing final response")
        || t.contains("summarizing final output")
        || t.contains("craft the final response")
        || t.contains("getting ready to craft")
        || t.contains("final response plan")
}

fn persist_agent_summary_artifact(
    workspace_root: &Path,
    program_key: &str,
    session_id: &str,
    summary: &str,
) -> Result<String> {
    let abs = program_session_dir(workspace_root, program_key, session_id).join("output.md");
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating summary artifact directory '{}'",
                parent.display()
            )
        })?;
    }

    let mut body = summary.trim().to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    fs::write(&abs, body)
        .with_context(|| format!("failed writing summary artifact '{}'", abs.display()))?;
    let rel = abs
        .strip_prefix(workspace_root)
        .map(to_slash_path)
        .unwrap_or_else(|_| to_slash_path(&abs));
    Ok(rel)
}

fn derive_session_key(session_id: Option<&str>) -> String {
    match session_id {
        Some(raw) if !raw.trim().is_empty() => sanitize_storage_token(raw, "session"),
        _ => format!("local-{}", now_unix_secs()),
    }
}

fn program_session_dir(workspace_root: &Path, program_id: &str, session_id: &str) -> PathBuf {
    workspace_root
        .join(".clawform")
        .join("programs")
        .join(sanitize_storage_token(program_id, "program"))
        .join("sessions")
        .join(sanitize_storage_token(session_id, "session"))
}

fn program_snapshot_path(workspace_root: &Path, program_id: &str, session_id: &str) -> PathBuf {
    program_session_dir(workspace_root, program_id, session_id).join("program.md")
}

fn program_variables_snapshot_path(
    workspace_root: &Path,
    program_id: &str,
    session_id: &str,
) -> PathBuf {
    program_session_dir(workspace_root, program_id, session_id).join("variables.json")
}

fn persist_program_snapshot(
    workspace_root: &Path,
    program_id: &str,
    session_id: &str,
    program_raw: &str,
) -> Result<String> {
    let abs = program_snapshot_path(workspace_root, program_id, session_id);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating program snapshot directory '{}'",
                parent.display()
            )
        })?;
    }

    fs::write(&abs, program_raw)
        .with_context(|| format!("failed writing program snapshot '{}'", abs.display()))?;

    let rel = abs
        .strip_prefix(workspace_root)
        .map(to_slash_path)
        .unwrap_or_else(|_| to_slash_path(&abs));
    Ok(rel)
}

fn persist_program_variables_snapshot(
    workspace_root: &Path,
    program_id: &str,
    session_id: &str,
    variables: &BTreeMap<String, String>,
) -> Result<String> {
    let abs = program_variables_snapshot_path(workspace_root, program_id, session_id);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating program variables snapshot directory '{}'",
                parent.display()
            )
        })?;
    }

    let body = serde_json::to_string_pretty(variables)
        .context("failed serializing program variables snapshot")?;
    fs::write(&abs, format!("{}\n", body)).with_context(|| {
        format!(
            "failed writing program variables snapshot '{}'",
            abs.display()
        )
    })?;

    let rel = abs
        .strip_prefix(workspace_root)
        .map(to_slash_path)
        .unwrap_or_else(|_| to_slash_path(&abs));
    Ok(rel)
}

fn persist_session_outcome(
    workspace_root: &Path,
    program_id: &str,
    outcome: &SessionOutcome,
) -> Result<String> {
    let abs = program_session_dir(workspace_root, program_id, outcome.session_id.as_str())
        .join("outcome.json");
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating outcome directory '{}'", parent.display()))?;
    }

    let body =
        serde_json::to_string_pretty(outcome).context("failed serializing session outcome")?;
    fs::write(&abs, format!("{}\n", body))
        .with_context(|| format!("failed writing session outcome '{}'", abs.display()))?;

    let rel = abs
        .strip_prefix(workspace_root)
        .map(to_slash_path)
        .unwrap_or_else(|_| to_slash_path(&abs));
    Ok(rel)
}

fn sanitize_storage_token(raw: &str, fallback: &str) -> String {
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

fn normalize_reported_rel_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        return None;
    }

    let mut normalized = PathBuf::new();
    for comp in candidate.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(seg) => normalized.push(seg),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if normalized.as_os_str().is_empty() {
        return None;
    }

    Some(to_slash_path(&normalized))
}

fn snapshot_workspace_state(
    workspace_root: &Path,
) -> Result<BTreeMap<String, WorkspaceFileSnapshot>> {
    if !workspace_root.is_dir() {
        return Err(anyhow!(
            "workspace root '{}' is not a directory",
            workspace_root.display()
        ));
    }

    let mut state = BTreeMap::new();
    snapshot_workspace_state_recursive(workspace_root, workspace_root, &mut state)?;
    Ok(state)
}

fn snapshot_workspace_state_recursive(
    workspace_root: &Path,
    current_dir: &Path,
    state: &mut BTreeMap<String, WorkspaceFileSnapshot>,
) -> Result<()> {
    for entry in fs::read_dir(current_dir)
        .with_context(|| format!("failed reading directory '{}'", current_dir.display()))?
    {
        let entry = entry?;
        let abs = entry.path();
        let rel = abs
            .strip_prefix(workspace_root)
            .with_context(|| format!("failed computing relative path for '{}'", abs.display()))?
            .to_path_buf();
        if should_skip_path(&rel) {
            continue;
        }

        let ft = entry.file_type()?;
        if ft.is_dir() {
            snapshot_workspace_state_recursive(workspace_root, &abs, state)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }

        let rel_key = to_slash_path(&rel);
        let hash = hash_file_or_missing(&abs)?;
        let text = read_snapshot_text(&abs)?;
        state.insert(rel_key, WorkspaceFileSnapshot { hash, text });
    }

    Ok(())
}

fn read_snapshot_text(path: &Path) -> Result<Option<String>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed reading metadata '{}'", path.display()))?;
    if metadata.len() as usize > SNAPSHOT_TEXT_LIMIT_BYTES {
        return Ok(None);
    }

    let bytes =
        fs::read(path).with_context(|| format!("failed reading file '{}'", path.display()))?;
    if bytes.len() > SNAPSHOT_TEXT_LIMIT_BYTES {
        return Ok(None);
    }

    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    Ok(Some(text.replace("\r\n", "\n")))
}

fn collect_changed_files_from_snapshots(
    before_state: &BTreeMap<String, WorkspaceFileSnapshot>,
    after_state: &BTreeMap<String, WorkspaceFileSnapshot>,
) -> Vec<String> {
    let all_paths: BTreeSet<&str> = before_state
        .keys()
        .map(String::as_str)
        .chain(after_state.keys().map(String::as_str))
        .collect();

    all_paths
        .into_iter()
        .filter(|path| {
            let before_hash = before_state.get(*path).map(|f| f.hash.as_str());
            let after_hash = after_state.get(*path).map(|f| f.hash.as_str());
            before_hash != after_hash
        })
        .map(ToOwned::to_owned)
        .collect()
}

fn filter_git_ignored_paths(workspace_root: &Path, changed_files: Vec<String>) -> Vec<String> {
    if changed_files.is_empty() {
        return changed_files;
    }

    let mut child = match Command::new("git")
        .arg("check-ignore")
        .arg("--stdin")
        .current_dir(workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return changed_files,
    };

    if let Some(mut stdin) = child.stdin.take() {
        let mut body = String::new();
        for path in &changed_files {
            body.push_str(path);
            body.push('\n');
        }
        if stdin.write_all(body.as_bytes()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return changed_files;
        }
    } else {
        let _ = child.kill();
        let _ = child.wait();
        return changed_files;
    }

    let output = match child.wait_with_output() {
        Ok(out) => out,
        Err(_) => return changed_files,
    };

    if output.status.code() == Some(128) {
        return changed_files;
    }

    let ignored: HashSet<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.replace('\\', "/"))
        .collect();

    if ignored.is_empty() {
        return changed_files;
    }

    changed_files
        .into_iter()
        .filter(|p| !ignored.contains(p))
        .collect()
}

fn confirm_interactive() -> Result<bool> {
    ensure_interrupt_handler()?;
    clear_interrupt_request();

    let use_color = io::stdout().is_terminal();
    if use_color {
        print!("\x1b[1mProceed?\x1b[0m \x1b[2m[y/N]\x1b[0m ");
    } else {
        print!("Proceed? [y/N] ");
    }
    io::stdout().flush().context("failed flushing prompt")?;

    let (tx, rx) = mpsc::channel::<Result<String, io::Error>>();
    std::thread::spawn(move || {
        let mut line = String::new();
        let res = io::stdin().read_line(&mut line).map(|_| line);
        let _ = tx.send(res);
    });

    loop {
        if interrupt_requested() {
            clear_interrupt_request();
            return Err(anyhow!("apply cancelled by user (Ctrl-C)"));
        }

        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(line)) => return Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES")),
            Ok(Err(err)) if err.kind() == io::ErrorKind::Interrupted => {
                if interrupt_requested() {
                    clear_interrupt_request();
                    return Err(anyhow!("apply cancelled by user (Ctrl-C)"));
                }
            }
            Ok(Err(err)) => return Err(err).context("failed reading confirmation"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(false);
            }
        }
    }
}

fn print_plan_preview(plan: &SharedPlanData, debug: bool, workspace_root: &Path) {
    let use_color = io::stdout().is_terminal();

    if let Some(last) = &plan.last_session {
        let status = match last.status {
            RunStatus::Success => color_success("success", use_color),
            RunStatus::Failure => color_error("failure", use_color),
        };
        let session = last.session_id.as_deref().unwrap_or("unknown");
        let age = format_age(last.ts_unix);
        println!(
            "{} {} ({}, {})",
            color_dim("Last session:", use_color),
            session,
            status,
            age,
        );

        let (diff_file, diff_summary) = format_program_diff_preview(plan);
        println!(
            "  {} {} {}",
            color_dim("program:", use_color),
            color_path(&diff_file, use_color),
            diff_summary
        );
        if let Some(variables_summary) = format_program_variables_diff_preview(plan) {
            println!(
                "  {} {}",
                color_dim("variables:", use_color),
                variables_summary
            );
        }

        if let Some(summary) = last.summary_short.as_deref() {
            if !summary.trim().is_empty() {
                let summary_line = truncate_chars(summary, 100);
                let output_hint = last
                    .session_id
                    .as_deref()
                    .map(|sid| output_artifact_rel_path(workspace_root, &plan.program_id, sid))
                    .unwrap_or_else(|| "<unknown>".to_string());
                let icon = if use_color {
                    "\x1b[35m💬\x1b[0m"
                } else {
                    "💬"
                };
                let msg_link = format_msg_link(workspace_root, &output_hint, use_color);
                println!("  {} {} | {}", icon, summary_line, msg_link);
            }
        }

        println!(
            "  {} {}",
            color_dim("changes:", use_color),
            format_file_count(last.files_total)
        );
        for path in &last.files_sample {
            println!("    {}", color_path(path, use_color));
        }
        let more = last.files_total.saturating_sub(last.files_sample.len());
        if more > 0 {
            println!("    … +{} more", more);
        }
    } else {
        println!("{} none", color_dim("Last session:", use_color));
        let (diff_file, diff_summary) = format_program_diff_preview(plan);
        println!(
            "{} {} {}",
            color_dim("program:", use_color),
            color_path(&diff_file, use_color),
            diff_summary
        );
        if let Some(variables_summary) = format_program_variables_diff_preview(plan) {
            println!(
                "{} {}",
                color_dim("variables:", use_color),
                variables_summary
            );
        }
    }

    if debug {
        println!("{}", color_dim("Debug:", use_color));
        println!(
            "  {} {} ({})",
            color_dim("program:", use_color),
            plan.program_id,
            plan.program_file
        );
        println!(
            "  {} {}",
            color_dim("model:", use_color),
            plan.model.as_deref().unwrap_or("<provider default>")
        );
        if let Some(last) = &plan.last_session {
            if let Some(session) = last.session_id.as_deref() {
                println!(
                    "  {} {}",
                    color_dim("last output file:", use_color),
                    output_artifact_rel_path(workspace_root, &plan.program_id, session)
                );
            }
            if last.input_tokens.is_some()
                || last.output_tokens.is_some()
                || last.cached_input_tokens.is_some()
            {
                println!(
                    "  {} in={} out={} cached={}",
                    color_dim("last tokens:", use_color),
                    fmt_token_compact_opt(last.input_tokens),
                    fmt_token_compact_opt(last.output_tokens),
                    fmt_token_compact_opt(last.cached_input_tokens)
                );
            }
            if let Some(err) = last.error_short.as_deref() {
                if !err.trim().is_empty() {
                    println!("  {} {}", color_dim("last error:", use_color), err.trim());
                }
            }
        }
    }
}

fn format_file_count(files_total: usize) -> String {
    format!(
        "{} file{}",
        files_total,
        if files_total == 1 { "" } else { "s" }
    )
}

fn format_program_diff_totals(changed: usize, insertions: usize, deletions: usize) -> String {
    match (changed, insertions, deletions) {
        (0, 0, 0) => "unchanged".to_string(),
        (c, adds, dels) => format!(
            "{} line{} changed, {} added, {} deleted",
            c,
            if c == 1 { "" } else { "s" },
            adds,
            dels
        ),
    }
}

fn format_program_diff_preview(plan: &SharedPlanData) -> (String, String) {
    let diff_file = plan.program_diff_vs_last_session.file.clone();
    let diff_summary = match plan.program_diff_vs_last_session.status.as_str() {
        "first_apply" => "first apply".to_string(),
        "unavailable" => "snapshot unavailable".to_string(),
        "unchanged" => "unchanged".to_string(),
        _ => format_program_diff_totals(
            plan.program_diff_vs_last_session.lines_changed,
            plan.program_diff_vs_last_session.lines_added,
            plan.program_diff_vs_last_session.lines_deleted,
        ),
    };
    (diff_file, diff_summary)
}

fn format_program_variables_diff_totals(changed: usize, added: usize, removed: usize) -> String {
    match (changed, added, removed) {
        (0, 0, 0) => "unchanged".to_string(),
        (c, a, r) => format!(
            "{} value{} changed, {} added, {} removed",
            c,
            if c == 1 { "" } else { "s" },
            a,
            r
        ),
    }
}

fn format_program_variables_diff_preview(plan: &SharedPlanData) -> Option<String> {
    let diff = plan.program_variables_diff_vs_last_session.as_ref()?;
    let summary = match diff.status.as_str() {
        "first_apply" => "first apply".to_string(),
        "unavailable" => "snapshot unavailable".to_string(),
        "unchanged" => "unchanged".to_string(),
        _ => format_program_variables_diff_totals(
            diff.values_changed,
            diff.values_added,
            diff.values_removed,
        ),
    };
    Some(summary)
}

fn fmt_token_compact_opt(value: Option<u64>) -> String {
    value
        .map(format_token_compact)
        .unwrap_or_else(|| "n/a".to_string())
}

fn format_age(ts_unix: u64) -> String {
    let now = now_unix_secs();
    if ts_unix >= now {
        return "just now".to_string();
    }
    let delta = now - ts_unix;
    if delta < 60 {
        return format!("{}s ago", delta);
    }
    if delta < 3_600 {
        return format!("{}m ago", delta / 60);
    }
    if delta < 86_400 {
        return format!("{}h ago", delta / 3_600);
    }
    format!("{}d ago", delta / 86_400)
}

fn output_artifact_rel_path(workspace_root: &Path, program_id: &str, session_id: &str) -> String {
    let abs = program_session_dir(workspace_root, program_id, session_id).join("output.md");
    abs.strip_prefix(workspace_root)
        .map(to_slash_path)
        .unwrap_or_else(|_| to_slash_path(&abs))
}

fn format_msg_link(workspace_root: &Path, rel_path: &str, use_color: bool) -> String {
    let rendered = if !supports_terminal_hyperlinks() {
        "msg".to_string()
    } else {
        let abs = workspace_root.join(rel_path);
        terminal_link(&abs, "msg").unwrap_or_else(|| "msg".to_string())
    };
    color_link_label(&rendered, use_color)
}

fn color_dim(text: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[2m{}\x1b[0m", text)
    } else {
        text.to_string()
    }
}

fn color_success(text: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[32m{}\x1b[0m", text)
    } else {
        text.to_string()
    }
}

fn color_error(text: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[31m{}\x1b[0m", text)
    } else {
        text.to_string()
    }
}

fn color_path(text: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[36m{}\x1b[0m", text)
    } else {
        text.to_string()
    }
}

fn color_link_label(text: &str, use_color: bool) -> String {
    if !use_color {
        return text.to_string();
    }
    if let Some((start, label, end)) = split_terminal_hyperlink(text) {
        return format!("{}\x1b[95m{}\x1b[0m{}", start, label, end);
    }
    format!("\x1b[95m{}\x1b[0m", text)
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

fn supports_terminal_hyperlinks() -> bool {
    if !io::stdout().is_terminal() {
        return false;
    }
    if std::env::var("CLAWFORM_NO_HYPERLINKS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return false;
    }
    match std::env::var("TERM") {
        Ok(term) if term.eq_ignore_ascii_case("dumb") => false,
        _ => true,
    }
}

fn terminal_link(path: &Path, label: &str) -> Option<String> {
    let file_url = format!("file://{}", percent_encode_path(path));
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

fn display_program_file(workspace_root: &Path, program_path: &Path) -> String {
    match program_path.strip_prefix(workspace_root) {
        Ok(rel) => to_slash_path(rel),
        Err(_) => to_slash_path(program_path),
    }
}

fn should_skip_path(rel: &Path) -> bool {
    let rel_str = rel.to_string_lossy().replace('\\', "/");

    rel_str == ".git"
        || rel_str.starts_with(".git/")
        || rel_str == "target"
        || rel_str.starts_with("target/")
        || rel_str == AGENT_OUTPUT_MANIFEST_REL
        || rel_str == AGENT_HUMAN_OUTPUT_REL
        || rel_str == AGENT_RESULT_REL
        || rel_str == RUNTIME_VARIABLES_INPUT_REL
        || rel_str == ".clawform/history"
        || rel_str.starts_with(".clawform/history/")
        || rel_str == ".clawform/programs"
        || rel_str.starts_with(".clawform/programs/")
        || rel_str == ".clawform/sessions"
        || rel_str.starts_with(".clawform/sessions/")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT_EXAMPLE_WITH_LAST: &str =
        include_str!("../../../test_data/runtime_prompt_with_last_session.md");
    const PROMPT_EXAMPLE_NO_LAST: &str =
        include_str!("../../../test_data/runtime_prompt_first_session.md");

    #[test]
    fn normalize_reported_path_rejects_parent_dir_escape() {
        assert_eq!(normalize_reported_rel_path("../x.txt"), None);
    }

    #[test]
    fn normalize_reported_path_accepts_clean_relative() {
        assert_eq!(
            normalize_reported_rel_path("./nested/output.txt"),
            Some("nested/output.txt".to_string())
        );
    }

    #[test]
    fn runtime_prompt_exact_match_without_last_session_fixture() {
        let plan = SharedPlanData {
            program_id: "calculator".to_string(),
            program_file: "examples/calc.md".to_string(),
            model: None,
            program_variables: None,
            program_variables_diff_vs_last_session: None,
            program_diff_vs_last_session: PlanProgramDiff {
                status: "first_apply".to_string(),
                file: "examples/calc.md".to_string(),
                lines_changed: 0,
                lines_added: 0,
                lines_deleted: 0,
            },
            last_session: None,
        };

        let prompt = build_runtime_prompt("# test\n", &plan, SandboxMode::Auto)
            .expect("prompt should build");
        assert_eq!(prompt, PROMPT_EXAMPLE_NO_LAST);
    }

    #[test]
    fn runtime_prompt_exact_match_with_last_session_fixture() {
        let plan = SharedPlanData {
            program_id: "calculator".to_string(),
            program_file: "examples/calc.md".to_string(),
            model: None,
            program_variables: None,
            program_variables_diff_vs_last_session: None,
            program_diff_vs_last_session: PlanProgramDiff {
                status: "changed".to_string(),
                file: "examples/calc.md".to_string(),
                lines_changed: 6,
                lines_added: 0,
                lines_deleted: 24,
            },
            last_session: Some(PlanLastSession {
                session_id: Some("019d55f0-fd15-7041-bca3-979c467b67eb".to_string()),
                ts_unix: 1775263601,
                status: RunStatus::Success,
                model: None,
                summary_short: None,
                files_total: 0,
                files_sample: Vec::new(),
                insertions: 0,
                deletions: 0,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                error_short: None,
            }),
        };

        let prompt = build_runtime_prompt("# test\n", &plan, SandboxMode::Auto)
            .expect("prompt should build");
        assert_eq!(prompt, PROMPT_EXAMPLE_WITH_LAST);
    }

    #[test]
    fn runtime_prompt_includes_variables_block_for_first_session() {
        let plan = SharedPlanData {
            program_id: "calculator".to_string(),
            program_file: "examples/calc.md".to_string(),
            model: None,
            program_variables: Some(PlanProgramVariables {
                values_total: 1,
                current_session_file: RUNTIME_VARIABLES_INPUT_REL.to_string(),
                last_session_file: None,
            }),
            program_variables_diff_vs_last_session: Some(PlanProgramVariablesDiff {
                status: "first_apply".to_string(),
                values_changed: 0,
                values_added: 1,
                values_removed: 0,
            }),
            program_diff_vs_last_session: PlanProgramDiff {
                status: "first_apply".to_string(),
                file: "examples/calc.md".to_string(),
                lines_changed: 0,
                lines_added: 0,
                lines_deleted: 0,
            },
            last_session: None,
        };

        let prompt = build_runtime_prompt("# test\n", &plan, SandboxMode::Auto)
            .expect("prompt should build");
        assert!(prompt.contains("Resolved program variables for this session"));
        assert!(prompt.contains(RUNTIME_VARIABLES_INPUT_REL));
    }

    #[test]
    fn runtime_prompt_includes_last_session_variables_hint_when_available() {
        let plan = SharedPlanData {
            program_id: "calculator".to_string(),
            program_file: "examples/calc.md".to_string(),
            model: None,
            program_variables: Some(PlanProgramVariables {
                values_total: 1,
                current_session_file: RUNTIME_VARIABLES_INPUT_REL.to_string(),
                last_session_file: Some(
                    ".clawform/programs/calculator/sessions/s-1/variables.json".to_string(),
                ),
            }),
            program_variables_diff_vs_last_session: Some(PlanProgramVariablesDiff {
                status: "changed".to_string(),
                values_changed: 1,
                values_added: 0,
                values_removed: 0,
            }),
            program_diff_vs_last_session: PlanProgramDiff {
                status: "changed".to_string(),
                file: "examples/calc.md".to_string(),
                lines_changed: 1,
                lines_added: 1,
                lines_deleted: 0,
            },
            last_session: Some(PlanLastSession {
                session_id: Some("s-1".to_string()),
                ts_unix: 1,
                status: RunStatus::Success,
                model: None,
                summary_short: None,
                files_total: 0,
                files_sample: Vec::new(),
                insertions: 0,
                deletions: 0,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                error_short: None,
            }),
        };

        let prompt = build_runtime_prompt("# test\n", &plan, SandboxMode::Auto)
            .expect("prompt should build");
        assert!(prompt.contains("Resolved program variables for this \"current session\""));
        assert!(prompt.contains("last_session_variables_file"));
        assert!(prompt.contains("variables.json"));
    }

    #[test]
    fn runtime_prompt_unsandboxed_omits_sandbox_guidance_for_first_session() {
        let plan = SharedPlanData {
            program_id: "calculator".to_string(),
            program_file: "examples/calc.md".to_string(),
            model: None,
            program_variables: None,
            program_variables_diff_vs_last_session: None,
            program_diff_vs_last_session: PlanProgramDiff {
                status: "first_apply".to_string(),
                file: "examples/calc.md".to_string(),
                lines_changed: 0,
                lines_added: 0,
                lines_deleted: 0,
            },
            last_session: None,
        };

        let prompt = build_runtime_prompt("# test\n", &plan, SandboxMode::Unsandboxed)
            .expect("prompt should build");
        assert!(!prompt.contains(
            "Verdict gate (required): after the first restriction symptom, stop normal task work and classify the block cause."
        ));
        assert!(!prompt.contains("sandbox limits"));
    }

    #[test]
    fn runtime_prompt_unsandboxed_omits_sandbox_guidance_for_last_session() {
        let plan = SharedPlanData {
            program_id: "calculator".to_string(),
            program_file: "examples/calc.md".to_string(),
            model: None,
            program_variables: None,
            program_variables_diff_vs_last_session: None,
            program_diff_vs_last_session: PlanProgramDiff {
                status: "changed".to_string(),
                file: "examples/calc.md".to_string(),
                lines_changed: 6,
                lines_added: 0,
                lines_deleted: 24,
            },
            last_session: Some(PlanLastSession {
                session_id: Some("019d55f0-fd15-7041-bca3-979c467b67eb".to_string()),
                ts_unix: 1775263601,
                status: RunStatus::Success,
                model: None,
                summary_short: None,
                files_total: 0,
                files_sample: Vec::new(),
                insertions: 0,
                deletions: 0,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                error_short: None,
            }),
        };

        let prompt = build_runtime_prompt("# test\n", &plan, SandboxMode::Unsandboxed)
            .expect("prompt should build");
        assert!(!prompt.contains(
            "Verdict gate (required): after the first restriction symptom, stop normal task work and classify the block cause."
        ));
        assert!(!prompt.contains("sandbox limits"));
    }

    #[test]
    fn runtime_prompt_auto_includes_mixed_evidence_precedence_rule() {
        let plan = SharedPlanData {
            program_id: "calculator".to_string(),
            program_file: "examples/calc.md".to_string(),
            model: None,
            program_variables: None,
            program_variables_diff_vs_last_session: None,
            program_diff_vs_last_session: PlanProgramDiff {
                status: "first_apply".to_string(),
                file: "examples/calc.md".to_string(),
                lines_changed: 0,
                lines_added: 0,
                lines_deleted: 0,
            },
            last_session: None,
        };

        let prompt = build_runtime_prompt("# test\n", &plan, SandboxMode::Auto)
            .expect("prompt should build");
        assert!(prompt.contains(
            "use `sandbox_blocked` if any restriction symptom appears in a failing required command"
        ));
        assert!(prompt.contains(
            "Use `reason: program_blocked` only when zero restriction symptoms appeared in failing required commands"
        ));
    }

    #[test]
    fn parses_agent_result_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(AGENT_RESULT_REL);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            r#"{"status":"partial","reason":"program_blocked","message":"could not run integration tests"}"#,
        )?;

        let result = read_agent_result(dir.path())?;
        assert_eq!(
            result,
            Some(AgentResult {
                status: AgentStatus::Partial,
                reason: Some(AgentReason::ProgramBlocked),
                message: Some("could not run integration tests".to_string()),
            })
        );
        Ok(())
    }

    #[test]
    fn parses_agent_result_reason_keyword() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(AGENT_RESULT_REL);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            r#"{"status":"failure","reason":"sandbox_blocked","message":"curl failed"}"#,
        )?;

        let result = read_agent_result(dir.path())?;
        assert_eq!(
            result,
            Some(AgentResult {
                status: AgentStatus::Failure,
                reason: Some(AgentReason::SandboxBlocked),
                message: Some("curl failed".to_string()),
            })
        );
        Ok(())
    }

    #[test]
    fn parses_agent_result_with_program_blocked_reason() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(AGENT_RESULT_REL);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            r#"{"status":"failure","reason":"program_blocked","message":"server unreachable"}"#,
        )?;

        let result = read_agent_result(dir.path())?;
        assert_eq!(
            result,
            Some(AgentResult {
                status: AgentStatus::Failure,
                reason: Some(AgentReason::ProgramBlocked),
                message: Some("server unreachable".to_string()),
            })
        );
        Ok(())
    }

    #[test]
    fn rejects_agent_result_with_unknown_reason() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(AGENT_RESULT_REL);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            r#"{"status":"failure","reason":"server_unreachable","message":"permission denied and connection failed"}"#,
        )?;

        let err = read_agent_result(dir.path()).expect_err("must reject unknown reason");
        assert!(format!("{:#}", err).contains("invalid JSON in agent result"));
        Ok(())
    }

    #[test]
    fn rejects_agent_result_with_legacy_reason_value() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(AGENT_RESULT_REL);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            r#"{"status":"failure","reason":"service_unreachable","message":"legacy reason"}"#,
        )?;

        let err = read_agent_result(dir.path()).expect_err("must reject legacy reason");
        assert!(format!("{:#}", err).contains("invalid JSON in agent result"));
        Ok(())
    }

    #[test]
    fn parses_agent_output_manifest_object_records() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(AGENT_OUTPUT_MANIFEST_REL);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            r#"[
  {"path":"out.txt","change":"modified"},
  {"path":"./nested/new.txt","change":"created"},
  {"path":".clawform/agent_result.json","change":"modified"}
]"#,
        )?;

        let files = read_agent_reported_files(dir.path())?;
        assert_eq!(
            files,
            vec!["nested/new.txt".to_string(), "out.txt".to_string()]
        );
        Ok(())
    }

    #[test]
    fn program_diff_replacement_only_is_not_rendered_as_unchanged() {
        assert_eq!(
            format_program_diff_totals(1, 0, 0),
            "1 line changed, 0 added, 0 deleted"
        );
    }

    #[test]
    fn program_diff_mixed_counts_are_rendered_compactly() {
        assert_eq!(
            format_program_diff_totals(2, 3, 1),
            "2 lines changed, 3 added, 1 deleted"
        );
    }

    #[test]
    fn program_variables_diff_mixed_counts_are_rendered_compactly() {
        assert_eq!(
            format_program_variables_diff_totals(1, 2, 3),
            "1 value changed, 2 added, 3 removed"
        );
    }

    #[test]
    fn color_link_label_colors_plain_msg_label() {
        assert_eq!(color_link_label("msg", true), "\x1b[95mmsg\x1b[0m");
    }

    #[test]
    fn color_link_label_colors_hyperlink_msg_label_only() {
        let raw = "\x1b]8;;file:///tmp/output.md\x1b\\msg\x1b]8;;\x1b\\";
        let rendered = color_link_label(raw, true);
        assert_eq!(
            rendered,
            "\x1b]8;;file:///tmp/output.md\x1b\\\x1b[95mmsg\x1b[0m\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn computes_program_variables_diff_changed_value() -> Result<()> {
        let ws = tempfile::tempdir()?;
        let program_id = "smoke";
        let session_id = "s-1";
        let snapshot = program_variables_snapshot_path(ws.path(), program_id, session_id);
        if let Some(parent) = snapshot.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &snapshot,
            serde_json::to_string_pretty(&BTreeMap::from([(
                "SMOKE_VALUE".to_string(),
                "SMOKE_OK".to_string(),
            )]))?
                + "\n",
        )?;

        let last_session = RunHistoryRecord {
            ts_unix: 1,
            program_id: program_id.to_string(),
            session_id: Some(session_id.to_string()),
            status: RunStatus::Success,
            model: None,
            summary_short: None,
            files_total: 0,
            insertions: 0,
            deletions: 0,
            files_sample: Vec::new(),
            error_short: None,
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
        };

        let diff = compute_program_variables_diff_vs_last_session(
            ws.path(),
            program_id,
            &BTreeMap::from([("SMOKE_VALUE".to_string(), "YU".to_string())]),
            Some(&last_session),
        )
        .expect("diff should be present");
        assert_eq!(diff.status, "changed");
        assert_eq!(diff.values_changed, 1);
        assert_eq!(diff.values_added, 0);
        assert_eq!(diff.values_removed, 0);
        Ok(())
    }

    #[test]
    fn validation_rejects_missing_agent_result() {
        let err = validate_agent_completion(&None).expect_err("must fail");
        assert!(format!("{:#}", err).contains(AGENT_RESULT_REL));
    }

    #[test]
    fn validation_rejects_partial_agent_result() {
        let result = AgentResult {
            status: AgentStatus::Partial,
            reason: Some(AgentReason::ProgramBlocked),
            message: Some("network was unavailable".to_string()),
        };
        let err = validate_agent_completion(&Some(result)).expect_err("partial must fail");
        assert!(format!("{:#}", err).contains("partial completion"));
    }

    #[test]
    fn validation_rejects_failure_agent_result() {
        let result = AgentResult {
            status: AgentStatus::Failure,
            reason: Some(AgentReason::ProgramBlocked),
            message: Some("blocked".to_string()),
        };
        let err = validate_agent_completion(&Some(result)).expect_err("failure must fail");
        assert!(format!("{:#}", err).contains("reported failure"));
    }

    #[test]
    fn validation_failure_error_includes_reason_when_present() {
        let result = AgentResult {
            status: AgentStatus::Failure,
            reason: Some(AgentReason::ProgramBlocked),
            message: Some("failed to connect".to_string()),
        };
        let err = validate_agent_completion(&Some(result)).expect_err("failure must fail");
        let text = format!("{:#}", err);
        assert!(text.contains("reason=program_blocked"));
        assert!(text.contains("failed to connect"));
    }

    #[test]
    fn validation_accepts_minimal_agent_result() {
        let result = AgentResult {
            status: AgentStatus::Success,
            reason: None,
            message: Some("done".to_string()),
        };
        validate_agent_completion(&Some(result)).expect("minimal result should pass");
    }

    #[test]
    fn validation_rejects_failure_without_reason() {
        let result = AgentResult {
            status: AgentStatus::Failure,
            reason: None,
            message: Some("failed".to_string()),
        };
        let err = validate_agent_completion(&Some(result)).expect_err("must fail");
        assert!(format!("{:#}", err).contains("requires reason"));
    }

    #[test]
    fn validation_failure_with_reason_reports_failure() {
        let result = AgentResult {
            status: AgentStatus::Failure,
            reason: Some(AgentReason::ProgramBlocked),
            message: Some("failed".to_string()),
        };
        let err = validate_agent_completion(&Some(result)).expect_err("failure must fail");
        assert!(format!("{:#}", err).contains("reported failure"));
    }

    #[test]
    fn validation_rejects_success_with_reason() {
        let result = AgentResult {
            status: AgentStatus::Success,
            reason: Some(AgentReason::ProgramBlocked),
            message: Some("done".to_string()),
        };
        let err = validate_agent_completion(&Some(result)).expect_err("must fail");
        assert!(format!("{:#}", err).contains("must omit reason"));
    }
}
