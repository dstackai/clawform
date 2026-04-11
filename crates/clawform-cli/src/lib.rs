use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use serde_json::Value;

#[cfg(test)]
use clawform_core::AgentReason;
use clawform_core::{
    load_config, reset_history, resolve_provider_runner, run_apply, AgentResult, AgentStatus,
    ApplyRequest, FileResult, HistoryResetTarget, ProviderUsage, SandboxMode,
};

const MAX_REPORTED_FILES_DISPLAY: usize = 20;
const AGENT_RESULT_REL: &str = ".clawform/agent_result.json";
const HELP_RENDER_WIDTH: usize = 100;
const HELP_SPEC_WIDTH: usize = 30;
const TOP_LEVEL_HELP_ROWS: &[(&str, &str)] = &[
    ("-h, --help", "print help"),
    ("-V, --version", "print version"),
];
const SUBCOMMAND_HELP_ROWS: &[(&str, &str)] = &[
    ("apply", "apply a single markdown program"),
    ("reset", "delete local session history and artifacts"),
    ("help", "print help for a subcommand"),
];
const APPLY_HELP_ROWS: &[(&str, &str)] = &[
    ("-f, --file <FILE>", "program markdown file"),
    (
        "-p, --provider <PROVIDER>",
        "provider name from .clawform/config.json",
    ),
    (
        "--var <NAME=VALUE>",
        "program variable (NAME=VALUE), repeatable",
    ),
    ("-y, --yes", "skip confirmation prompt"),
    ("-d, --debug", "enable debug output"),
    (
        "-v, --verbose",
        "print full command and message outputs in the live stream",
    ),
    (
        "--progress <MODE>",
        "progress mode (default: rich; values: rich, plain, off)",
    ),
    (
        "-q, --quiet",
        "hide intermediate progress steps (read/search/text/turn details)",
    ),
    ("-r, --reset", "ignore prior run history context"),
    (
        "-s, --sandbox <MODE>",
        "sandbox mode (default: auto; values: auto, workspace, full-access)",
    ),
    ("--auto", "shorthand for --sandbox auto"),
    ("--workspace", "shorthand for --sandbox workspace"),
    ("--full-access", "shorthand for --sandbox full-access"),
    ("-h, --help", "print help"),
];

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum CliSandboxMode {
    Auto,
    #[value(alias = "workspace-write", alias = "sandboxed")]
    Workspace,
    #[value(alias = "danger-full-access", alias = "unsandboxed", alias = "full")]
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

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum CliProgressMode {
    Rich,
    Plain,
    Off,
}

#[derive(Debug, Parser)]
#[command(version, about = "markdown-first declarative agent apply")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// apply a single markdown program.
    Apply {
        /// program markdown file.
        #[arg(short = 'f', long = "file")]
        file: PathBuf,

        /// provider name from `.clawform/config.json`.
        #[arg(short = 'p', long = "provider", value_name = "PROVIDER")]
        provider: Option<String>,

        /// program variable (`NAME=VALUE`). repeatable.
        #[arg(long = "var", value_name = "NAME=VALUE", action = ArgAction::Append)]
        vars: Vec<String>,

        /// skip confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,

        /// enable debug output.
        #[arg(short = 'd', long)]
        debug: bool,

        /// print full command and message outputs in the live stream.
        #[arg(short = 'v', long)]
        verbose: bool,

        /// progress mode.
        #[arg(
            long = "progress",
            value_enum,
            default_value_t = CliProgressMode::Rich
        )]
        progress_mode: CliProgressMode,

        /// legacy alias for `--progress off`.
        #[arg(long = "no-progress", action = ArgAction::SetTrue, hide = true)]
        no_progress_legacy: bool,

        /// legacy alias for `--progress plain`.
        #[arg(long = "no-interactive", action = ArgAction::SetTrue, hide = true)]
        no_interactive_legacy: bool,

        /// hide intermediate progress steps (read/search/text/turn details).
        #[arg(
            short = 'q',
            long = "quiet",
            alias = "no-intermediate",
            action = ArgAction::SetTrue
        )]
        quiet: bool,

        /// ignore prior run history context.
        #[arg(short = 'r', long = "reset", action = ArgAction::SetTrue)]
        reset_context: bool,

        #[arg(
            short = 's',
            long = "sandbox",
            alias = "sandbox-mode",
            help = "sandbox mode (default: auto)",
            value_enum,
            value_name = "MODE"
        )]
        sandbox_mode: Option<CliSandboxMode>,

        /// shorthand for `--sandbox auto`.
        #[arg(
            long = "auto",
            action = ArgAction::SetTrue,
            conflicts_with_all = ["sandbox_mode", "sandbox_workspace", "sandbox_full_access"]
        )]
        sandbox_auto: bool,

        /// shorthand for `--sandbox workspace`.
        #[arg(
            long = "workspace",
            action = ArgAction::SetTrue,
            conflicts_with_all = ["sandbox_mode", "sandbox_auto", "sandbox_full_access"]
        )]
        sandbox_workspace: bool,

        /// shorthand for `--sandbox full-access`.
        #[arg(
            long = "full-access",
            action = ArgAction::SetTrue,
            conflicts_with_all = ["sandbox_mode", "sandbox_auto", "sandbox_workspace"]
        )]
        sandbox_full_access: bool,
    },
    /// delete local session history and artifacts.
    Reset {
        /// program id whose session history should be deleted.
        #[arg(short = 'p', long)]
        program: Option<String>,

        /// delete session history for all programs.
        #[arg(short = 'a', long)]
        all: bool,

        /// skip confirmation prompt.
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
            provider,
            vars,
            yes,
            debug,
            verbose,
            progress_mode,
            no_progress_legacy,
            no_interactive_legacy,
            quiet,
            reset_context,
            sandbox_mode,
            sandbox_auto,
            sandbox_workspace,
            sandbox_full_access,
        } => {
            let workspace_root =
                env::current_dir().context("failed resolving current working directory")?;
            let config = load_config(&workspace_root)?;
            let resolved_provider = config.resolve_provider(provider.as_deref())?;
            let runner = resolve_provider_runner(resolved_provider.provider_type)?;
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
            let sandbox_mode = resolve_cli_sandbox_mode(
                sandbox_mode,
                sandbox_auto,
                sandbox_workspace,
                sandbox_full_access,
            )?;
            let program_variables = parse_apply_variables(&vars)?;

            if debug {
                let caps = runner.capabilities();
                println!(
                    "Provider: {} ({})",
                    resolved_provider.name, resolved_provider.provider_type
                );
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
                    workspace_root: workspace_root.clone(),
                    program_path: file,
                    provider_name: provider,
                    program_variables,
                    confirm,
                    debug,
                    verbose_output: verbose,
                    progress: true,
                    render_progress,
                    interactive_ui,
                    show_intermediate_steps: !quiet,
                    use_history_context: !reset_context,
                    sandbox_mode,
                },
                runner,
            ) {
                Ok(result) => result,
                Err(err) => {
                    maybe_print_agent_status_from_result_file(&workspace_root);
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
                    }
                    print_file_summary(
                        result.agent_result.as_ref(),
                        result.agent_human_summary.as_deref(),
                        result.agent_human_summary_artifact.as_deref(),
                        &result.file_results,
                        &run.usage,
                        progress_mode == CliProgressMode::Off || quiet,
                    );
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
    if should_show_apply_help(&args) {
        print_apply_help_and_exit();
    }
    if should_infer_apply_subcommand(&args) {
        args.insert(1, OsString::from("apply"));
    }
    Cli::parse_from(args)
}

fn resolve_cli_sandbox_mode(
    sandbox_mode: Option<CliSandboxMode>,
    sandbox_auto: bool,
    sandbox_workspace: bool,
    sandbox_full_access: bool,
) -> Result<SandboxMode> {
    let mut selected = Vec::new();

    if let Some(mode) = sandbox_mode {
        selected.push(mode);
    }
    if sandbox_auto {
        selected.push(CliSandboxMode::Auto);
    }
    if sandbox_workspace {
        selected.push(CliSandboxMode::Workspace);
    }
    if sandbox_full_access {
        selected.push(CliSandboxMode::FullAccess);
    }

    if selected.len() > 1 {
        return Err(anyhow!(
            "choose only one sandbox mode: use either --sandbox <MODE> or one of --auto/--workspace/--full-access"
        ));
    }

    Ok(selected
        .into_iter()
        .next()
        .unwrap_or(CliSandboxMode::Auto)
        .into())
}

fn should_show_combined_help(args: &[OsString]) -> bool {
    matches!(
        args.get(1).map(|s| s.to_string_lossy()),
        Some(flag) if flag.as_ref() == "-h" || flag.as_ref() == "--help"
    )
}

fn should_show_apply_help(args: &[OsString]) -> bool {
    matches!(
        (
            args.get(1).map(|s| s.to_string_lossy()),
            args.get(2).map(|s| s.to_string_lossy())
        ),
        (Some(cmd), Some(flag))
            if cmd.as_ref() == "apply" && (flag.as_ref() == "-h" || flag.as_ref() == "--help")
    )
}

fn print_combined_help_and_exit() -> ! {
    let bin = env::args_os()
        .next()
        .and_then(|p| PathBuf::from(p).file_name().map(|n| n.to_owned()))
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cf".to_string());

    let mut help = String::new();
    help.push_str("markdown-first declarative agent apply\n\n");
    help.push_str(&format!(
        "{} {bin} -f <program.md> [apply options]\n       {bin} <subcommand>\n\n",
        format_help_heading("Usage:")
    ));
    help.push_str(&render_help_rows(
        &format_help_heading("Subcommands:"),
        SUBCOMMAND_HELP_ROWS,
    ));
    help.push('\n');
    help.push_str(&render_help_rows(
        &format_help_heading("Options:"),
        TOP_LEVEL_HELP_ROWS,
    ));
    help.push('\n');
    help.push_str(&render_apply_options_block(&format_help_heading(
        "Apply options:",
    )));
    print!("{help}");
    std::process::exit(0);
}

fn print_apply_help_and_exit() -> ! {
    let bin = env::args_os()
        .next()
        .and_then(|p| PathBuf::from(p).file_name().map(|n| n.to_owned()))
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cf".to_string());

    let mut help = String::new();
    help.push_str("apply a single markdown program\n\n");
    help.push_str(&format!(
        "{} {bin} apply [options] --file <FILE>\n\n",
        format_help_heading("Usage:")
    ));
    help.push_str(&render_help_rows(
        &format_help_heading("Options:"),
        APPLY_HELP_ROWS,
    ));
    print!("{help}");
    std::process::exit(0);
}

fn render_apply_options_block(heading: &str) -> String {
    render_help_rows(heading, APPLY_HELP_ROWS)
}

fn render_help_rows(heading: &str, rows: &[(&str, &str)]) -> String {
    let mut out = String::new();
    out.push_str(heading);
    out.push('\n');

    let desc_width = HELP_RENDER_WIDTH
        .saturating_sub(2 + HELP_SPEC_WIDTH + 2)
        .max(24);

    for (spec, desc) in rows {
        let wrapped = wrap_help_text(desc, desc_width);
        let mut lines = wrapped.into_iter();
        if let Some(first) = lines.next() {
            out.push_str(&format!(
                "  {:<width$}  {}\n",
                spec,
                first,
                width = HELP_SPEC_WIDTH
            ));
        }
        for line in lines {
            out.push_str(&format!(
                "  {:<width$}  {}\n",
                "",
                line,
                width = HELP_SPEC_WIDTH
            ));
        }
    }
    out
}

fn format_help_heading(text: &str) -> String {
    if io::stdout().is_terminal() {
        format!("\x1b[1m{}\x1b[0m", text)
    } else {
        text.to_string()
    }
}

fn wrap_help_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let next_len = current.len() + usize::from(!current.is_empty()) + word.len();
        if next_len > width && !current.is_empty() {
            lines.push(current);
            current = word.to_string();
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
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

    let result_suffix = render_result_suffix(Path::new(AGENT_RESULT_REL), use_color);
    println!(
        "{}",
        format_agent_status_line(agent_result, use_color, &result_suffix)
    );
}

fn format_agent_status_line(
    agent_result: &AgentResult,
    use_color: bool,
    result_suffix: &str,
) -> String {
    let badge = format_agent_status_badge(agent_result.status.clone(), use_color);
    let reason_suffix = agent_result
        .reason
        .map(|reason| reason.as_str().to_string());
    let message_suffix = agent_result.message.as_deref().and_then(|message| {
        let trimmed = message.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_one_line(trimmed, 120))
        }
    });

    let mut parts = vec![badge.to_string()];
    if let Some(reason) = reason_suffix {
        parts.push(reason);
    }
    if let Some(message) = message_suffix {
        parts.push(message);
    }
    let mut line = parts.join(" ");
    if !result_suffix.trim().is_empty() {
        line.push_str(status_file_separator(use_color));
        line.push_str(result_suffix);
    }
    line
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

    let sep = if use_color {
        " \x1b[2m|\x1b[0m "
    } else {
        " | "
    };
    let suffix = artifact_rel.map(|rel| {
        if supports_terminal_hyperlinks() {
            match terminal_link(Path::new(rel), "msg") {
                Some(link) => format!("{}{}", sep, style_short_link_token(&link, use_color)),
                None => format!("{}{}", sep, style_short_link_token("msg", use_color)),
            }
        } else {
            if use_color {
                format!("{}\x1b[95mmsg\x1b[0m={}", sep, rel)
            } else {
                format!("{}msg={}", sep, rel)
            }
        }
    });

    match suffix {
        Some(s) => println!("{} {}{}", icon, one_line, s),
        None => println!("{} {}", icon, one_line),
    }
}

fn maybe_print_agent_status_from_result_file(workspace_root: &Path) {
    let path = workspace_root.join(AGENT_RESULT_REL);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return,
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return,
    };

    let status = parsed
        .get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(status) = status else {
        return;
    };

    let reason = parsed
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let message = parsed
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let use_color = std::io::stderr().is_terminal() && std::io::stdout().is_terminal();
    let mut parts = vec![if let Some(status) = parse_agent_status(status) {
        format_agent_status_badge(status, use_color)
    } else {
        format!("• {}", status)
    }];
    if let Some(reason) = reason {
        parts.push(reason.to_string());
    }
    if let Some(message) = message {
        parts.push(truncate_one_line(message, 120));
    }
    let mut line = parts.join(" ");
    let result_suffix = render_result_suffix(&path, use_color);
    if !result_suffix.trim().is_empty() {
        line.push_str(status_file_separator(use_color));
        line.push_str(&result_suffix);
    }
    eprintln!("{}", line);
}

fn status_file_separator(use_color: bool) -> &'static str {
    if use_color {
        " \x1b[2m|\x1b[0m "
    } else {
        " | "
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

fn render_result_suffix(path: &Path, use_color: bool) -> String {
    render_result_suffix_with_hyperlinks(path, use_color, supports_terminal_hyperlinks())
}

fn render_result_suffix_with_hyperlinks(
    path: &Path,
    use_color: bool,
    supports_hyperlinks: bool,
) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|| path.to_path_buf())
    };
    let label = if supports_hyperlinks {
        match terminal_link(&abs, "file") {
            Some(link) => link,
            None => "file".to_string(),
        }
    } else {
        "file".to_string()
    };

    style_short_link_token(&label, use_color)
}

fn style_short_link_token(token: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[95m{}\x1b[0m", token)
    } else {
        token.to_string()
    }
}

fn format_agent_status_badge(status: AgentStatus, use_color: bool) -> String {
    let (icon, label, color_code) = match status {
        AgentStatus::Success => ("✅", None, "32"),
        AgentStatus::Partial => ("🟡", Some("partial"), "33"),
        AgentStatus::Failure => ("❌", Some("failed"), "31"),
    };
    let styled_icon = if use_color {
        format!("\x1b[{}m{}\x1b[0m", color_code, icon)
    } else {
        icon.to_string()
    };
    match label {
        Some(label) => {
            let styled_label = if use_color {
                format!("\x1b[{}m{}\x1b[0m", color_code, label)
            } else {
                label.to_string()
            };
            format!("{} {}", styled_icon, styled_label)
        }
        None => styled_icon,
    }
}

fn parse_agent_status(value: &str) -> Option<AgentStatus> {
    match value {
        "success" => Some(AgentStatus::Success),
        "partial" => Some(AgentStatus::Partial),
        "failure" => Some(AgentStatus::Failure),
        _ => None,
    }
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

    #[test]
    fn show_apply_help_for_apply_help_flags() {
        for flag in ["-h", "--help"] {
            let args = vec![
                OsString::from("cf"),
                OsString::from("apply"),
                OsString::from(flag),
            ];
            assert!(should_show_apply_help(&args));
        }
    }

    #[test]
    fn do_not_show_apply_help_for_top_level_help() {
        let args = vec![OsString::from("cf"), OsString::from("--help")];
        assert!(!should_show_apply_help(&args));
    }

    #[test]
    fn print_agent_status_line_includes_reason_and_result_path() {
        let result = AgentResult {
            status: AgentStatus::Failure,
            reason: Some(AgentReason::ProgramBlocked),
            message: Some("failed to connect".to_string()),
        };

        let line = format_agent_status_line(&result, false, "file");
        assert!(line.contains("❌ failed"));
        assert!(line.contains("program_blocked"));
        assert!(line.contains(" | file"));
        assert!(line.contains("failed to connect"));
    }

    #[test]
    fn print_agent_status_line_includes_success_status() {
        let result = AgentResult {
            status: AgentStatus::Success,
            reason: None,
            message: Some("ok".to_string()),
        };

        let line = format_agent_status_line(&result, false, "file");
        assert!(line.contains("✅"));
        assert!(!line.contains("done"));
        assert!(line.ends_with(" | file"));
    }

    #[test]
    fn render_result_suffix_colors_file_label_without_hyperlink() {
        let suffix = render_result_suffix_with_hyperlinks(
            Path::new(".clawform/agent_result.json"),
            true,
            false,
        );
        assert_eq!(suffix, "\x1b[95mfile\x1b[0m");
    }

    #[test]
    fn style_short_link_token_uses_pink() {
        assert_eq!(style_short_link_token("msg", true), "\x1b[95mmsg\x1b[0m");
        assert_eq!(style_short_link_token("file", true), "\x1b[95mfile\x1b[0m");
    }

    #[test]
    fn apply_cli_accepts_provider_override_short_flag() {
        let cli = Cli::try_parse_from(["cf", "apply", "-f", "demo.md", "-p", "claude_safe"])
            .expect("must parse");

        match cli.command {
            Commands::Apply { provider, .. } => {
                assert_eq!(provider.as_deref(), Some("claude_safe"));
            }
            _ => panic!("expected apply command"),
        }
    }

    #[test]
    fn apply_cli_accepts_workspace_sandbox_value() {
        let cli = Cli::try_parse_from(["cf", "apply", "-f", "demo.md", "--sandbox", "workspace"])
            .expect("must parse");

        match cli.command {
            Commands::Apply { sandbox_mode, .. } => {
                assert_eq!(sandbox_mode, Some(CliSandboxMode::Workspace));
            }
            _ => panic!("expected apply command"),
        }
    }

    #[test]
    fn apply_cli_accepts_legacy_workspace_write_alias() {
        let cli = Cli::try_parse_from([
            "cf",
            "apply",
            "-f",
            "demo.md",
            "--sandbox",
            "workspace-write",
        ])
        .expect("must parse");

        match cli.command {
            Commands::Apply { sandbox_mode, .. } => {
                assert_eq!(sandbox_mode, Some(CliSandboxMode::Workspace));
            }
            _ => panic!("expected apply command"),
        }
    }

    #[test]
    fn apply_cli_accepts_full_access_shortcut_flag() {
        let cli = Cli::try_parse_from(["cf", "apply", "-f", "demo.md", "--full-access"])
            .expect("must parse");

        match cli.command {
            Commands::Apply {
                sandbox_mode,
                sandbox_full_access,
                ..
            } => {
                assert_eq!(sandbox_mode, None);
                assert!(sandbox_full_access);
            }
            _ => panic!("expected apply command"),
        }
    }

    #[test]
    fn apply_cli_accepts_workspace_shortcut_flag() {
        let cli = Cli::try_parse_from(["cf", "apply", "-f", "demo.md", "--workspace"])
            .expect("must parse");

        match cli.command {
            Commands::Apply {
                sandbox_mode,
                sandbox_workspace,
                ..
            } => {
                assert_eq!(sandbox_mode, None);
                assert!(sandbox_workspace);
            }
            _ => panic!("expected apply command"),
        }
    }

    #[test]
    fn apply_cli_rejects_conflicting_sandbox_selectors() {
        let err = Cli::try_parse_from([
            "cf",
            "apply",
            "-f",
            "demo.md",
            "--sandbox",
            "workspace",
            "--full-access",
        ])
        .expect_err("must fail");

        let rendered = err.to_string();
        assert!(
            rendered.contains("--sandbox <MODE>") || rendered.contains("--full-access"),
            "unexpected clap error: {rendered}"
        );
    }

    #[test]
    fn resolve_cli_sandbox_mode_defaults_to_auto() {
        let mode = resolve_cli_sandbox_mode(None, false, false, false).expect("must resolve");
        assert_eq!(mode, SandboxMode::Auto);
    }

    #[test]
    fn resolve_cli_sandbox_mode_prefers_shortcut_flags() {
        let mode = resolve_cli_sandbox_mode(None, false, true, false).expect("must resolve");
        assert_eq!(mode, SandboxMode::Sandboxed);

        let mode = resolve_cli_sandbox_mode(None, false, false, true).expect("must resolve");
        assert_eq!(mode, SandboxMode::Unsandboxed);
    }

    #[test]
    fn apply_cli_still_accepts_progress_long_flag() {
        let cli = Cli::try_parse_from(["cf", "apply", "-f", "demo.md", "--progress", "off"])
            .expect("must parse");

        match cli.command {
            Commands::Apply { progress_mode, .. } => {
                assert_eq!(progress_mode, CliProgressMode::Off);
            }
            _ => panic!("expected apply command"),
        }
    }

    #[test]
    fn render_apply_options_block_is_flat_and_explicit() {
        let block = render_apply_options_block("Apply options:");
        assert!(block.starts_with("Apply options:\n"));
        assert!(block.contains("\n  --var <NAME=VALUE>"));
        assert!(block.contains("\n  --progress <MODE>"));
        assert!(block.contains("--sandbox <MODE>"));
        assert!(block.contains("\n  --auto"));
        assert!(block.contains("enable debug output"));
        assert!(!block.contains("\n      --var"));
    }
}
