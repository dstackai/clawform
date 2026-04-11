use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    clawform: Option<ToolConfig>,
    #[serde(default)]
    claudeform: Option<ToolConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Codex,
    Claude,
}

impl ProviderKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub name: String,
    pub provider_type: ProviderKind,
    pub default_model: Option<String>,
}

pub fn load_config(workspace_root: &Path) -> Result<ToolConfig> {
    let primary_path = workspace_root.join(".clawform/config.json");
    let legacy_path = workspace_root.join(".claudeform/config.json");
    let (path, raw) = match fs::read_to_string(&primary_path) {
        Ok(raw) => (primary_path, raw),
        Err(primary_err) if primary_err.kind() == ErrorKind::NotFound => {
            match fs::read_to_string(&legacy_path) {
                Ok(raw) => (legacy_path, raw),
                Err(legacy_err) if legacy_err.kind() == ErrorKind::NotFound => {
                    return Err(primary_err).with_context(|| {
                        format!("failed reading config file {}", primary_path.display())
                    });
                }
                Err(legacy_err) => {
                    return Err(legacy_err).with_context(|| {
                        format!("failed reading config file {}", legacy_path.display())
                    });
                }
            }
        }
        Err(primary_err) => {
            return Err(primary_err)
                .with_context(|| format!("failed reading config file {}", primary_path.display()));
        }
    };
    let parsed: ConfigFile = serde_json::from_str(&raw)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;
    let config = parsed.into_tool_config()?;

    config.validate()?;
    Ok(config)
}

impl ConfigFile {
    fn into_tool_config(self) -> Result<ToolConfig> {
        match (self.clawform, self.claudeform) {
            (Some(cfg), None) => Ok(cfg),
            (None, Some(cfg)) => Ok(cfg),
            (Some(_), Some(_)) => bail!(
                ".clawform/config.json cannot define both 'clawform' and legacy 'claudeform' keys"
            ),
            (None, None) => {
                bail!(".clawform/config.json must define 'clawform' (or legacy 'claudeform')")
            }
        }
    }
}

impl ToolConfig {
    pub fn validate(&self) -> Result<()> {
        if self.providers.is_empty() {
            bail!(".clawform/config.json must define at least one provider");
        }

        for (name, provider) in &self.providers {
            parse_provider_kind(name, &provider.provider_type)?;
            if let Some(model) = &provider.default_model {
                if model.trim().is_empty() {
                    bail!("provider '{}' default_model cannot be empty", name);
                }
            }
        }

        Ok(())
    }

    pub fn resolve_default_provider(&self) -> Result<ResolvedProvider> {
        self.resolve_provider(None)
    }

    pub fn resolve_provider(&self, name: Option<&str>) -> Result<ResolvedProvider> {
        let (name, provider) = match name {
            Some(name) => {
                let provider = self
                    .providers
                    .get(name)
                    .with_context(|| unknown_provider_error_message(name, &self.providers))?;
                (name.to_string(), provider)
            }
            None => {
                let defaults = self
                    .providers
                    .iter()
                    .filter(|(_, p)| p.default)
                    .collect::<Vec<_>>();
                if defaults.len() != 1 {
                    bail!(
                        "exactly one provider must set default=true (found {})",
                        defaults.len()
                    );
                }
                let (name, provider) = defaults
                    .into_iter()
                    .next()
                    .context("no default provider found after validation")?;
                (name.clone(), provider)
            }
        };
        let provider_type = parse_provider_kind(&name, &provider.provider_type)?;

        Ok(ResolvedProvider {
            name,
            provider_type,
            default_model: provider.default_model.clone(),
        })
    }
}

fn parse_provider_kind(name: &str, raw: &str) -> Result<ProviderKind> {
    ProviderKind::parse(raw).ok_or_else(|| unsupported_provider_type_error(name, raw))
}

fn unsupported_provider_type_error(name: &str, raw: &str) -> anyhow::Error {
    anyhow!(
        "provider '{}' has unsupported type '{}' in v0 (supported: 'codex', 'claude')",
        name,
        raw
    )
}

fn unknown_provider_error_message(
    requested: &str,
    providers: &BTreeMap<String, ProviderConfig>,
) -> String {
    let available = providers.keys().cloned().collect::<Vec<_>>().join(", ");
    format!(
        "unknown provider '{}' (available providers: {})",
        requested, available
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_tool_config(s: &str) -> anyhow::Result<ToolConfig> {
        let parsed: ConfigFile = serde_json::from_str(s)?;
        parsed.into_tool_config()
    }

    #[test]
    fn validates_single_default_provider() {
        let cfg = parse_tool_config(
            r#"{
              "clawform": {
                "providers": {
                  "codex": {"type":"codex", "default": true}
                }
              }
            }"#,
        )
        .unwrap();

        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn fails_when_multiple_defaults() {
        let cfg = parse_tool_config(
            r#"{
              "clawform": {
                "providers": {
                  "a": {"type":"codex", "default": true},
                  "b": {"type":"codex", "default": true}
                }
              }
            }"#,
        )
        .unwrap();

        cfg.validate().unwrap();
        let err = cfg.resolve_default_provider().expect_err("must fail");
        assert!(format!("{:#}", err).contains("exactly one provider must set default=true"));
    }

    #[test]
    fn fails_when_no_default_for_default_resolution() {
        let cfg = parse_tool_config(
            r#"{
              "clawform": {
                "providers": {
                  "a": {"type":"codex", "default": false},
                  "b": {"type":"claude", "default": false}
                }
              }
            }"#,
        )
        .unwrap();

        cfg.validate().unwrap();
        let err = cfg.resolve_default_provider().expect_err("must fail");
        assert!(format!("{:#}", err).contains("exactly one provider must set default=true"));
    }

    #[test]
    fn fails_on_non_supported_provider_type() {
        let cfg = parse_tool_config(
            r#"{
              "clawform": {
                "providers": {
                  "x": {"type":"other", "default": true}
                }
              }
            }"#,
        )
        .unwrap();

        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_claude_provider_type() {
        let cfg = parse_tool_config(
            r#"{
              "clawform": {
                "providers": {
                  "claude": {"type":"claude", "default": true, "default_model":"sonnet"}
                }
              }
            }"#,
        )
        .unwrap();

        cfg.validate().unwrap();
        let provider = cfg.resolve_default_provider().unwrap();
        assert_eq!(provider.name, "claude");
        assert_eq!(provider.provider_type, ProviderKind::Claude);
        assert_eq!(provider.default_model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn resolves_typed_default_provider() {
        let cfg = parse_tool_config(
            r#"{
              "clawform": {
                "providers": {
                  "codex": {"type":"codex", "default": true, "default_model":"gpt-5-codex"}
                }
              }
            }"#,
        )
        .unwrap();

        cfg.validate().unwrap();
        let provider = cfg.resolve_default_provider().unwrap();
        assert_eq!(provider.name, "codex");
        assert_eq!(provider.provider_type, ProviderKind::Codex);
        assert_eq!(provider.default_model.as_deref(), Some("gpt-5-codex"));
    }

    #[test]
    fn resolves_named_provider() {
        let cfg = parse_tool_config(
            r#"{
              "clawform": {
                "providers": {
                  "codex_fast": {"type":"codex", "default": true, "default_model":"gpt-5-codex"},
                  "claude_safe": {"type":"claude", "default": false, "default_model":"sonnet"}
                }
              }
            }"#,
        )
        .unwrap();

        cfg.validate().unwrap();
        let provider = cfg.resolve_provider(Some("claude_safe")).unwrap();
        assert_eq!(provider.name, "claude_safe");
        assert_eq!(provider.provider_type, ProviderKind::Claude);
        assert_eq!(provider.default_model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn errors_on_unknown_named_provider() {
        let cfg = parse_tool_config(
            r#"{
              "clawform": {
                "providers": {
                  "codex_fast": {"type":"codex", "default": true},
                  "claude_safe": {"type":"claude", "default": false}
                }
              }
            }"#,
        )
        .unwrap();

        cfg.validate().unwrap();
        let err = cfg
            .resolve_provider(Some("missing"))
            .expect_err("must fail");
        let rendered = format!("{:#}", err);
        assert!(rendered.contains("unknown provider 'missing'"));
        assert!(rendered.contains("codex_fast"));
        assert!(rendered.contains("claude_safe"));
    }

    #[test]
    fn accepts_legacy_claudeform_key() {
        let cfg = parse_tool_config(
            r#"{
              "claudeform": {
                "providers": {
                  "codex": {"type":"codex", "default": true}
                }
              }
            }"#,
        )
        .unwrap();

        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn fails_when_both_current_and_legacy_keys_present() {
        let err = parse_tool_config(
            r#"{
              "clawform": {
                "providers": {
                  "codex_a": {"type":"codex", "default": true}
                }
              },
              "claudeform": {
                "providers": {
                  "codex_b": {"type":"codex", "default": true}
                }
              }
            }"#,
        )
        .expect_err("must fail");

        assert!(format!("{:#}", err).contains("cannot define both"));
    }

    #[test]
    fn loads_legacy_config_path_when_new_path_is_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let legacy_dir = tmp.path().join(".claudeform");
        std::fs::create_dir_all(&legacy_dir).expect("create legacy dir");
        std::fs::write(
            legacy_dir.join("config.json"),
            r#"{
              "claudeform": {
                "providers": {
                  "codex": {"type":"codex", "default": true}
                }
              }
            }"#,
        )
        .expect("write legacy config");

        let cfg = load_config(tmp.path()).expect("load legacy config");
        assert!(cfg.providers.contains_key("codex"));
    }
}
