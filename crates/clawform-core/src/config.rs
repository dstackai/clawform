use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    clawform: ToolConfig,
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

#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub name: String,
    pub provider_type: String,
    pub default_model: Option<String>,
}

pub fn load_config(workspace_root: &Path) -> Result<ToolConfig> {
    let path = workspace_root.join(".clawform/config.json");
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed reading config file {}", path.display()))?;
    let parsed: ConfigFile = serde_json::from_str(&raw)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;

    parsed.clawform.validate()?;
    Ok(parsed.clawform)
}

impl ToolConfig {
    pub fn validate(&self) -> Result<()> {
        if self.providers.is_empty() {
            bail!(".clawform/config.json must define at least one provider");
        }

        let defaults: Vec<_> = self
            .providers
            .iter()
            .filter(|(_, p)| p.default)
            .map(|(name, _)| name)
            .collect();

        if defaults.len() != 1 {
            bail!(
                "exactly one provider must set default=true (found {})",
                defaults.len()
            );
        }

        for (name, provider) in &self.providers {
            if provider.provider_type != "codex" {
                bail!(
                    "provider '{}' has unsupported type '{}' in v0 (only 'codex' is supported)",
                    name,
                    provider.provider_type
                );
            }
            if let Some(model) = &provider.default_model {
                if model.trim().is_empty() {
                    bail!("provider '{}' default_model cannot be empty", name);
                }
            }
        }

        Ok(())
    }

    pub fn resolve_default_provider(&self) -> Result<ResolvedProvider> {
        let (name, provider) = self
            .providers
            .iter()
            .find(|(_, p)| p.default)
            .context("no default provider found after validation")?;

        Ok(ResolvedProvider {
            name: name.clone(),
            provider_type: provider.provider_type.clone(),
            default_model: provider.default_model.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_tool_config(s: &str) -> anyhow::Result<ToolConfig> {
        let parsed: ConfigFile = serde_json::from_str(s)?;
        Ok(parsed.clawform)
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

        assert!(cfg.validate().is_err());
    }

    #[test]
    fn fails_on_non_codex_provider_type() {
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
}
