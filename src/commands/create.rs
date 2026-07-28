use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};

use crate::{config, finder_xattrs, hooks, jujutsu, operations};

use super::types::CreateResult;

/// Owned-data carrier for a deferred `create` invocation (see
/// `PendingHandoff::Create`). `Config` is cloned out of `App.config` at
/// deferral time so the drain block doesn't need to borrow `App`.
pub struct CreateParamsOwned {
    pub repo_root: PathBuf,
    pub config: config::Config,
    pub repo_name: String,
    pub bookmark: String,
    pub revision: String,
    pub source_ws_path: PathBuf,
    pub ws_path: PathBuf,
    pub msg: String,
}

/// Execute workspace creation with hooks.
///
/// When `quiet` is true (TUI mode), hook errors are silently ignored.
/// When `quiet` is false (CLI mode), hook errors propagate.
#[allow(clippy::too_many_arguments)]
pub fn create(
    repo_root: &Path,
    config: &config::Config,
    repo_name: &str,
    bookmark: &str,
    revision: &str,
    source_ws_path: &Path,
    ws_path: &Path,
    msg: &str,
    quiet: bool,
) -> Result<CreateResult> {
    // Finder-metadata capture from the source workspace before anything
    // materializes. The best-effort entry snapshot makes `jj file list`
    // (the capture's enumeration) see not-yet-snapshotted files, e.g. a
    // Finder alias just dropped on the source disk; capture reads the disk
    // regardless, so a failed snapshot only shrinks the tracked list.
    let _ = jujutsu::snapshot_ws(source_ws_path);
    let source_capture = [("source".to_string(), source_ws_path.to_path_buf())];
    let xattr_guard =
        finder_xattrs::XattrGuard::capture(&source_capture, config.preserve_finder_xattrs);

    operations::create_workspace(
        repo_root,
        source_ws_path,
        revision,
        ws_path,
        msg,
        config.ji_author.as_deref(),
    )?;

    // Resolve the legitimate stale state created in the source workspace by
    // the step-forward mutation made via --ignore-working-copy.
    let _ = jujutsu::update_workspace_stale(source_ws_path);

    let ws_name = ws_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // Restore Finder metadata before templates/hooks run: pre-start hooks
    // must see intact aliases. The source pass heals any source file the
    // update-stale above rewrote; the new-workspace pass scans everything
    // that was just materialized.
    let mut warnings = xattr_guard.restore(&source_capture);
    warnings.extend(xattr_guard.restore_new_workspace(&ws_name, ws_path));

    if quiet {
        let _ = jujutsu::create_bookmark(repo_root, &ws_name, bookmark);
    } else {
        jujutsu::create_bookmark(repo_root, &ws_name, bookmark).context("create bookmark")?;
    }

    if let Ok(vars) = jujutsu::hook_vars(repo_root, ws_path, &ws_name, bookmark, repo_name) {
        let write_result = hooks::write_templates(&config.templates, &vars, ws_path, quiet);
        if !quiet {
            write_result.context("write templates")?;
        }
        let pre_result = hooks::run_blocking("pre-start", &config.pre_start, &vars, ws_path, quiet);
        if !quiet {
            pre_result.context("pre-start hook")?;
        }
        hooks::run_background("post-start", &config.post_start, &vars, ws_path, quiet);
    }

    Ok(CreateResult {
        workspace_name: ws_name,
        workspace_path: ws_path.to_path_buf(),
        warnings,
    })
}

/// Resolve the workspace path from CLI arguments or config template.
pub fn resolve_workspace_path(
    repo_root: &Path,
    config: &config::Config,
    repo_name: &str,
    bookmark: &str,
    path_override: Option<&str>,
) -> Result<PathBuf> {
    let ws_path_str = match path_override {
        Some(p) => {
            let abs = if Path::new(p).is_absolute() {
                PathBuf::from(p)
            } else {
                std::env::current_dir()
                    .context("failed to get current directory")?
                    .join(p)
            };
            abs.to_string_lossy().to_string()
        }
        None => {
            let vars = jujutsu::path_vars(repo_root, bookmark, repo_name);
            hooks::expand(&config.workspace_path, &vars)
        }
    };
    let resolved = repo_root.join(&ws_path_str);
    validate_resolved_path(
        &resolved,
        repo_root,
        match path_override {
            Some(_) => None,
            None => Some(&config.workspace_path),
        },
    )?;
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

/// Normalize a path by resolving `.` and `..` components lexically.
///
/// - `Normal` components after `..` are popped (standard resolution).
/// - `..` past `RootDir` or `Prefix` is a no-op (matches OS behavior: `/..` = `/`).
/// - `..` with nothing to pop is preserved (relative paths).
fn normalize_path(path: &Path) -> PathBuf {
    let mut components: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => match components.last() {
                Some(Component::Normal(_)) => {
                    components.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {} // no-op
                _ => components.push(component),
            },
            Component::CurDir => {}
            c => components.push(c),
        }
    }
    components.iter().collect()
}

/// Compare two paths, handling symlinks when both exist.
fn paths_equivalent(a: &Path, b: &Path) -> bool {
    if let (Ok(ca), Ok(cb)) = (a.canonicalize(), b.canonicalize()) {
        return ca == cb;
    }
    normalize_path(a) == normalize_path(b)
}

/// Validate a resolved workspace path before creation.
///
/// Pass `template` as `Some(raw_template)` when the path came from template
/// expansion (enforces `{{ bookmark }}`). Pass `None` when the user provided
/// an explicit path override (e.g. `--path`).
///
/// The `exists()` check is for better error messages only — `jj workspace add`
/// is the authoritative guard (TOCTOU is accepted).
pub(crate) fn validate_resolved_path(
    resolved: &Path,
    repo_root: &Path,
    template: Option<&str>,
) -> Result<()> {
    // 1. Missing {{ bookmark }} enforcement (template paths only).
    if let Some(t) = template
        && !t.contains("{{ bookmark }}")
    {
        anyhow::bail!(
            "workspace-path must contain {{{{ bookmark }}}} — \
             each workspace needs a unique path; use --path to override per invocation"
        );
    }

    // 2. Self-overwrite of default workspace.
    if paths_equivalent(resolved, repo_root) {
        anyhow::bail!(
            "workspace-path resolves to the default workspace root ({}) — \
             the template must produce a different path",
            repo_root.display()
        );
    }

    // 3. Path already exists.
    if resolved.exists() {
        anyhow::bail!("workspace path already exists: {}", resolved.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // -- normalize_path --

    #[test]
    fn normalize_resolves_parent() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn normalize_resolves_curdir() {
        assert_eq!(
            normalize_path(Path::new("/a/b/./c")),
            PathBuf::from("/a/b/c")
        );
    }

    #[test]
    fn normalize_noop_clean_path() {
        assert_eq!(normalize_path(Path::new("/a/b/c")), PathBuf::from("/a/b/c"));
    }

    #[test]
    fn normalize_relative_underflow_preserves_dotdot() {
        assert_eq!(
            normalize_path(Path::new("a/../../b")),
            PathBuf::from("../b")
        );
    }

    #[test]
    fn normalize_absolute_underflow_noop_past_root() {
        // /a/../../b -> pop a, then .. at root is no-op -> /b
        assert_eq!(normalize_path(Path::new("/a/../../b")), PathBuf::from("/b"));
    }

    #[test]
    fn normalize_root_dotdot_noop() {
        assert_eq!(normalize_path(Path::new("/../b")), PathBuf::from("/b"));
    }

    // -- validate_resolved_path --

    #[test]
    fn rejects_self_overwrite_via_dotdot() {
        let repo = PathBuf::from("/a/b/repo");
        let resolved = repo.join("../repo");
        // Pass None for template to isolate the self-overwrite check.
        let err = validate_resolved_path(&resolved, &repo, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("default workspace root"), "got: {err}");
    }

    #[test]
    fn rejects_self_overwrite_via_dot() {
        let repo = PathBuf::from("/a/b/repo");
        let resolved = repo.join(".");
        let err = validate_resolved_path(&resolved, &repo, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("default workspace root"), "got: {err}");
    }

    #[test]
    fn rejects_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let existing = dir.path().join("existing");
        std::fs::create_dir_all(&existing).unwrap();
        let err = validate_resolved_path(&existing, &repo, Some("../{{ bookmark }}"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "got: {err}");
    }

    #[test]
    fn rejects_missing_bookmark_from_template() {
        let repo = PathBuf::from("/a/b/repo");
        let resolved = PathBuf::from("/somewhere/else");
        let err = validate_resolved_path(&resolved, &repo, Some("../{{ repo }}"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("{{ bookmark }}"), "got: {err}");
    }

    #[test]
    fn accepts_missing_bookmark_when_override() {
        let repo = PathBuf::from("/a/b/repo");
        let resolved = PathBuf::from("/somewhere/unique");
        assert!(validate_resolved_path(&resolved, &repo, None).is_ok());
    }

    #[test]
    fn accepts_valid_path() {
        let repo = PathBuf::from("/a/b/repo");
        let resolved = PathBuf::from("/a/b/repo.feature");
        assert!(
            validate_resolved_path(&resolved, &repo, Some("../{{ repo }}.{{ bookmark }}")).is_ok()
        );
    }

    #[test]
    fn rejects_path_override_to_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        // --path override pointing to an existing directory
        let err = validate_resolved_path(dir.path(), &repo, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "got: {err}");
    }
}
