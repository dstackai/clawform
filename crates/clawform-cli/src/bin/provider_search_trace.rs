use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use serde_json::{json, Value};

use clawform_core::program::load_program;
use clawform_core::{
    load_config, resolve_provider_runner, ProviderKind, ProviderRequest, SandboxMode,
};

#[derive(Debug, Parser)]
#[command(
    about = "Run a provider probe that tries to force built-in search usage and trace the resulting events."
)]
struct Cli {
    /// markdown program file to use as the probe instructions
    #[arg(short = 'f', long = "file", default_value = "program.md")]
    file: PathBuf,

    /// provider name from .clawform/config.json
    #[arg(short = 'p', long = "provider")]
    provider: Option<String>,

    /// sandbox mode to use for the provider run
    #[arg(short = 's', long = "sandbox", value_enum, default_value_t = CliSandboxMode::Workspace)]
    sandbox: CliSandboxMode,

    /// directory where raw traces and the summary report will be written
    #[arg(long = "save-dir")]
    save_dir: Option<PathBuf>,

    /// print the final summary as JSON
    #[arg(long = "json")]
    json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliSandboxMode {
    Auto,
    Workspace,
    FullAccess,
}

impl From<CliSandboxMode> for SandboxMode {
    fn from(value: CliSandboxMode) -> Self {
        match value {
            CliSandboxMode::Auto => SandboxMode::Auto,
            CliSandboxMode::Workspace => SandboxMode::Sandboxed,
            CliSandboxMode::FullAccess => SandboxMode::Unsandboxed,
        }
    }
}

#[derive(Debug, Clone)]
struct TraceEvent {
    source: String,
    item_id: Option<String>,
    item_type: Option<String>,
    name: Option<String>,
    summary: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct OutputTrace {
    session_id: Option<String>,
    advertised_tools: Vec<String>,
    tool_events: Vec<TraceEvent>,
    search_events: Vec<TraceEvent>,
    usage_search_requests: Option<u64>,
    usage_fetch_requests: Option<u64>,
    json_lines: usize,
    unparsed_lines: usize,
}

fn main() {
    if let Err(err) = real_main() {
        eprintln!("error: {:#}", err);
        process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let workspace_root =
        env::current_dir().context("failed resolving current working directory")?;
    let config = load_config(&workspace_root)?;
    let resolved_provider = config.resolve_provider(cli.provider.as_deref())?;
    let runner = resolve_provider_runner(resolved_provider.provider_type)?;
    let program = load_program(&cli.file)?;
    let program_id = program.program_key()?;
    let model = program.resolved_model(resolved_provider.default_model.as_deref());
    let sandbox_mode: SandboxMode = cli.sandbox.into();

    let save_dir = cli.save_dir.unwrap_or_else(|| {
        default_save_dir(
            &workspace_root,
            resolved_provider.name.as_str(),
            program_id.as_str(),
        )
    });
    fs::create_dir_all(&save_dir).with_context(|| {
        format!(
            "failed creating probe output directory '{}'",
            save_dir.display()
        )
    })?;

    let prompt = build_probe_prompt(program.raw_markdown.as_str());
    let prompt_path = save_dir.join("prompt.txt");
    let program_snapshot_path = save_dir.join("program.md");
    fs::write(&prompt_path, prompt.as_bytes())
        .with_context(|| format!("failed writing prompt '{}'", prompt_path.display()))?;
    fs::write(&program_snapshot_path, program.raw_markdown.as_bytes()).with_context(|| {
        format!(
            "failed writing program snapshot '{}'",
            program_snapshot_path.display()
        )
    })?;

    let stdout_path = save_dir.join("provider.stdout");
    let stderr_path = save_dir.join("provider.stderr");
    let summary_path = save_dir.join("trace_summary.json");

    let request = ProviderRequest {
        workspace_root: workspace_root.clone(),
        artifacts_root: Some(save_dir.clone()),
        program_id: Some(program_id.clone()),
        model: model.clone(),
        agent_result_rel: String::new(),
        sandbox_mode,
        prompt,
        progress: true,
        render_progress: false,
        debug_mode: false,
        verbose_output: false,
        verbose_events: true,
        interactive_ui: false,
        show_intermediate_steps: true,
    };

    let capabilities = runner.capabilities();
    let run = match runner.run(&request) {
        Ok(run) => run,
        Err(err) => {
            let summary = json!({
                "workspace_root": workspace_root.display().to_string(),
                "program_file": cli.file.display().to_string(),
                "program_id": program_id,
                "provider_name": resolved_provider.name,
                "provider_type": resolved_provider.provider_type.as_str(),
                "model": model,
                "sandbox_mode": sandbox_mode.label(),
                "save_dir": save_dir.display().to_string(),
                "prompt_file": prompt_path.display().to_string(),
                "program_snapshot_file": program_snapshot_path.display().to_string(),
                "stdout_file": stdout_path.display().to_string(),
                "stderr_file": stderr_path.display().to_string(),
                "summary_file": summary_path.display().to_string(),
                "runner_capabilities": capabilities_to_json(&capabilities),
                "run_error": format!("{:#}", err),
            });
            fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)
                .with_context(|| format!("failed writing summary '{}'", summary_path.display()))?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                print_human_summary(&summary);
            }

            return Err(err);
        }
    };

    fs::write(&stdout_path, run.stdout.as_bytes())
        .with_context(|| format!("failed writing stdout '{}'", stdout_path.display()))?;
    fs::write(&stderr_path, run.stderr.as_bytes())
        .with_context(|| format!("failed writing stderr '{}'", stderr_path.display()))?;

    let trace = trace_provider_output(resolved_provider.provider_type, run.stdout.as_str());
    let summary = json!({
        "workspace_root": workspace_root.display().to_string(),
        "program_file": cli.file.display().to_string(),
        "program_id": program_id,
        "provider_name": resolved_provider.name,
        "provider_type": resolved_provider.provider_type.as_str(),
        "model": model,
        "sandbox_mode": sandbox_mode.label(),
        "save_dir": save_dir.display().to_string(),
        "prompt_file": prompt_path.display().to_string(),
        "program_snapshot_file": program_snapshot_path.display().to_string(),
        "stdout_file": stdout_path.display().to_string(),
        "stderr_file": stderr_path.display().to_string(),
        "summary_file": summary_path.display().to_string(),
        "runner_capabilities": capabilities_to_json(&capabilities),
        "session_id": run.session_id,
        "trace_session_id": trace.session_id,
        "provider_exit_code": run.exit_code,
        "provider_turn_count": run.turn_count,
        "provider_usage": {
            "input_tokens": run.usage.input_tokens,
            "output_tokens": run.usage.output_tokens,
            "cached_input_tokens": run.usage.cached_input_tokens,
        },
        "json_lines": trace.json_lines,
        "unparsed_lines": trace.unparsed_lines,
        "advertised_tools": trace.advertised_tools,
        "tool_event_count": trace.tool_events.len(),
        "search_event_count": trace.search_events.len(),
        "search_used": !trace.search_events.is_empty(),
        "usage_search_requests": trace.usage_search_requests,
        "usage_fetch_requests": trace.usage_fetch_requests,
        "tool_events": trace_events_to_json(&trace.tool_events),
        "search_events": trace_events_to_json(&trace.search_events),
    });

    fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)
        .with_context(|| format!("failed writing summary '{}'", summary_path.display()))?;

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_human_summary(&summary);
    }

    Ok(())
}

fn build_probe_prompt(program_markdown: &str) -> String {
    let mut out = String::new();
    out.push_str("Provider capability probe\n\n");
    out.push_str("Follow the markdown program below exactly.\n");
    out.push_str(
        "This probe succeeds only if you attempt a built-in web or search tool at least once.\n",
    );
    out.push_str("Do not substitute Bash, curl, wget, Python, or any other direct network workaround for the built-in web or search tool.\n");
    out.push_str("If the built-in web or search tool is unavailable, say so explicitly in the final answer.\n");
    out.push_str("Keep the final answer concise.\n\n");
    out.push_str("Program markdown:\n\n");
    out.push_str(program_markdown);
    if !program_markdown.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn default_save_dir(workspace_root: &Path, provider_name: &str, program_id: &str) -> PathBuf {
    workspace_root
        .join(".clawform")
        .join("provider_search_trace")
        .join(format!(
            "{}-{}-{}",
            now_unix_secs(),
            sanitize_segment(provider_name),
            sanitize_segment(program_id)
        ))
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sanitize_segment(raw: &str) -> String {
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
        "value".to_string()
    } else {
        trimmed.to_string()
    }
}

fn trace_provider_output(provider_type: ProviderKind, raw_stdout: &str) -> OutputTrace {
    match provider_type {
        ProviderKind::Claude => trace_claude_output(raw_stdout),
        ProviderKind::Codex => trace_codex_output(raw_stdout),
    }
}

fn trace_claude_output(raw_stdout: &str) -> OutputTrace {
    let mut out = OutputTrace::default();

    for line in raw_stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                out.unparsed_lines += 1;
                continue;
            }
        };
        out.json_lines += 1;

        match value.get("type").and_then(Value::as_str) {
            Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
                out.session_id = value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                if let Some(tools) = value.get("tools").and_then(Value::as_array) {
                    out.advertised_tools = tools
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect();
                }
            }
            Some("assistant") => {
                let Some(contents) = value
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_array)
                else {
                    continue;
                };

                for content in contents {
                    if content.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let name = content
                        .get("name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let item_id = content
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let summary = name
                        .as_deref()
                        .and_then(|tool_name| {
                            content
                                .get("input")
                                .and_then(|input| summarize_named_tool(tool_name, input))
                        })
                        .or_else(|| name.clone());

                    let event = TraceEvent {
                        source: "assistant.tool_use".to_string(),
                        item_id,
                        item_type: Some("tool_use".to_string()),
                        name: name.clone(),
                        summary,
                    };

                    out.tool_events.push(event.clone());
                    if name.as_deref().map(is_search_like_name).unwrap_or(false) {
                        out.search_events.push(event);
                    }
                }
            }
            Some("result") => {
                if out.session_id.is_none() {
                    out.session_id = value
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
                let direct_search_requests = value
                    .get("usage")
                    .and_then(|usage| usage.get("server_tool_use"))
                    .and_then(|server_tool_use| server_tool_use.get("web_search_requests"))
                    .and_then(Value::as_u64);
                let direct_fetch_requests = value
                    .get("usage")
                    .and_then(|usage| usage.get("server_tool_use"))
                    .and_then(|server_tool_use| server_tool_use.get("web_fetch_requests"))
                    .and_then(Value::as_u64);

                let model_usage_search_requests =
                    sum_model_usage_counter(&value, "webSearchRequests");
                let model_usage_fetch_requests =
                    sum_model_usage_counter(&value, "webFetchRequests");

                out.usage_search_requests = direct_search_requests
                    .filter(|count| *count > 0)
                    .or(model_usage_search_requests);
                out.usage_fetch_requests = direct_fetch_requests
                    .filter(|count| *count > 0)
                    .or(model_usage_fetch_requests)
                    .or(direct_fetch_requests);
            }
            _ => {}
        }
    }

    out
}

fn trace_codex_output(raw_stdout: &str) -> OutputTrace {
    let mut out = OutputTrace::default();

    for line in raw_stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                out.unparsed_lines += 1;
                continue;
            }
        };
        out.json_lines += 1;

        match value.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                out.session_id = value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
            Some("item.started") | Some("item.updated") | Some("item.completed") => {
                let item = value.get("item").unwrap_or(&Value::Null);
                let normalized_item_type = normalize_codex_item_type(item);
                let name = item
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("name").and_then(Value::as_str))
                    .map(ToOwned::to_owned);
                let tool_like = is_codex_tool_item(Some(normalized_item_type.as_str()), item);
                if !tool_like {
                    continue;
                }

                let event = TraceEvent {
                    source: value
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("item")
                        .to_string(),
                    item_id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    item_type: Some(normalized_item_type.clone()),
                    name: name.clone(),
                    summary: summarize_codex_item(item, normalized_item_type.as_str()),
                };

                out.tool_events.push(event.clone());
                if is_search_like_item(Some(normalized_item_type.as_str()), name.as_deref(), item) {
                    out.search_events.push(event);
                }
            }
            _ => {}
        }
    }

    out
}

fn is_codex_tool_item(item_type: Option<&str>, item: &Value) -> bool {
    if matches!(
        item_type,
        Some(
            "command_execution"
                | "file_change"
                | "tool_selection"
                | "web_search"
                | "web_fetch"
                | "mcp_tool_call"
                | "todo_list"
        )
    ) {
        return true;
    }

    if normalized_tool_name_for_item(item).is_some() {
        return true;
    }

    item.get("command").is_some() || item.get("path").is_some()
}

fn is_search_like_item(item_type: Option<&str>, name: Option<&str>, item: &Value) -> bool {
    if matches!(
        item_type,
        Some("tool_selection" | "web_search" | "web_fetch")
    ) {
        return true;
    }
    if item_type.map(is_search_like_name).unwrap_or(false) {
        return true;
    }
    if name.map(is_search_like_name).unwrap_or(false) {
        return true;
    }

    extract_tool_string(item, &["query", "url"]).is_some()
}

fn is_search_like_name(raw: &str) -> bool {
    let normalized = canonical_tool_name(raw);
    normalized == "toolsearch"
        || normalized == "websearch"
        || normalized == "webfetch"
        || normalized == "search"
        || normalized.contains("search")
}

fn summarize_codex_item(item: &Value, item_type: &str) -> Option<String> {
    match item_type {
        "tool_selection" | "web_search" | "web_fetch" | "todo_list" => {
            normalized_tool_name_for_item(item)
                .and_then(|tool_name| summarize_named_tool(tool_name, item))
                .or_else(|| extract_probe_subject(item))
        }
        "mcp_tool_call" | "command_execution" | "file_change" => {
            normalized_tool_name_for_item(item)
                .and_then(|tool_name| summarize_named_tool(tool_name, item))
                .or_else(|| extract_probe_subject(item))
        }
        _ => extract_probe_subject(item)
            .or_else(|| normalized_tool_name_for_item(item).map(|name| name.trim().to_string())),
    }
}

fn summarize_named_tool(name: &str, value: &Value) -> Option<String> {
    match canonical_tool_name(name).as_str() {
        "bash" => extract_tool_string(value, &["command"])
            .map(|command| truncate_one_line(command.as_str()))
            .or_else(|| {
                extract_tool_string(value, &["description"])
                    .map(|text| truncate_one_line(text.as_str()))
            }),
        "read" => extract_tool_string(value, &["file_path", "path"])
            .map(|path| format!("read {}", normalize_path_text(path.as_str()))),
        "write" => extract_tool_string(value, &["file_path", "path"])
            .map(|path| format!("write {}", normalize_path_text(path.as_str()))),
        "edit" | "notebookedit" => {
            extract_tool_string(value, &["file_path", "notebook_path", "path"])
                .map(|path| format!("edit {}", normalize_path_text(path.as_str())))
        }
        "glob" => extract_tool_string(value, &["pattern"])
            .map(|pattern| format!("glob {}", truncate_one_line(pattern.as_str()))),
        "grep" => extract_tool_string(value, &["pattern"])
            .map(|pattern| format!("grep {}", truncate_one_line(pattern.as_str()))),
        "todowrite" => summarize_todo_list(value),
        "toolsearch" => extract_tool_string(value, &["query"])
            .as_deref()
            .and_then(summarize_tool_selection_query),
        "websearch" => extract_tool_string(value, &["query", "url"])
            .map(|text| truncate_one_line(text.as_str())),
        "webfetch" => extract_tool_string(value, &["url", "query"])
            .map(|text| truncate_one_line(text.as_str())),
        _ => {
            let tool_name = name.trim();
            match extract_probe_subject(value) {
                Some(subject) => Some(format!("{}: {}", tool_name, subject)),
                None => Some(tool_name.to_string()),
            }
        }
    }
}

fn summarize_todo_list(value: &Value) -> Option<String> {
    match value {
        Value::Array(entries) => summarize_todo_entries(entries),
        Value::String(raw) => summarize_todo_text(raw),
        Value::Object(_) => {
            for key in ["todos", "items", "entries", "tasks", "list"] {
                if let Some(summary) = value.get(key).and_then(summarize_todo_list) {
                    return Some(summary);
                }
            }
            for key in ["input", "args", "arguments", "action"] {
                if let Some(summary) = value.get(key).and_then(summarize_todo_list) {
                    return Some(summary);
                }
            }
            extract_tool_string(
                value,
                &[
                    "content",
                    "text",
                    "title",
                    "task",
                    "description",
                    "label",
                    "name",
                    "prompt",
                ],
            )
            .map(|text| truncate_one_line(text.as_str()))
        }
        _ => None,
    }
}

fn summarize_todo_entries(entries: &[Value]) -> Option<String> {
    let labels: Vec<String> = entries.iter().filter_map(summarize_todo_entry).collect();
    let first = labels.first()?;
    if labels.len() == 1 {
        return Some(first.clone());
    }
    Some(format!("{} (+{} more)", first, labels.len() - 1))
}

fn summarize_todo_entry(value: &Value) -> Option<String> {
    match value {
        Value::String(raw) => summarize_todo_text(raw),
        Value::Object(_) => {
            for key in ["todo", "item", "entry", "task"] {
                if let Some(summary) = value.get(key).and_then(summarize_todo_entry) {
                    return Some(summary);
                }
            }
            extract_tool_string(
                value,
                &[
                    "content",
                    "text",
                    "title",
                    "task",
                    "description",
                    "label",
                    "name",
                    "prompt",
                ],
            )
            .map(|text| truncate_one_line(text.as_str()))
        }
        _ => None,
    }
}

fn summarize_todo_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_one_line(trimmed))
}

fn summarize_tool_selection_query(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed
        .strip_prefix("select:")
        .or_else(|| trimmed.strip_prefix("SELECT:"))
    {
        let items: Vec<&str> = rest
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .collect();
        if !items.is_empty() {
            return Some(truncate_one_line(&items.join(", ")));
        }
    }
    Some(truncate_one_line(trimmed))
}

fn extract_probe_subject(value: &Value) -> Option<String> {
    if let Some(query) = extract_tool_string(value, &["query"]) {
        return Some(truncate_one_line(query.as_str()));
    }
    if let Some(url) = extract_tool_string(value, &["url"]) {
        return Some(truncate_one_line(url.as_str()));
    }
    if let Some(path) = extract_tool_string(value, &["path", "file_path", "notebook_path"]) {
        return Some(normalize_path_text(path.as_str()));
    }
    if let Some(pattern) = extract_tool_string(value, &["pattern"]) {
        return Some(truncate_one_line(pattern.as_str()));
    }
    if let Some(text) = extract_tool_string(
        value,
        &[
            "text",
            "prompt",
            "command",
            "question",
            "description",
            "message",
            "title",
            "content",
            "task",
        ],
    ) {
        return Some(truncate_one_line(text.as_str()));
    }
    None
}

fn extract_tool_string(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    for parent in ["input", "args", "arguments", "action"] {
        if let Some(nested) = value.get(parent) {
            if let Some(text) = extract_tool_string_from_nested(nested, keys) {
                return Some(text);
            }
        }
    }
    None
}

fn extract_tool_string_from_nested(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(_) => extract_tool_string(value, keys),
        Value::String(raw) => {
            let trimmed = raw.trim();
            if !matches!(trimmed.as_bytes().first(), Some(b'{') | Some(b'[')) {
                return None;
            }
            let parsed = serde_json::from_str::<Value>(trimmed).ok()?;
            extract_tool_string(&parsed, keys)
        }
        _ => None,
    }
}

fn normalize_path_text(raw: &str) -> String {
    Path::new(raw.trim()).to_string_lossy().replace('\\', "/")
}

fn normalize_codex_item_type(item: &Value) -> String {
    let raw_item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if matches!(
        raw_item_type,
        "command_execution"
            | "file_change"
            | "tool_selection"
            | "web_search"
            | "web_fetch"
            | "mcp_tool_call"
            | "todo_list"
    ) {
        return raw_item_type.to_string();
    }

    let raw_named_type = named_tool_item_type(raw_item_type);
    if raw_named_type != "mcp_tool_call" {
        return raw_named_type.to_string();
    }

    if let Some(tool_name) = normalized_tool_name_for_item(item) {
        let normalized = named_tool_item_type(tool_name);
        if normalized != "mcp_tool_call" {
            return normalized.to_string();
        }
    }

    let raw = canonical_tool_name(raw_item_type);
    if raw == "search" {
        return "web_search".to_string();
    }
    if raw == "fetch" {
        return "web_fetch".to_string();
    }

    if raw_item_type.contains("tool") || raw_item_type.contains("mcp") {
        return "mcp_tool_call".to_string();
    }

    if normalized_tool_name_for_item(item).is_some() {
        return "mcp_tool_call".to_string();
    }

    raw_item_type.to_string()
}

fn normalized_tool_name_for_item<'a>(item: &'a Value) -> Option<&'a str> {
    item.get("tool_name")
        .and_then(Value::as_str)
        .or_else(|| item.get("name").and_then(Value::as_str))
        .or_else(|| {
            let raw_item_type = item.get("type").and_then(Value::as_str)?;
            if named_tool_item_type(raw_item_type) != "mcp_tool_call" {
                Some(raw_item_type)
            } else {
                None
            }
        })
}

fn named_tool_item_type(name: &str) -> &'static str {
    match canonical_tool_name(name).as_str() {
        "bash" | "read" | "glob" | "grep" => "command_execution",
        "write" | "edit" | "notebookedit" => "file_change",
        "todowrite" => "todo_list",
        "toolsearch" => "tool_selection",
        "websearch" => "web_search",
        "webfetch" => "web_fetch",
        _ => "mcp_tool_call",
    }
}

fn canonical_tool_name(name: &str) -> String {
    name.trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn sum_model_usage_counter(value: &Value, field: &str) -> Option<u64> {
    let model_usage = value.get("modelUsage")?.as_object()?;
    let total = model_usage
        .values()
        .filter_map(|entry| entry.get(field))
        .filter_map(Value::as_u64)
        .sum::<u64>();

    Some(total)
}

fn truncate_one_line(text: &str) -> String {
    let one_line = text.replace('\n', " ").replace('\r', " ");
    let trimmed = one_line.trim();
    const MAX: usize = 120;
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }

    let mut out = String::new();
    for ch in trimmed.chars().take(MAX.saturating_sub(3)) {
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn capabilities_to_json(capabilities: &clawform_core::ProviderCapabilities) -> Value {
    json!({
        "live_events": capabilities.live_events,
        "partial_text": capabilities.partial_text,
        "tool_call_events": capabilities.tool_call_events,
        "file_change_events": capabilities.file_change_events,
        "resume": capabilities.resume,
        "cancel": capabilities.cancel,
        "approvals": capabilities.approvals,
    })
}

fn trace_events_to_json(events: &[TraceEvent]) -> Value {
    Value::Array(
        events
            .iter()
            .map(|event| {
                json!({
                    "source": event.source,
                    "item_id": event.item_id,
                    "item_type": event.item_type,
                    "name": event.name,
                    "summary": event.summary,
                })
            })
            .collect(),
    )
}

fn print_human_summary(summary: &Value) {
    let provider_name = summary
        .get("provider_name")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let provider_type = summary
        .get("provider_type")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let model = summary
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("<provider-default>");
    let save_dir = summary
        .get("save_dir")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let exit_code = summary
        .get("provider_exit_code")
        .and_then(Value::as_i64)
        .map(|v| v.to_string())
        .unwrap_or_else(|| "<none>".to_string());
    let search_used = summary
        .get("search_used")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let search_count = summary
        .get("search_event_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tool_count = summary
        .get("tool_event_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let usage_search_requests = summary.get("usage_search_requests").and_then(Value::as_u64);
    let usage_fetch_requests = summary.get("usage_fetch_requests").and_then(Value::as_u64);

    println!("provider: {} ({})", provider_name, provider_type);
    println!("model: {}", model);
    println!("save_dir: {}", save_dir);
    if let Some(run_error) = summary.get("run_error").and_then(Value::as_str) {
        println!("run_error: {}", run_error);
        return;
    }
    println!("provider_exit_code: {}", exit_code);
    println!("tool_events: {}", tool_count);
    println!("search_events: {}", search_count);
    println!("search_used: {}", if search_used { "yes" } else { "no" });
    if let Some(count) = usage_search_requests {
        println!("usage_search_requests: {}", count);
    }
    if let Some(count) = usage_fetch_requests {
        println!("usage_fetch_requests: {}", count);
    }

    if let Some(advertised_tools) = summary.get("advertised_tools").and_then(Value::as_array) {
        if !advertised_tools.is_empty() {
            let tools = advertised_tools
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            println!("advertised_tools: {}", tools);
        }
    }

    if let Some(search_events) = summary.get("search_events").and_then(Value::as_array) {
        for event in search_events.iter().take(5) {
            let source = event
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("event");
            let name = event
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<none>");
            let summary = event
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("<no summary>");
            println!("search_event: {} | {} | {}", source, name, summary);
        }
    }

    if let Some(summary_file) = summary.get("summary_file").and_then(Value::as_str) {
        println!("summary_file: {}", summary_file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traces_claude_search_tool_use_and_usage_counters() {
        let raw = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-1\",\"tools\":[\"Bash\",\"WebSearch\",\"Write\"]}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"WebSearch\",\"input\":{\"query\":\"Example Domain example.com\"}}]}}\n",
            "{\"type\":\"result\",\"session_id\":\"sess-1\",\"usage\":{\"server_tool_use\":{\"web_search_requests\":1,\"web_fetch_requests\":0}}}\n"
        );

        let trace = trace_claude_output(raw);
        assert_eq!(trace.session_id.as_deref(), Some("sess-1"));
        assert_eq!(trace.advertised_tools, vec!["Bash", "WebSearch", "Write"]);
        assert_eq!(trace.tool_events.len(), 1);
        assert_eq!(trace.search_events.len(), 1);
        assert_eq!(trace.usage_search_requests, Some(1));
        assert_eq!(trace.usage_fetch_requests, Some(0));
        assert_eq!(
            trace.search_events[0].summary.as_deref(),
            Some("Example Domain example.com")
        );
    }

    #[test]
    fn falls_back_to_claude_model_usage_search_counter() {
        let raw = concat!(
            "{\"type\":\"result\",\"session_id\":\"sess-2\",\"usage\":{\"server_tool_use\":{\"web_search_requests\":0,\"web_fetch_requests\":0}},\"modelUsage\":{\"claude-haiku\":{\"webSearchRequests\":1}}}\n"
        );

        let trace = trace_claude_output(raw);
        assert_eq!(trace.usage_search_requests, Some(1));
        assert_eq!(trace.usage_fetch_requests, Some(0));
    }

    #[test]
    fn traces_codex_web_search_item() {
        let raw = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}\n",
            "{\"type\":\"item.started\",\"item\":{\"id\":\"item_4\",\"type\":\"web_search\",\"query\":\"Example Domain example.com\"}}\n"
        );

        let trace = trace_codex_output(raw);
        assert_eq!(trace.session_id.as_deref(), Some("thread-1"));
        assert_eq!(trace.tool_events.len(), 1);
        assert_eq!(trace.search_events.len(), 1);
        assert_eq!(
            trace.search_events[0].summary.as_deref(),
            Some("Example Domain example.com")
        );
    }

    #[test]
    fn traces_codex_write_tool_as_file_change() {
        let raw = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-2\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_5\",\"type\":\"tool_call\",\"name\":\"Write\",\"input\":{\"file_path\":\"src/main.rs\"}}}\n"
        );

        let trace = trace_codex_output(raw);
        assert_eq!(trace.tool_events.len(), 1);
        assert_eq!(
            trace.tool_events[0].item_type.as_deref(),
            Some("file_change")
        );
        assert_eq!(
            trace.tool_events[0].summary.as_deref(),
            Some("write src/main.rs")
        );
    }

    #[test]
    fn does_not_misclassify_plain_codex_message_as_tool_use() {
        let raw = "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_9\",\"type\":\"assistant_message\",\"text\":\"done\"}}\n";
        let trace = trace_codex_output(raw);
        assert!(trace.tool_events.is_empty());
        assert!(trace.search_events.is_empty());
    }

    #[test]
    fn detects_search_like_names_case_insensitively() {
        assert!(is_search_like_name("WebSearch"));
        assert!(is_search_like_name("toolsearch"));
        assert!(is_search_like_name("web_search"));
        assert!(!is_search_like_name("Write"));
    }
}
