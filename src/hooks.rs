use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

/// Template variables available in hook commands.
pub type HookVars = BTreeMap<String, String>;

/// Variables valid in `workspace-path` templates (knowable before workspace creation).
pub const PATH_VARS: &[&str] = &["home", "repo", "bookmark", "default_workspace_path"];

/// Variables valid in hook commands and file templates (available after workspace creation).
pub const HOOK_VARS: &[&str] = &[
    "home",
    "repo",
    "bookmark",
    "workspace_path",
    "default_workspace_path",
    "workspace_name",
    "change_id",
];

// ---------------------------------------------------------------------------
// Template expansion
// ---------------------------------------------------------------------------

pub fn expand(cmd: &str, vars: &HookVars) -> String {
    let mut result = cmd.to_string();
    for (key, value) in vars {
        result = result.replace(&format!("{{{{ {key} }}}}"), value);
    }
    result
}

/// Reverse-parse the `{{ bookmark }}` value from a workspace name using the
/// `workspace-path` template and known variables.
///
/// Takes the last path component of the template, substitutes all known
/// variables except `bookmark`, then matches the workspace name to extract
/// the bookmark portion.
pub fn derive_bookmark_from_ws_name(
    template: &str,
    repo_name: &str,
    ws_name: &str,
) -> Option<String> {
    // Extract the last path component (the directory name pattern).
    let name_pattern = template.rsplit('/').next()?;
    if !name_pattern.contains("{{ bookmark }}") {
        return None;
    }

    // Substitute known variables except bookmark.
    let pattern = name_pattern
        .replace("{{ repo }}", repo_name)
        .replace("{{ home }}", "")
        .replace("{{ default_workspace_path }}", "");

    // Split on {{ bookmark }} to get prefix and suffix.
    let (prefix, suffix) = pattern.split_once("{{ bookmark }}")?;

    // Match the workspace name against prefix..suffix to extract the bookmark.
    let rest = ws_name.strip_prefix(prefix)?;
    let bookmark = rest.strip_suffix(suffix)?;
    if bookmark.is_empty() {
        return None;
    }
    Some(bookmark.to_string())
}

// ---------------------------------------------------------------------------
// Template validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum WarningKind {
    /// Well-formed `{{ x }}` where x is not a known variable.
    UnknownVariable,
    /// Looks like a template variable but delimiters are wrong
    /// (e.g. `{{x}}`, `{{ x}}`, `{ x }`).
    MalformedDelimiters,
    /// `workspace-path` template lacks `{{ bookmark }}`, so every workspace
    /// resolves to the same path.
    MissingBookmark,
    /// `workspace-path` contains shell syntax (`~`, `${…}`) that won't be
    /// expanded because paths are passed as process arguments, not through a shell.
    ShellSyntax,
}

#[derive(Debug, Clone)]
pub struct TemplateWarning {
    /// Which config field produced this warning (e.g. "workspace-path", "pre-start/deps").
    pub context: String,
    /// The suspicious text found (e.g. "{{ bbb }}", "{{home}}").
    pub pattern: String,
    /// What kind of problem was detected.
    pub kind: WarningKind,
}

impl fmt::Display for TemplateWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            WarningKind::UnknownVariable => {
                write!(
                    f,
                    "unknown template variable '{}' in {}",
                    self.pattern, self.context
                )
            }
            WarningKind::MalformedDelimiters => {
                write!(
                    f,
                    "malformed template variable '{}' in {} (expected '{{{{ name }}}}')",
                    self.pattern, self.context
                )
            }
            WarningKind::MissingBookmark => {
                write!(
                    f,
                    "{} must contain {{{{ bookmark }}}} — each workspace needs a unique path",
                    self.context
                )
            }
            WarningKind::ShellSyntax => {
                write!(
                    f,
                    "{} contains '{}' which won't be expanded (use {{{{ home }}}} instead)",
                    self.context, self.pattern
                )
            }
        }
    }
}

/// Scan a template string for unrecognized or malformed variable references.
///
/// Returns an empty vec if everything looks clean.
pub fn validate_template(
    template: &str,
    context: &str,
    known_vars: &[&str],
) -> Vec<TemplateWarning> {
    let mut warnings = Vec::new();
    let bytes = template.as_bytes();
    let len = bytes.len();

    // Pass 1: double-brace patterns  {{ ... }}  or {{...}}
    let mut i = 0;
    while i + 1 < len {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find closing }}
            if let Some(close) = find_double_close(bytes, i + 2) {
                let inner = &template[i + 2..close];
                let full_pattern = &template[i..close + 2];
                let trimmed = inner.trim();

                if is_word(trimmed) {
                    if known_vars.contains(&trimmed) {
                        // Check if spacing is canonical: "{{ name }}"
                        let canonical = format!("{{{{ {trimmed} }}}}");
                        if full_pattern != canonical {
                            warnings.push(TemplateWarning {
                                context: context.to_string(),
                                pattern: full_pattern.to_string(),
                                kind: WarningKind::MalformedDelimiters,
                            });
                        }
                        // else: valid, skip
                    } else {
                        warnings.push(TemplateWarning {
                            context: context.to_string(),
                            pattern: full_pattern.to_string(),
                            kind: WarningKind::UnknownVariable,
                        });
                    }
                }
                i = close + 2;
            } else {
                i += 2;
            }
        } else {
            i += 1;
        }
    }

    // Pass 2: single-brace patterns  { word }  but not {{ (already handled)
    i = 0;
    while i < len {
        if bytes[i] == b'{'
            && (i + 1 >= len || bytes[i + 1] != b'{')
            && (i == 0 || bytes[i - 1] != b'{')
            && (i == 0 || bytes[i - 1] != b'$')
        {
            if let Some(close) = bytes[i + 1..].iter().position(|&b| b == b'}') {
                let close_abs = i + 1 + close;
                // Make sure it's not a double-close
                if close_abs + 1 >= len || bytes[close_abs + 1] != b'}' {
                    let inner = &template[i + 1..close_abs];
                    let trimmed = inner.trim();
                    if is_word(trimmed) {
                        let full_pattern = &template[i..close_abs + 1];
                        warnings.push(TemplateWarning {
                            context: context.to_string(),
                            pattern: full_pattern.to_string(),
                            kind: WarningKind::MalformedDelimiters,
                        });
                    }
                }
                i = close_abs + 1;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    warnings
}

/// Find the position of `}}` starting from `start`.
fn find_double_close(bytes: &[u8], start: usize) -> Option<usize> {
    let mut j = start;
    while j + 1 < bytes.len() {
        if bytes[j] == b'}' && bytes[j + 1] == b'}' {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Check if a string is a single word (alphanumeric + underscore, non-empty).
fn is_word(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Structural checks specific to the `workspace-path` template.
///
/// Unlike [`validate_template`] (which checks syntax / variable names), this
/// function checks semantic requirements:
/// - `{{ bookmark }}` must be present (uniqueness).
/// - Shell syntax (`~`, `${…}`) must not appear (won't be expanded).
pub fn validate_workspace_path_template(template: &str) -> Vec<TemplateWarning> {
    let ctx = "workspace-path";
    let mut warnings = Vec::new();

    // 1. {{ bookmark }} is required for uniqueness.
    if !template.contains("{{ bookmark }}") {
        warnings.push(TemplateWarning {
            context: ctx.to_string(),
            pattern: String::new(),
            kind: WarningKind::MissingBookmark,
        });
    }

    // 2. Leading tilde (shell home-directory expansion).
    if template == "~" || template.starts_with("~/") {
        warnings.push(TemplateWarning {
            context: ctx.to_string(),
            pattern: "~".to_string(),
            kind: WarningKind::ShellSyntax,
        });
    }

    // 3. ${…} shell variable references.
    let bytes = template.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'$' && bytes[i + 1] == b'{' {
            if let Some(close) = bytes[i + 2..].iter().position(|&b| b == b'}') {
                let full = &template[i..i + 2 + close + 1]; // ${…}
                warnings.push(TemplateWarning {
                    context: ctx.to_string(),
                    pattern: full.to_string(),
                    kind: WarningKind::ShellSyntax,
                });
                i += 2 + close + 1;
            } else {
                i += 2;
            }
        } else {
            i += 1;
        }
    }

    warnings
}

/// Format a "known variables" hint for error messages.
pub fn known_vars_hint(known_vars: &[&str]) -> String {
    known_vars.join(", ")
}

/// Run hooks sequentially. Fails on first non-zero exit.
pub fn run_blocking(
    label: &str,
    hooks: &BTreeMap<String, String>,
    vars: &HookVars,
    cwd: &Path,
    quiet: bool,
) -> Result<()> {
    for (name, cmd) in hooks {
        let cmd = expand(cmd, vars);
        if !quiet {
            eprintln!("(ji)::hook running {label}/{name}: {cmd}");
        }
        let start = std::time::Instant::now();
        let status = Command::new("sh")
            .args(["-c", &cmd])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("failed to run {label}/{name}"))?;
        crate::subprocess_log::log_hook(&format!("{label}/{name}"), &cmd, start.elapsed());
        if !status.success() {
            anyhow::bail!("{label}/{name} failed with {status}");
        }
    }
    Ok(())
}

/// Write file templates with expanded variables.
pub fn write_templates(
    templates: &BTreeMap<String, String>,
    vars: &HookVars,
    ws_root: &Path,
    quiet: bool,
) -> Result<()> {
    for (rel_path, content) in templates {
        let expanded = expand(content, vars);
        let dest = ws_root.join(rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory for {rel_path}"))?;
        }
        fs::write(&dest, expanded)
            .with_context(|| format!("failed to write template {rel_path}"))?;
        if !quiet {
            eprintln!("(ji)::template wrote {rel_path}");
        }
    }
    Ok(())
}

/// Spawn hooks in background. Does not wait for completion.
pub fn run_background(
    label: &str,
    hooks: &BTreeMap<String, String>,
    vars: &HookVars,
    cwd: &Path,
    quiet: bool,
) {
    for (name, cmd) in hooks {
        let cmd = expand(cmd, vars);
        if !quiet {
            eprintln!("(ji)::hook starting {label}/{name}: {cmd}");
        }
        let result = Command::new("sh")
            .args(["-c", &cmd])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        crate::subprocess_log::log_hook(
            &format!("{label}/{name}"),
            &cmd,
            std::time::Duration::ZERO,
        );
        if let Err(e) = result
            && !quiet
        {
            eprintln!("(ji)::hook failed to start {label}/{name}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_template_produces_no_warnings() {
        let w = validate_template("../{{ repo }}.{{ bookmark }}", "test", PATH_VARS);
        assert!(w.is_empty(), "expected no warnings, got: {w:?}");
    }

    #[test]
    fn unknown_variable_detected() {
        let w = validate_template("{{ bbb }}", "workspace-path", PATH_VARS);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].pattern, "{{ bbb }}");
        assert_eq!(w[0].kind, WarningKind::UnknownVariable);
        assert_eq!(w[0].context, "workspace-path");
    }

    #[test]
    fn malformed_no_spaces() {
        let w = validate_template("{{home}}", "workspace-path", PATH_VARS);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].pattern, "{{home}}");
        assert_eq!(w[0].kind, WarningKind::MalformedDelimiters);
    }

    #[test]
    fn malformed_asymmetric_spaces() {
        let w = validate_template("{{ home}}", "workspace-path", PATH_VARS);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].kind, WarningKind::MalformedDelimiters);

        let w = validate_template("{{home }}", "workspace-path", PATH_VARS);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].kind, WarningKind::MalformedDelimiters);
    }

    #[test]
    fn malformed_single_braces() {
        let w = validate_template("{home}", "workspace-path", PATH_VARS);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].pattern, "{home}");
        assert_eq!(w[0].kind, WarningKind::MalformedDelimiters);
    }

    #[test]
    fn single_braces_with_spaces() {
        let w = validate_template("{ home }", "workspace-path", PATH_VARS);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].kind, WarningKind::MalformedDelimiters);
    }

    #[test]
    fn unknown_with_malformed_delimiters() {
        // Unknown variable + wrong delimiters: still UnknownVariable since the name isn't known
        let w = validate_template("{{bbb}}", "workspace-path", PATH_VARS);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].kind, WarningKind::UnknownVariable);
    }

    #[test]
    fn multiple_issues_in_one_template() {
        let w = validate_template(
            "../{{ bbb }}/{{home}}/{{ repo }}",
            "workspace-path",
            PATH_VARS,
        );
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].pattern, "{{ bbb }}");
        assert_eq!(w[0].kind, WarningKind::UnknownVariable);
        assert_eq!(w[1].pattern, "{{home}}");
        assert_eq!(w[1].kind, WarningKind::MalformedDelimiters);
    }

    #[test]
    fn hook_vars_superset_of_path_vars() {
        for v in PATH_VARS {
            assert!(HOOK_VARS.contains(v), "{v} should be in HOOK_VARS");
        }
    }

    #[test]
    fn no_false_positives_on_plain_text() {
        let w = validate_template("/some/path/to/dir", "test", PATH_VARS);
        assert!(w.is_empty());
    }

    #[test]
    fn no_false_positives_on_json_braces() {
        let w = validate_template(r#"{"key": "value"}"#, "test", HOOK_VARS);
        assert!(w.is_empty());
    }

    #[test]
    fn no_false_positives_on_shell_vars() {
        let w = validate_template("echo ${HOME}/bin", "pre-start/test", HOOK_VARS);
        assert!(w.is_empty());
    }

    #[test]
    fn hook_context_allows_workspace_path() {
        let w = validate_template("{{ workspace_path }}", "pre-start/mcp", HOOK_VARS);
        assert!(w.is_empty());
    }

    #[test]
    fn path_context_rejects_workspace_path() {
        let w = validate_template("{{ workspace_path }}", "workspace-path", PATH_VARS);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].kind, WarningKind::UnknownVariable);
    }

    // -- validate_workspace_path_template tests --

    #[test]
    fn ws_template_with_bookmark_no_missing_warning() {
        let w = validate_workspace_path_template("../{{ repo }}.{{ bookmark }}");
        assert!(
            !w.iter().any(|w| w.kind == WarningKind::MissingBookmark),
            "expected no MissingBookmark, got: {w:?}"
        );
    }

    #[test]
    fn ws_template_without_bookmark_warns() {
        let w = validate_workspace_path_template("../{{ repo }}");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].kind, WarningKind::MissingBookmark);
    }

    #[test]
    fn ws_template_tilde_slash_warns() {
        let w = validate_workspace_path_template("~/workspaces/{{ bookmark }}");
        assert!(
            w.iter()
                .any(|w| w.kind == WarningKind::ShellSyntax && w.pattern == "~")
        );
    }

    #[test]
    fn ws_template_bare_tilde_warns() {
        let w = validate_workspace_path_template("~");
        // MissingBookmark + ShellSyntax
        assert!(
            w.iter()
                .any(|w| w.kind == WarningKind::ShellSyntax && w.pattern == "~")
        );
        assert!(w.iter().any(|w| w.kind == WarningKind::MissingBookmark));
    }

    #[test]
    fn ws_template_shell_var_warns() {
        let w = validate_workspace_path_template("${HOME}/ws/{{ bookmark }}");
        assert!(
            w.iter()
                .any(|w| w.kind == WarningKind::ShellSyntax && w.pattern == "${HOME}")
        );
    }

    #[test]
    fn ws_template_home_var_no_shell_warning() {
        let w = validate_workspace_path_template("{{ home }}/ws/{{ bookmark }}");
        assert!(
            !w.iter().any(|w| w.kind == WarningKind::ShellSyntax),
            "expected no ShellSyntax, got: {w:?}"
        );
    }

    #[test]
    fn ws_template_combined_issues() {
        let w = validate_workspace_path_template("~/ws/${USER}");
        assert!(w.iter().any(|w| w.kind == WarningKind::MissingBookmark));
        assert!(
            w.iter()
                .any(|w| w.kind == WarningKind::ShellSyntax && w.pattern == "~")
        );
        assert!(
            w.iter()
                .any(|w| w.kind == WarningKind::ShellSyntax && w.pattern == "${USER}")
        );
    }
}
