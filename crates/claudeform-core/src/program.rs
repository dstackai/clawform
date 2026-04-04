use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramFrontmatter {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProgramDocument {
    pub source_path: PathBuf,
    pub raw_markdown: String,
    pub body_markdown: String,
    pub frontmatter: ProgramFrontmatter,
}

pub fn load_program(program_path: &Path) -> Result<ProgramDocument> {
    let source_path = program_path.to_path_buf();
    let raw_markdown = fs::read_to_string(program_path)
        .with_context(|| format!("failed reading program {}", program_path.display()))?;

    let (frontmatter, body_markdown) = parse_frontmatter(&raw_markdown)?;

    if let Some(id) = &frontmatter.id {
        if id.trim().is_empty() {
            bail!("program id cannot be empty");
        }
    }
    if let Some(model) = &frontmatter.model {
        if model.trim().is_empty() {
            bail!("frontmatter model cannot be empty");
        }
    }

    Ok(ProgramDocument {
        source_path,
        raw_markdown,
        body_markdown,
        frontmatter,
    })
}

impl ProgramDocument {
    pub fn program_key(&self) -> Result<String> {
        if let Some(id) = &self.frontmatter.id {
            return Ok(id.trim().to_string());
        }

        let stem = self
            .source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("program filename must have a valid UTF-8 stem")?;

        if stem.trim().is_empty() {
            bail!("program filename stem cannot be empty");
        }

        Ok(stem.to_string())
    }

    pub fn resolved_model<'a>(&'a self, provider_default_model: Option<&'a str>) -> Option<String> {
        self.frontmatter
            .model
            .clone()
            .or_else(|| provider_default_model.map(ToString::to_string))
    }
}

fn parse_frontmatter(raw: &str) -> Result<(ProgramFrontmatter, String)> {
    let mut lines = raw.lines();
    let Some(first) = lines.next() else {
        return Ok((ProgramFrontmatter::default(), String::new()));
    };

    if first.trim() != "---" {
        return Ok((ProgramFrontmatter::default(), raw.to_string()));
    }

    let mut yaml_lines = Vec::new();
    let mut body_start_line_idx = None;

    for (idx, line) in raw.lines().enumerate().skip(1) {
        if line.trim() == "---" {
            body_start_line_idx = Some(idx + 1);
            break;
        }
        yaml_lines.push(line);
    }

    let Some(body_start) = body_start_line_idx else {
        bail!("frontmatter starts with '---' but closing '---' was not found");
    };

    let yaml_text = yaml_lines.join("\n");
    let frontmatter = if yaml_text.trim().is_empty() {
        ProgramFrontmatter::default()
    } else {
        serde_yaml::from_str::<ProgramFrontmatter>(&yaml_text)
            .context("invalid frontmatter YAML or unknown keys")?
    };

    let body = raw.lines().skip(body_start).collect::<Vec<_>>().join("\n");

    Ok((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_without_frontmatter() {
        let input = "# Hello\nBody";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert!(fm.id.is_none());
        assert_eq!(body, input);
    }

    #[test]
    fn parses_frontmatter_with_id_and_model() {
        let input = "---\nid: abc\nmodel: gpt-5\n---\nbody";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert_eq!(fm.id.as_deref(), Some("abc"));
        assert_eq!(fm.model.as_deref(), Some("gpt-5"));
        assert_eq!(body, "body");
    }

    #[test]
    fn fails_on_unknown_frontmatter_key() {
        let input = "---\nname: x\n---\nbody";
        assert!(parse_frontmatter(input).is_err());
    }

    #[test]
    fn program_key_falls_back_to_filename_stem() {
        let doc = ProgramDocument {
            source_path: PathBuf::from("smoke.md"),
            raw_markdown: "x".to_string(),
            body_markdown: "x".to_string(),
            frontmatter: ProgramFrontmatter::default(),
        };
        assert_eq!(doc.program_key().unwrap(), "smoke");
    }
}
