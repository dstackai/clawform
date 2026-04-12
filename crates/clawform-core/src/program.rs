use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramVariableSpec {
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramFrontmatter {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub variables: BTreeMap<String, ProgramVariableSpec>,
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
    validate_skill_definitions(&frontmatter.skills)?;
    validate_variable_definitions(&frontmatter.variables)?;
    validate_variable_references(&body_markdown, &frontmatter.variables)?;

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

    pub fn resolved_skills(&self) -> Vec<String> {
        self.frontmatter
            .skills
            .iter()
            .map(|skill| skill.trim().to_string())
            .collect()
    }

    pub fn resolve_variables(
        &self,
        provided: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        for key in provided.keys() {
            if !is_valid_variable_name(key.as_str()) {
                bail!(
                    "invalid apply variable name '{}': expected [A-Za-z_][A-Za-z0-9_]*",
                    key
                );
            }
            if !self.frontmatter.variables.contains_key(key) {
                bail!(
                    "apply variable '{}' is not defined in program frontmatter variables",
                    key
                );
            }
        }

        let mut resolved = BTreeMap::new();
        for (name, spec) in &self.frontmatter.variables {
            if let Some(value) = provided.get(name) {
                resolved.insert(name.clone(), value.clone());
                continue;
            }
            if let Some(default) = spec.default.as_ref() {
                resolved.insert(name.clone(), default.clone());
                continue;
            }
            bail!(
                "missing required apply variable '{}'; pass '--var {}=VALUE' or set a frontmatter default",
                name,
                name
            );
        }

        Ok(resolved)
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

fn validate_variable_definitions(
    definitions: &BTreeMap<String, ProgramVariableSpec>,
) -> Result<()> {
    for name in definitions.keys() {
        if !is_valid_variable_name(name) {
            bail!(
                "invalid frontmatter variable name '{}': expected [A-Za-z_][A-Za-z0-9_]*",
                name
            );
        }
    }
    Ok(())
}

fn validate_skill_definitions(skills: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for raw in skills {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("frontmatter skill names cannot be empty");
        }
        if trimmed.chars().any(char::is_whitespace) {
            bail!(
                "invalid frontmatter skill '{}': expected a single token without whitespace",
                raw
            );
        }
        if !seen.insert(trimmed.to_string()) {
            bail!("duplicate frontmatter skill '{}'", trimmed);
        }
    }
    Ok(())
}

fn validate_variable_references(
    body_markdown: &str,
    definitions: &BTreeMap<String, ProgramVariableSpec>,
) -> Result<()> {
    let refs = extract_variable_references(body_markdown)?;
    for name in refs {
        if !definitions.contains_key(name.as_str()) {
            bail!(
                "program references undefined variable '{}' via '${{{{ var.{} }}}}'",
                name,
                name
            );
        }
    }
    Ok(())
}

fn extract_variable_references(body_markdown: &str) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let mut cursor = 0usize;

    while let Some(start_rel) = body_markdown[cursor..].find("${{") {
        let start = cursor + start_rel;
        let expr_start = start + 3;
        let Some(end_rel) = body_markdown[expr_start..].find("}}") else {
            break;
        };
        let expr_end = expr_start + end_rel;
        let expr = body_markdown[expr_start..expr_end].trim();
        if let Some(name_raw) = expr.strip_prefix("var.") {
            let name = name_raw.trim();
            if !is_valid_variable_name(name) {
                bail!(
                    "invalid variable reference '${{{{ {} }}}}': expected '${{{{ var.NAME }}}}' with NAME matching [A-Za-z_][A-Za-z0-9_]*",
                    expr
                );
            }
            out.insert(name.to_string());
        }
        cursor = expr_end + 2;
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
        let input = "---\nid: abc\nmodel: gpt-5\nskills: [dstack]\n---\nbody";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert_eq!(fm.id.as_deref(), Some("abc"));
        assert_eq!(fm.model.as_deref(), Some("gpt-5"));
        assert_eq!(fm.skills, vec!["dstack".to_string()]);
        assert!(fm.variables.is_empty());
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

    #[test]
    fn parses_frontmatter_variables() {
        let input =
            "---\nvariables:\n  APP_NAME: {}\n  APP_PORT:\n    default: \"8080\"\n---\nbody";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert!(fm.skills.is_empty());
        assert!(fm.variables.contains_key("APP_NAME"));
        assert_eq!(
            fm.variables
                .get("APP_PORT")
                .and_then(|v| v.default.as_deref()),
            Some("8080")
        );
        assert_eq!(body, "body");
    }

    #[test]
    fn load_program_rejects_invalid_frontmatter_variable_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("program.md");
        fs::write(
            &path,
            "---\nvariables:\n  BAD-NAME: {}\n---\nUse ${{ var.BAD-NAME }}\n",
        )
        .unwrap();
        let err = load_program(&path).expect_err("must fail");
        assert!(format!("{:#}", err).contains("invalid frontmatter variable name"));
    }

    #[test]
    fn load_program_resolves_trimmed_skills() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("program.md");
        fs::write(
            &path,
            "---\nskills:\n  - dstack\n  - find-skills\n---\nbody\n",
        )
        .unwrap();

        let program = load_program(&path).unwrap();
        assert_eq!(
            program.resolved_skills(),
            vec!["dstack".to_string(), "find-skills".to_string()]
        );
    }

    #[test]
    fn load_program_rejects_duplicate_skill_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("program.md");
        fs::write(&path, "---\nskills: [dstack, dstack]\n---\nbody\n").unwrap();

        let err = load_program(&path).expect_err("must reject duplicate skills");
        assert!(format!("{:#}", err).contains("duplicate frontmatter skill"));
    }

    #[test]
    fn load_program_rejects_skill_names_with_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("program.md");
        fs::write(&path, "---\nskills: ['bad skill']\n---\nbody\n").unwrap();

        let err = load_program(&path).expect_err("must reject invalid skill names");
        assert!(format!("{:#}", err).contains("invalid frontmatter skill"));
    }

    #[test]
    fn load_program_rejects_undefined_variable_reference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("program.md");
        fs::write(
            &path,
            "---\nvariables:\n  APP_NAME: {}\n---\nUse ${{ var.APP_PORT }}\n",
        )
        .unwrap();
        let err = load_program(&path).expect_err("must fail");
        assert!(format!("{:#}", err).contains("undefined variable"));
    }

    #[test]
    fn load_program_rejects_invalid_variable_reference_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("program.md");
        fs::write(
            &path,
            "---\nvariables:\n  APP_NAME: {}\n---\nUse ${{ var.APP-NAME }}\n",
        )
        .unwrap();
        let err = load_program(&path).expect_err("must fail");
        assert!(format!("{:#}", err).contains("invalid variable reference"));
    }

    #[test]
    fn resolve_variables_merges_cli_values_and_defaults() {
        let doc = ProgramDocument {
            source_path: PathBuf::from("program.md"),
            raw_markdown: String::new(),
            body_markdown: String::new(),
            frontmatter: ProgramFrontmatter {
                id: None,
                model: None,
                skills: Vec::new(),
                variables: BTreeMap::from([
                    (
                        "APP_NAME".to_string(),
                        ProgramVariableSpec { default: None },
                    ),
                    (
                        "APP_PORT".to_string(),
                        ProgramVariableSpec {
                            default: Some("8080".to_string()),
                        },
                    ),
                ]),
            },
        };
        let provided = BTreeMap::from([("APP_NAME".to_string(), "calc".to_string())]);
        let resolved = doc.resolve_variables(&provided).expect("must resolve");
        assert_eq!(resolved.get("APP_NAME").map(String::as_str), Some("calc"));
        assert_eq!(resolved.get("APP_PORT").map(String::as_str), Some("8080"));
    }

    #[test]
    fn resolve_variables_rejects_missing_required_value() {
        let doc = ProgramDocument {
            source_path: PathBuf::from("program.md"),
            raw_markdown: String::new(),
            body_markdown: String::new(),
            frontmatter: ProgramFrontmatter {
                id: None,
                model: None,
                skills: Vec::new(),
                variables: BTreeMap::from([(
                    "APP_NAME".to_string(),
                    ProgramVariableSpec { default: None },
                )]),
            },
        };
        let err = doc
            .resolve_variables(&BTreeMap::new())
            .expect_err("must fail");
        assert!(format!("{:#}", err).contains("missing required apply variable"));
    }

    #[test]
    fn resolve_variables_rejects_unknown_apply_variable() {
        let doc = ProgramDocument {
            source_path: PathBuf::from("program.md"),
            raw_markdown: String::new(),
            body_markdown: String::new(),
            frontmatter: ProgramFrontmatter {
                id: None,
                model: None,
                skills: Vec::new(),
                variables: BTreeMap::new(),
            },
        };
        let err = doc
            .resolve_variables(&BTreeMap::from([("APP_NAME".to_string(), "x".to_string())]))
            .expect_err("must fail");
        assert!(format!("{:#}", err).contains("is not defined"));
    }
}
