use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use crate::jujutsu;

/// Find the workspace matching `target` and return its path.
///
/// Matches against workspace name, change ID prefix, and bookmarks.
/// Errors if no match or ambiguous.
pub fn switch(repo_root: &Path, target: &str) -> Result<PathBuf> {
    let workspaces = jujutsu::list_workspaces(repo_root)?;

    let matches: Vec<&jujutsu::Workspace> = workspaces
        .iter()
        .filter(|ws| {
            ws.name == target
                || ws.change_id.starts_with(target)
                || ws.bookmarks_at_head.iter().any(|(b, _)| b == target)
                || ws.bookmarks_behind.iter().any(|(b, _)| b == target)
        })
        .collect();

    match matches.len() {
        0 => bail!("no workspace matching '{target}'"),
        1 => Ok(matches[0].path.clone()),
        _ => {
            let names: Vec<&str> = matches.iter().map(|ws| ws.name.as_str()).collect();
            bail!("ambiguous target '{target}': matches {}", names.join(", "))
        }
    }
}
