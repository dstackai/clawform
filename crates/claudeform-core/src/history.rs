use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Success,
    Failure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunHistoryRecord {
    pub ts_unix: u64,
    pub program_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub status: RunStatus,
    pub model: Option<String>,
    pub summary_short: Option<String>,
    pub files_total: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub files_sample: Vec<String>,
    pub error_short: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct ProgramHistoryContext {
    pub last_session: Option<RunHistoryRecord>,
    pub last_success: Option<RunHistoryRecord>,
    pub last_failure: Option<RunHistoryRecord>,
}

#[derive(Debug, Clone)]
pub enum HistoryResetTarget {
    All,
    Program(String),
}

#[derive(Debug, Clone, Default)]
pub struct HistoryResetOutcome {
    pub removed_records: usize,
    pub index_deleted: bool,
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn history_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".claudeform/history")
}

fn legacy_sessions_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".claudeform/sessions")
}

fn legacy_runs_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".claudeform/runs")
}

pub fn history_index_path(workspace_root: &Path) -> PathBuf {
    history_dir(workspace_root).join("index.jsonl")
}

fn legacy_sessions_history_index_path(workspace_root: &Path) -> PathBuf {
    legacy_sessions_dir(workspace_root).join("index.jsonl")
}

fn legacy_history_index_path(workspace_root: &Path) -> PathBuf {
    legacy_runs_dir(workspace_root).join("index.jsonl")
}

fn migrate_history_index_if_needed(workspace_root: &Path) -> Result<()> {
    let new_path = history_index_path(workspace_root);
    if new_path.exists() {
        return Ok(());
    }

    let legacy_sessions_path = legacy_sessions_history_index_path(workspace_root);
    let legacy_runs_path = legacy_history_index_path(workspace_root);
    let legacy_path = if legacy_sessions_path.exists() {
        legacy_sessions_path
    } else if legacy_runs_path.exists() {
        legacy_runs_path
    } else {
        return Ok(());
    };

    if let Some(parent) = new_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating history directory '{}' during history migration",
                parent.display()
            )
        })?;
    }

    fs::rename(&legacy_path, &new_path).with_context(|| {
        format!(
            "failed migrating history index '{}' -> '{}'",
            legacy_path.display(),
            new_path.display()
        )
    })?;
    Ok(())
}

pub fn append_history_record(workspace_root: &Path, record: &RunHistoryRecord) -> Result<()> {
    migrate_history_index_if_needed(workspace_root)?;
    let path = history_index_path(workspace_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating history directory '{}'", parent.display()))?;
    }

    let raw = serde_json::to_vec(record).context("failed serializing history record")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed opening history index '{}'", path.display()))?;
    file.write_all(&raw)
        .with_context(|| format!("failed writing history index '{}'", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("failed finalizing history index '{}'", path.display()))?;
    Ok(())
}

pub fn load_program_history_context(
    workspace_root: &Path,
    program_id: &str,
) -> Result<ProgramHistoryContext> {
    migrate_history_index_if_needed(workspace_root)?;
    let path = history_index_path(workspace_root);
    if !path.exists() {
        return Ok(ProgramHistoryContext::default());
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed reading history index '{}'", path.display()))?;
    let mut out = ProgramHistoryContext::default();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<RunHistoryRecord>(line) else {
            continue;
        };
        if rec.program_id != program_id {
            continue;
        }
        out.last_session = Some(rec.clone());
        match rec.status {
            RunStatus::Success => out.last_success = Some(rec),
            RunStatus::Failure => out.last_failure = Some(rec),
        }
    }

    Ok(out)
}

pub fn reset_history(
    workspace_root: &Path,
    target: HistoryResetTarget,
) -> Result<HistoryResetOutcome> {
    migrate_history_index_if_needed(workspace_root)?;
    let path = history_index_path(workspace_root);
    if !path.exists() {
        return Ok(HistoryResetOutcome::default());
    }

    match target {
        HistoryResetTarget::All => {
            fs::remove_file(&path)
                .with_context(|| format!("failed removing history index '{}'", path.display()))?;
            Ok(HistoryResetOutcome {
                removed_records: 0,
                index_deleted: true,
            })
        }
        HistoryResetTarget::Program(program_id) => {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed reading history index '{}'", path.display()))?;

            let mut kept_lines = Vec::new();
            let mut removed_records = 0usize;
            for line in raw.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<RunHistoryRecord>(trimmed) {
                    Ok(rec) if rec.program_id == program_id => {
                        removed_records += 1;
                    }
                    _ => kept_lines.push(trimmed.to_string()),
                }
            }

            let mut out = String::new();
            for line in &kept_lines {
                out.push_str(line);
                out.push('\n');
            }
            fs::write(&path, out)
                .with_context(|| format!("failed rewriting history index '{}'", path.display()))?;

            Ok(HistoryResetOutcome {
                removed_records,
                index_deleted: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_latest_success_and_failure_for_program() -> Result<()> {
        let ws = tempfile::tempdir()?;
        append_history_record(
            ws.path(),
            &RunHistoryRecord {
                ts_unix: 1,
                program_id: "a".to_string(),
                session_id: Some("s1".to_string()),
                status: RunStatus::Success,
                model: None,
                summary_short: Some("ok".to_string()),
                files_total: 1,
                insertions: 1,
                deletions: 0,
                files_sample: vec!["a.txt".to_string()],
                error_short: None,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
            },
        )?;
        append_history_record(
            ws.path(),
            &RunHistoryRecord {
                ts_unix: 2,
                program_id: "a".to_string(),
                session_id: Some("s2".to_string()),
                status: RunStatus::Failure,
                model: None,
                summary_short: None,
                files_total: 0,
                insertions: 0,
                deletions: 0,
                files_sample: Vec::new(),
                error_short: Some("boom".to_string()),
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
            },
        )?;

        let ctx = load_program_history_context(ws.path(), "a")?;
        assert!(ctx.last_session.is_some());
        assert!(ctx.last_success.is_some());
        assert!(ctx.last_failure.is_some());
        Ok(())
    }

    #[test]
    fn reset_program_removes_only_matching_records() -> Result<()> {
        let ws = tempfile::tempdir()?;
        append_history_record(
            ws.path(),
            &RunHistoryRecord {
                ts_unix: 1,
                program_id: "a".to_string(),
                session_id: Some("s1".to_string()),
                status: RunStatus::Success,
                model: None,
                summary_short: Some("ok".to_string()),
                files_total: 1,
                insertions: 1,
                deletions: 0,
                files_sample: vec!["a.txt".to_string()],
                error_short: None,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
            },
        )?;
        append_history_record(
            ws.path(),
            &RunHistoryRecord {
                ts_unix: 2,
                program_id: "b".to_string(),
                session_id: Some("s2".to_string()),
                status: RunStatus::Success,
                model: None,
                summary_short: Some("ok".to_string()),
                files_total: 1,
                insertions: 1,
                deletions: 0,
                files_sample: vec!["b.txt".to_string()],
                error_short: None,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
            },
        )?;

        let outcome = reset_history(ws.path(), HistoryResetTarget::Program("a".to_string()))?;
        assert_eq!(outcome.removed_records, 1);
        let ctx_a = load_program_history_context(ws.path(), "a")?;
        let ctx_b = load_program_history_context(ws.path(), "b")?;
        assert!(ctx_a.last_success.is_none());
        assert!(ctx_b.last_success.is_some());
        Ok(())
    }
}
