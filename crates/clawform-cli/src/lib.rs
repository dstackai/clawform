use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{ArgAction, CommandFactory, Parser, Subcommand, ValueEnum};

use clawform_core::{
    reset_history, run_apply, AgentResult, AgentStatus, ApplyRequest, CodexRunner, FileResult,
    HistoryResetTarget, ProviderRunner, ProviderUsage, SandboxMode,
};

const MAX_REPORTED_FILES_DISPLAY: usize = 20;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliSandboxMode {
    Auto,
    #[value(alias = "sandboxed")]
    WorkspaceWrite,
    #[value(alias = "unsandboxed")]
    DangerFullAccess,
}

impl From<CliSandboxMode> for SandboxMode {
    fn from(value: CliSandboxMode) -> Self {
        match value {
            CliSandboxMode::Auto => SandboxMode::Auto,
            CliSandboxMode::WorkspaceWrite => SandboxMode::Sandboxed,
            CliSandboxMode::DangerFullAccess => SandboxMode::Unsandboxed,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum CliProgressMode {
    Rich,
    Plain,
    Off,
}

#[derive(Debug, Parser)]
#[command(version, about = "Markdown-first declarative agent apply")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Apply a single markdown program.
    Apply {
        /// Program markdown file.
        #[arg(short = 'f', long = "file")]
        file: PathBuf,

        /// Program variable value (`NAME=VALUE`). Repeatable.
        #[arg(long = "var", value_name = "NAME=VALUE", action = ArgAction::Append)]
        vars: Vec<String>,

        /// Auto-approve apply without interactive confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,

        /// Print raw provider stdout/stderr (debug output).
        #[arg(short = 'd', long)]
        debug: bool,

        /// Progress rendering mode.
        #[arg(
            short = 'p',
            long = "progress",
            value_enum,
            default_value_t = CliProgressMode::Rich
        )]
        progress_mode: CliProgressMode,

        /// Legacy alias for `--progress off`.
        #[arg(long = "no-progress", action = ArgAction::SetTrue, hide = true)]
        no_progress_legacy: bool,

        /// Legacy alias for `--progress plain`.
        #[arg(long = "no-interactive", action = ArgAction::SetTrue, hide = true)]
        no_interactive_legacy: bool,

        /// Quiet mode: hide intermediate progress steps (read/search/text/turn details).
        #[arg(
            short = 'q',
            long = "quiet",
            alias = "no-intermediate",
            action = ArgAction::SetTrue
        )]
        quiet: bool,

        /// Reset context for this run (ignore prior run history context).
        #[arg(short = 'r', long = "reset", action = ArgAction::SetTrue)]
        reset_context: bool,

        /// Sandbox policy for model-generated shell commands (`auto` escalates when needed).
        #[arg(
            short = 's',
            long = "sandbox",
            alias = "sandbox-mode",
            value_enum,
            default_value_t = CliSandboxMode::Auto
        )]
        sandbox_mode: CliSandboxMode,
    },
    /// Delete local session history and artifacts.
    Reset {
        /// Program id whose session history should be deleted.
        #[arg(short = 'p', long)]
        program: Option<String>,

        /// Delete session history for all programs.
        #[arg(short = 'a', long)]
        all: bool,

        /// Auto-approve destructive delete without interactive confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

pub fn main_entry() {
    if let Err(err) = real_main() {
        if is_user_cancelled_error(&err) {
            print_canceled(true);
            std::process::exit(130);
        }
        if is_blocked_error(&err) {
            print_blocked();
            std::process::exit(2);
        }
        eprintln!("error: {:#}", err);
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = parse_cli();

    match cli.command {
        Commands::Apply {
            file,
            vars,
            yes,
            debug,
            progress_mode,
            no_progress_legacy,
            no_interactive_legacy,
            quiet,
            reset_context,
            sandbox_mode,
        } => {
            let workspace_root =
                env::current_dir().context("failed resolving current working directory")?;
            let runner = CodexRunner;
            let interactive_shell =
                std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            let progress_mode = if no_progress_legacy {
                CliProgressMode::Off
            } else if no_interactive_legacy && progress_mode == CliProgressMode::Rich {
                CliProgressMode::Plain
            } else {
                progress_mode
            };
            let (render_progress, interactive_ui) = match progress_mode {
                CliProgressMode::Rich => (true, interactive_shell),
                CliProgressMode::Plain => (true, false),
                CliProgressMode::Off => (false, false),
            };
            let confirm = interactive_shell && !yes;
            let sandbox_mode: SandboxMode = sandbox_mode.into();
            let program_variables = parse_apply_variables(&vars)?;

            if debug {
                let caps = runner.capabilities();
                println!("Provider: codex");
                println!(
                    "Capabilities: live_events={} partial_text={} tool_call_events={} file_change_events={} resume={} cancel={} approvals={}",
                    caps.live_events,
                    caps.partial_text,
                    caps.tool_call_events,
                    caps.file_change_events,
                    caps.resume,
                    caps.cancel,
                    caps.approvals
                );
                println!(
                    "UI: {}",
                    match progress_mode {
                        CliProgressMode::Off => "off",
                        CliProgressMode::Rich if interactive_ui => "interactive",
                        _ => "plain",
                    }
                );
                println!("Sandbox mode: {}", sandbox_mode.label());
            }

            let result = match run_apply(
                &ApplyRequest {
                    workspace_root,
                    program_path: file,
                    program_variables,
                    confirm,
                    debug,
                    progress: true,
                    render_progress,
                    interactive_ui,
                    show_intermediate_steps: !quiet,
                    use_history_context: !reset_context,
                    sandbox_mode,
                },
                &runner,
            ) {
                Ok(result) => result,
                Err(err) => {
                    if debug {
                        eprintln!(
                            "debug hint: inspect .clawform/programs/*/sessions/*/{{prompt.md,plan.json,events.ndjson,provider.stdout.log,provider.stderr.log}}"
                        );
                    }
                    return Err(err);
                }
            };

            match result.provider_result {
                None => {
                    print_canceled(false);
                }
                Some(run) => {
                    if debug {
                        println!(
                            "history: injected success={} failure={}",
                            yes_no(result.history_injected_success),
                            yes_no(result.history_injected_failure),
                        );
                        print_debug_artifacts(
                            result.prompt_artifact.as_deref(),
                            result.plan_artifact.as_deref(),
                            result.events_artifact.as_deref(),
                            result.provider_stdout_artifact.as_deref(),
                            result.provider_stderr_artifact.as_deref(),
                        );
                    }
                    print_file_summary(
                        result.agent_result.as_ref(),
                        result.agent_human_summary.as_deref(),
                        result.agent_human_summary_artifact.as_deref(),
                        &result.file_results,
                        &run.usage,
                        progress_mode == CliProgressMode::Off || quiet,
                    );

                    if debug {
                        if !run.stdout.trim().is_empty() {
                            println!("Provider stdout:\n{}", run.stdout);
                        }
                        if !run.stderr.trim().is_empty() {
                            eprintln!("Provider stderr:\n{}", run.stderr);
                        }
                    }
                }
            }
        }
        Commands::Reset { program, all, yes } => run_history_reset(program, all, yes)?,
    }

    Ok(())
}

fn parse_cli() -> Cli {
    let mut args: Vec<OsString> = env::args_os().collect();
    if should_show_combined_help(&args) {
        print_combined_help_and_exit();
    }
    if should_infer_apply_subcommand(&args) {
        args.insert(1, OsString::from("apply"));
    }
    Cli::parse_from(args)
}

fn should_show_combined_help(args: &[OsString]) -> bool {
    matches!(
        args.get(1).map(|s| s.to_string_lossy()),
        Some(flag) if flag.as_ref() == "-h" || flag.as_ref() == "--help"
    )
}

fn print_combined_help_and_exit() -> ! {
    let bin = env::args_os()
        .next()
        .and_then(|p| PathBuf::from(p).file_name().map(|n| n.to_owned()))
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cf".to_string());

    let mut top = Cli::command()
        .subcommand_required(false)
        .override_usage(format!(
            "{bin} [GLOBAL_OPTIONS] <COMMAND>\n       {bin} -f <program.md> [APPLY_OPTIONS]"
        ));
    top = top.after_help(
        "Mode guide:\n  GLOBAL_OPTIONS: -h, --help, -V, --version\n  APPLY_OPTIONS: shown in the 'Default apply mode' section below",
    );

    if let Err(err) = top.print_long_help() {
        eprintln!("error: failed rendering help: {err}");
        std::process::exit(2);
    }

    if let Some(mut apply) = Cli::command().find_subcommand("apply").cloned() {
        println!();
        println!();
        println!("Default apply mode (used by `cf -f ...` and by explicit `cf apply ...`):");
        apply = apply.override_usage(format!(
            "{bin} -f <program.md> [APPLY_OPTIONS]\n       {bin} apply -f <program.md> [APPLY_OPTIONS]"
        ));
        if let Err(err) = apply.print_long_help() {
            eprintln!("error: failed rendering apply help: {err}");
            std::process::exit(2);
        }
    }

    println!();
    std::process::exit(0);
}

fn should_infer_apply_subcommand(args: &[OsString]) -> bool {
    let Some(first) = args.get(1).map(|s| s.to_string_lossy()) else {
        return false;
    };

    // Keep explicit top-level command/help/version behavior unchanged.
    if matches!(
        first.as_ref(),
        "apply" | "reset" | "help" | "-h" | "--help" | "-V" | "--version"
    ) {
        return false;
    }

    // Treat no-subcommand flag-based invocations like `cf -f ...` as `cf apply -f ...`.
    first.starts_with('-')
}

fn run_history_reset(program: Option<String>, all: bool, yes: bool) -> Result<()> {
    if all == program.is_some() {
        return Err(anyhow!("specify exactly one of --program or --all"));
    }
    let interactive_shell = io::stdin().is_terminal() && io::stdout().is_terminal();
    if interactive_shell && !yes {
        let target = if all {
            "all programs"
        } else {
            program.as_deref().unwrap_or("selected program")
        };
        if !confirm_history_reset_interactive(target)? {
            print_canceled(false);
            return Ok(());
        }
    }
    let workspace_root =
        env::current_dir().context("failed resolving current working directory")?;
    let outcome = if all {
        reset_history(&workspace_root, HistoryResetTarget::All)?
    } else {
        reset_history(
            &workspace_root,
            HistoryResetTarget::Program(program.expect("validated above")),
        )?
    };

    if outcome.index_deleted {
        println!("history delete: removed history index");
    } else {
        println!(
            "history delete: removed {} session record{}",
            outcome.removed_records,
            if outcome.removed_records == 1 {
                ""
            } else {
                "s"
            }
        );
    }

    Ok(())
}

fn confirm_history_reset_interactive(target: &str) -> Result<bool> {
    let use_color = io::stdout().is_terminal();
    if use_color {
        print!(
            "\x1b[1mProceed with deleting session history for {}?\x1b[0m \x1b[2m[y/N]\x1b[0m ",
            target
        );
    } else {
        print!(
            "Proceed with deleting session history for {}? [y/N] ",
            target
        );
    }
    io::stdout()
        .flush()
        .context("failed flushing history reset prompt")?;

    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("failed reading history reset confirmation")?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}

fn parse_apply_variables(entries: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for raw in entries {
        let Some((name_raw, value_raw)) = raw.split_once('=') else {
            return Err(anyhow!("invalid --var '{}': expected NAME=VALUE", raw));
        };
        let name = name_raw.trim();
        if name.is_empty() {
            return Err(anyhow!(
                "invalid --var '{}': variable name cannot be empty",
                raw
            ));
        }
        if !is_valid_variable_name(name) {
            return Err(anyhow!(
                "invalid --var '{}': NAME must match [A-Za-z_][A-Za-z0-9_]*",
                raw
            ));
        }
        if out.contains_key(name) {
            return Err(anyhow!("duplicate --var for '{}'", name));
        }
        out.insert(name.to_string(), value_raw.to_string());
    }
    Ok(out)
}

fn is_valid_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}

fn print_debug_artifacts(
    prompt: Option<&str>,
    plan: Option<&str>,
    events: Option<&str>,
    stdout_log: Option<&str>,
    stderr_log: Option<&str>,
) {
    if let Some(path) = prompt {
        println!("artifact: prompt={}", path);
    }
    if let Some(path) = plan {
        println!("artifact: plan={}", path);
    }
    if let Some(path) = events {
        println!("artifact: events={}", path);
    }
    if let Some(path) = stdout_log {
        println!("artifact: stdout={}", path);
    }
    if let Some(path) = stderr_log {
        println!("artifact: stderr={}", path);
    }
}

fn print_canceled(ctrl_c: bool) {
    let use_color = std::io::stderr().is_terminal() && std::io::stdout().is_terminal();
    let base = if use_color {
        "\x1b[33mCanceled\x1b[0m"
    } else {
        "Canceled"
    };

    if ctrl_c {
        eprintln!();
        eprintln!("{}.", base);
    } else {
        println!("{}.", base);
    }
}

fn print_blocked() {
    let use_color = std::io::stderr().is_terminal() && std::io::stdout().is_terminal();
    let base = if use_color {
        "\x1b[31mBlocked\x1b[0m"
    } else {
        "Blocked"
    };
    eprintln!("{}.", base);
}

fn is_user_cancelled_error(err: &anyhow::Error) -> bool {
    let text = format!("{:#}", err).to_ascii_lowercase();
    text.contains("cancelled by user")
        || text.contains("ctrl-c")
        || text.contains("interrupted")
        || text.contains("signal 2")
}

fn is_blocked_error(err: &anyhow::Error) -> bool {
    let text = format!("{:#}", err).to_ascii_lowercase();
    if is_user_cancelled_error(err) {
        return false;
    }

    let markers = [
        "blocked",
        "sandbox restriction",
        "network restriction",
        "permission denied",
        "operation not permitted",
        "cannot download required",
        "could not resolve host",
        "failed to lookup address information",
        "network is unreachable",
        "no route to host",
        "temporary failure in name resolution",
    ];
    markers.iter().any(|marker| text.contains(marker))
}

fn print_file_summary(
    agent_result: Option<&AgentResult>,
    agent_human_summary: Option<&str>,
    agent_human_summary_artifact: Option<&str>,
    file_results: &[FileResult],
    usage: &ProviderUsage,
    show_agent_summary_line: bool,
) {
    let use_color = std::io::stdout().is_terminal();

    print_agent_status_line(agent_result, use_color);
    if show_agent_summary_line {
        print_agent_summary_line(agent_human_summary, agent_human_summary_artifact, use_color);
    }
    print_usage_summary(usage, use_color);
    print_reported_files(file_results, use_color);
}

fn print_agent_status_line(agent_result: Option<&AgentResult>, use_color: bool) {
    let Some(agent_result) = agent_result else {
        return;
    };

    if agent_result.status == AgentStatus::Success {
        return;
    }

    let (label, icon) = match agent_result.status {
        AgentStatus::Success => {
            if use_color {
                ("\x1b[32msuccess\x1b[0m", "\x1b[32m◎\x1b[0m")
            } else {
                ("success", "◎")
            }
        }
        AgentStatus::Partial => {
            if use_color {
                ("\x1b[33mpartial\x1b[0m", "\x1b[33m◐\x1b[0m")
            } else {
                ("partial", "◐")
            }
        }
        AgentStatus::Failure => {
            if use_color {
                ("\x1b[31mfailure\x1b[0m", "\x1b[31m✖\x1b[0m")
            } else {
                ("failure", "✖")
            }
        }
    };
    match agent_result.message.as_deref() {
        Some(message) if !message.trim().is_empty() => {
            println!(
                "{} agent {} | {}",
                icon,
                label,
                truncate_one_line(message, 120)
            );
        }
        _ => {
            println!("{} agent {}", icon, label);
        }
    }
}

fn print_agent_summary_line(summary: Option<&str>, artifact_rel: Option<&str>, use_color: bool) {
    let Some(summary) = summary else {
        return;
    };
    let one_line = truncate_one_line(summary, 140);
    let icon = if use_color {
        "\x1b[35m💬\x1b[0m"
    } else {
        "💬"
    };

    let suffix = artifact_rel.map(|rel| {
        if supports_terminal_hyperlinks() {
            match terminal_link(Path::new(rel), "msg") {
                Some(link) => format!(" | {}", link),
                None => " | msg".to_string(),
            }
        } else {
            format!(" | msg={}", rel)
        }
    });

    match suffix {
        Some(s) => println!("{} {}{}", icon, one_line, s),
        None => println!("{} {}", icon, one_line),
    }
}

fn print_reported_files(file_results: &[FileResult], use_color: bool) {
    let mut paths: Vec<String> = file_results
        .iter()
        .filter(|f| f.reported)
        .map(|f| f.path.clone())
        .collect();
    paths.sort();

    if paths.is_empty() {
        println!("changes: 0 files");
        return;
    }

    let total = paths.len();
    println!(
        "changes: {} file{}",
        total,
        if total == 1 { "" } else { "s" }
    );

    if total <= MAX_REPORTED_FILES_DISPLAY {
        for path in paths {
            println!(" {}", colorize_cli_path(&path, use_color));
        }
        return;
    }

    let (folders, hidden_folders) = compact_reported_folders(&paths, MAX_REPORTED_FILES_DISPLAY);
    for (folder, count) in folders {
        println!(
            " {} ({} file{})",
            colorize_cli_path(&format_folder_label(&folder), use_color),
            count,
            if count == 1 { "" } else { "s" }
        );
    }
    if hidden_folders > 0 {
        println!(" … +{} more folders", hidden_folders);
    }
}

fn colorize_cli_path(path: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[36m{}\x1b[0m", path)
    } else {
        path.to_string()
    }
}

fn compact_reported_folders(paths: &[String], max_display: usize) -> (Vec<(String, usize)>, usize) {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for path in paths {
        let folder = reported_parent_folder(path);
        *counts.entry(folder).or_insert(0) += 1;
    }

    let mut folders = counts.into_iter().collect::<Vec<_>>();
    folders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let total_folders = folders.len();
    if total_folders <= max_display {
        return (folders, 0);
    }

    let shown = folders.into_iter().take(max_display).collect::<Vec<_>>();
    let hidden = total_folders.saturating_sub(shown.len());
    (shown, hidden)
}

fn reported_parent_folder(path: &str) -> String {
    let rel = Path::new(path);
    match rel.parent() {
        Some(parent) if !parent.as_os_str().is_empty() && parent != Path::new(".") => {
            parent.to_string_lossy().replace('\\', "/")
        }
        _ => ".".to_string(),
    }
}

fn format_folder_label(folder: &str) -> String {
    if folder == "." {
        "./".to_string()
    } else {
        format!("{}/", folder.trim_end_matches('/'))
    }
}

fn print_usage_summary(usage: &ProviderUsage, use_color: bool) {
    if usage.input_tokens.is_none()
        && usage.cached_input_tokens.is_none()
        && usage.output_tokens.is_none()
    {
        if use_color {
            println!("\x1b[2mtotal | tokens: n/a\x1b[0m");
        } else {
            println!("total | tokens: n/a");
        }
        return;
    }

    let line = format!(
        "total | tokens: in={} out={} cached={}",
        fmt_token_compact_opt(usage.input_tokens),
        fmt_token_compact_opt(usage.output_tokens),
        fmt_token_compact_opt(usage.cached_input_tokens)
    );
    if use_color {
        println!("\x1b[2m{}\x1b[0m", line);
    } else {
        println!("{}", line);
    }
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

fn fmt_token_compact_opt(value: Option<u64>) -> String {
    value
        .map(format_token_compact)
        .unwrap_or_else(|| "n/a".to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_one_line_trims_and_ellipsizes() {
        let input = "line1\nline2 with extra content";
        let out = truncate_one_line(input, 12);
        assert_eq!(out, "line1 lin...");
    }

    #[test]
    fn compact_reported_folders_groups_and_sorts_by_count() {
        let paths = vec![
            "a/one.txt".to_string(),
            "a/two.txt".to_string(),
            "b/one.txt".to_string(),
            "root.txt".to_string(),
        ];
        let (shown, hidden) = compact_reported_folders(&paths, 20);
        assert_eq!(hidden, 0);
        assert_eq!(
            shown,
            vec![
                ("a".to_string(), 2),
                (".".to_string(), 1),
                ("b".to_string(), 1),
            ]
        );
    }

    #[test]
    fn parse_apply_variables_parses_name_value_pairs() {
        let vars =
            parse_apply_variables(&["APP_NAME=calc".to_string(), "APP_PORT=8080".to_string()])
                .expect("must parse");
        assert_eq!(vars.get("APP_NAME").map(String::as_str), Some("calc"));
        assert_eq!(vars.get("APP_PORT").map(String::as_str), Some("8080"));
    }

    #[test]
    fn parse_apply_variables_rejects_invalid_name() {
        let err = parse_apply_variables(&["BAD-NAME=x".to_string()]).expect_err("must fail");
        assert!(format!("{:#}", err).contains("NAME must match"));
    }

    #[test]
    fn infer_apply_subcommand_for_flag_first_invocation() {
        let args = vec![
            OsString::from("cf"),
            OsString::from("-f"),
            OsString::from("examples/smoke.md"),
        ];
        assert!(should_infer_apply_subcommand(&args));
    }

    #[test]
    fn does_not_infer_apply_for_explicit_subcommand() {
        let args = vec![
            OsString::from("cf"),
            OsString::from("reset"),
            OsString::from("--all"),
        ];
        assert!(!should_infer_apply_subcommand(&args));
    }

    #[test]
    fn does_not_infer_apply_for_help_flags() {
        for flag in ["-h", "--help"] {
            let args = vec![OsString::from("cf"), OsString::from(flag)];
            assert!(!should_infer_apply_subcommand(&args));
        }
    }

    #[test]
    fn does_not_infer_apply_for_version_flags() {
        for flag in ["-V", "--version"] {
            let args = vec![OsString::from("cf"), OsString::from(flag)];
            assert!(!should_infer_apply_subcommand(&args));
        }
    }

    #[test]
    fn show_combined_help_for_help_flags() {
        for flag in ["-h", "--help"] {
            let args = vec![OsString::from("cf"), OsString::from(flag)];
            assert!(should_show_combined_help(&args));
        }
    }

    #[test]
    fn do_not_show_combined_help_for_help_subcommand() {
        let args = vec![OsString::from("cf"), OsString::from("help")];
        assert!(!should_show_combined_help(&args));
    }
}
