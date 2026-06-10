use anyhow::{Context, Result};
use std::path::Path;

use crate::jujutsu;

/// Check whether a revision range is "effectively linear" — the path from
/// `lca` to `src_eff` is closed (no non-empty forks escape the chain).
///
/// Diamonds (fork + rejoin) are permitted. Dangling branches — non-empty
/// children of chain members that are not themselves in the chain — are not.
///
/// Returns `true` when the range is squashable into a single commit.
pub fn is_effectively_linear(repo: &Path, lca: &str, src_eff: &str) -> Result<bool> {
    // Non-empty children of chain members (excluding src_eff's children)
    // that are NOT in the chain. Any such revision is a dangling fork.
    let revset =
        format!("(children(({lca}..{src_eff}) ~ {src_eff}) ~ ({lca}..{src_eff})) ~ empty()");
    let output = jujutsu::run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &revset,
            "--template",
            r#"change_id ++ "\n""#,
        ],
    )
    .context("check effective linearity")?;
    Ok(output.trim().is_empty())
}
