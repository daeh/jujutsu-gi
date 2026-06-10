use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::hooks::{self, TemplateWarning};

const CONFIG_TEMPLATE: &str = include_str!("ji.template.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(rename = "workspace-path", default = "default_workspace_path")]
    pub workspace_path: String,
    /// Override for `{{ repo }}` template variable. Falls back to directory name if unset/empty.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(rename = "pre-start", default)]
    pub pre_start: BTreeMap<String, String>,
    #[serde(rename = "post-start", default)]
    pub post_start: BTreeMap<String, String>,
    /// File templates: key = relative path, value = template content.
    #[serde(default)]
    pub templates: BTreeMap<String, String>,
    /// Custom jj template for the displayed graph pane (passed to `jj log --template`).
    #[serde(rename = "log-template", default)]
    pub log_template: Option<String>,
    /// Override the author on every revision created by ji via `jj metaedit --author`.
    #[serde(rename = "ji-author", default)]
    pub ji_author: Option<String>,
    /// Template validation warnings collected at config load time.
    #[serde(skip)]
    pub warnings: Vec<TemplateWarning>,
}

fn default_workspace_path() -> String {
    "../{{ repo }}.{{ bookmark }}".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace_path: default_workspace_path(),
            repo: None,
            pre_start: BTreeMap::new(),
            post_start: BTreeMap::new(),
            templates: BTreeMap::new(),
            log_template: None,
            ji_author: None,
            warnings: Vec::new(),
        }
    }
}

/// Resolve the `{{ repo }}` variable: use the config override if non-empty,
/// otherwise fall back to the repository directory name.
pub fn resolve_repo_name(config: &Config, repo_root: &Path) -> String {
    if let Some(ref name) = config.repo {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub fn load_config(repo_root: &Path) -> Result<Config> {
    let config_path = repo_root.join(".config/ji.toml");
    if !config_path.exists() {
        return Ok(Config::default());
    }
    let contents = std::fs::read_to_string(&config_path)?;
    let mut config: Config = toml::from_str(&contents)?;

    // Validate all template strings for unrecognized or malformed variables.
    let mut warnings = Vec::new();
    warnings.extend(hooks::validate_template(
        &config.workspace_path,
        "workspace-path",
        hooks::PATH_VARS,
    ));
    warnings.extend(hooks::validate_workspace_path_template(
        &config.workspace_path,
    ));
    let sections: &[(&str, &BTreeMap<String, String>)] = &[
        ("pre-start", &config.pre_start),
        ("post-start", &config.post_start),
        ("templates", &config.templates),
    ];
    for (section, map) in sections {
        for (key, value) in *map {
            warnings.extend(hooks::validate_template(
                value,
                &format!("{section}/{key}"),
                hooks::HOOK_VARS,
            ));
        }
    }
    config.warnings = warnings;

    Ok(config)
}

pub fn create_config(repo_root: &Path) -> Result<()> {
    let config_dir = repo_root.join(".config");
    let config_path = config_dir.join("ji.toml");

    if config_path.exists() {
        anyhow::bail!("{} already exists", config_path.display());
    }

    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create {}", config_dir.display()))?;
    std::fs::write(&config_path, CONFIG_TEMPLATE)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    eprintln!("(ji)::config created {}", config_path.display());
    Ok(())
}
