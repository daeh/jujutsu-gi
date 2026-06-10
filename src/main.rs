// Modules are declared in the library crate (src/lib.rs) and re-exported
// here so the binary and the xtask helper share one compilation. Declaring
// them in both crates would mint two distinct copies of each type (a
// bin-crate `Config` is not a lib-crate `Config`), so keep a single source.
use anyhow::{Context, bail};
use clap::{CommandFactory, Parser};
use std::path::{Path, PathBuf};

use ji::cli::{Cli, Commands, ConfigCommands, ShellCommands};
use ji::{commands, config, jujutsu, operations, shell, subprocess_log, text_utils, tui};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Ok(root) = jujutsu::current_workspace_root() {
        subprocess_log::init(&root);
    }

    let result = match cli.command {
        None => tui::run(),

        Some(Commands::Switch { target }) => tui::switch(&target),

        Some(Commands::New {
            bookmark,
            revision,
            path,
            message,
            create_if_necessary,
        }) => {
            if create_if_necessary {
                tui::create_or_switch(
                    &bookmark,
                    revision.as_deref(),
                    path.as_deref(),
                    message.as_deref(),
                )
            } else {
                tui::create(
                    &bookmark,
                    revision.as_deref(),
                    path.as_deref(),
                    message.as_deref(),
                )
            }
        }

        Some(Commands::Close {
            target,
            source,
            method,
            delete_files,
        }) => {
            // Validate: target required for non-disposal methods.
            let needs_target = !matches!(
                method,
                commands::types::CloseMethod::Detach | commands::types::CloseMethod::Abandon
            );
            if needs_target && target.is_none() && source.is_none() {
                bail!("target workspace is required for --method {method:?}");
            }

            let repo_root = jujutsu::workspace_root()?;
            let cfg = config::load_config(&repo_root)?;
            let repo_name = config::resolve_repo_name(&cfg, &repo_root);
            let workspaces = jujutsu::list_workspaces(&repo_root)?;

            let (src_name, src_path, tgt_name, tgt_path) = resolve_src_tgt(
                &repo_root,
                &workspaces,
                source.as_deref(),
                target.as_deref(),
            )?;

            let ws_paths: Vec<(String, PathBuf)> = workspaces
                .iter()
                .filter(|w| !w.path.as_os_str().is_empty() && w.path.exists())
                .map(|w| (w.name.clone(), w.path.clone()))
                .collect();

            // Gather revisions for the source workspace (needed for abandon).
            let src_ws = workspaces.iter().find(|w| w.name == src_name);
            let revisions = src_ws.map(|w| w.revisions.clone()).unwrap_or_default();

            let params = commands::close::CloseParams {
                repo_root: &repo_root,
                source_name: &src_name,
                source_path: &src_path,
                target_name: &tgt_name,
                target_path: &tgt_path,
                target_change_id: &src_ws
                    .and_then(|_| workspaces.iter().find(|w| w.name == tgt_name))
                    .map(|w| w.change_id.clone())
                    .unwrap_or_default(),
                method,
                delete_files,
                bookmark_action: commands::types::BookmarkAction::NoAction,
                bookmarks: Vec::new(),
                revisions: &revisions,
                workspace_path_template: &cfg.workspace_path,
                repo_name: &repo_name,
                author: cfg.ji_author.as_deref(),
                all_ws_paths: &ws_paths,
            };

            let result = commands::close::close(&params)?;

            if !result.stale_warnings.is_empty() {
                eprintln!("(ji)::stale: {}", result.stale_warnings.join(", "));
            }
            if let Some(remove_path) = result.pending_remove_path {
                eprintln!("(ji)::removing {}", remove_path.display());
                let _ = std::fs::remove_dir_all(&remove_path);
            }

            // If we closed the current workspace, switch to repo root.
            let current_root = jujutsu::current_workspace_root()?;
            if src_path == current_root {
                shell::write_directive_cd(&repo_root)?;
            }

            Ok(())
        }

        Some(Commands::Transfer {
            target,
            source,
            method,
        }) => {
            let repo_root = jujutsu::workspace_root()?;
            let cfg = config::load_config(&repo_root)?;
            let repo_name = config::resolve_repo_name(&cfg, &repo_root);
            let workspaces = jujutsu::list_workspaces(&repo_root)?;

            let (src_name, src_path, tgt_name, tgt_path) = resolve_src_tgt(
                &repo_root,
                &workspaces,
                source.as_deref(),
                target.as_deref(),
            )?;

            let ws_paths: Vec<(String, PathBuf)> = workspaces
                .iter()
                .filter(|w| !w.path.as_os_str().is_empty() && w.path.exists())
                .map(|w| (w.name.clone(), w.path.clone()))
                .collect();

            let params = commands::transfer::TransferParams {
                repo_root: &repo_root,
                source_name: &src_name,
                source_path: &src_path,
                target_name: &tgt_name,
                target_path: &tgt_path,
                method,
                workspace_path_template: &cfg.workspace_path,
                repo_name: &repo_name,
                author: cfg.ji_author.as_deref(),
                all_ws_paths: &ws_paths,
            };

            let result = commands::transfer::transfer(&params)?;

            if !result.stale_warnings.is_empty() {
                eprintln!("(ji)::stale: {}", result.stale_warnings.join(", "));
            }

            Ok(())
        }

        Some(Commands::Sync { target, source }) => {
            let repo_root = jujutsu::workspace_root()?;
            let cfg = config::load_config(&repo_root)?;
            let repo_name = config::resolve_repo_name(&cfg, &repo_root);
            let workspaces = jujutsu::list_workspaces(&repo_root)?;

            let (src_name, src_path, tgt_name, tgt_path) = resolve_src_tgt(
                &repo_root,
                &workspaces,
                source.as_deref(),
                target.as_deref(),
            )?;

            let outcome = commands::sync::sync(
                &repo_root,
                &src_name,
                &src_path,
                &tgt_name,
                &tgt_path,
                &cfg.workspace_path,
                &repo_name,
                cfg.ji_author.as_deref(),
            )?;

            match outcome {
                operations::SyncOutcome::AlreadyInSync => {
                    eprintln!("(ji)::sync already in sync");
                }
                operations::SyncOutcome::Done { warnings } => {
                    for w in &warnings {
                        eprintln!("(ji)::sync warning: {w}");
                    }
                }
            }

            Ok(())
        }

        Some(Commands::Ls) => cmd_ls(),
        Some(Commands::Init) => {
            let ws_root = jujutsu::current_workspace_root()?;
            config::create_config(&ws_root)
        }
        Some(Commands::Hook) => {
            eprintln!("(ji)::hook not yet implemented");
            Ok(())
        }
        Some(Commands::Config { command }) => match command {
            ConfigCommands::Init => {
                let ws_root = jujutsu::current_workspace_root()?;
                config::create_config(&ws_root)
            }
            ConfigCommands::Shell {
                command: ShellCommands::Init { shell: sh },
            } => {
                let sh = sh.map_or_else(shell::detect_shell, Ok)?;
                let mut cmd = Cli::command();
                shell::print_init(&sh, &mut cmd)
            }
            ConfigCommands::Shell {
                command:
                    ShellCommands::Install {
                        shell: sh,
                        dry_run,
                        force,
                    },
            } => {
                let sh = sh.map_or_else(shell::detect_shell, Ok)?;
                let mut cmd = Cli::command();
                let env = shell::ShellEnv::from_process_env()?;
                let opts = shell::InstallOpts { dry_run, force };
                shell::install(&env, &sh, &mut cmd, opts)
            }
            ConfigCommands::Shell {
                command:
                    ShellCommands::Uninstall {
                        shell: sh,
                        dry_run,
                        force,
                    },
            } => {
                let sh = sh.map_or_else(shell::detect_shell, Ok)?;
                let env = shell::ShellEnv::from_process_env()?;
                let opts = shell::UninstallOpts { dry_run, force };
                shell::uninstall(&env, &sh, opts)
            }
            ConfigCommands::Shell {
                command: ShellCommands::Status { shell: sh },
            } => {
                let sh = sh.map_or_else(shell::detect_shell, Ok)?;
                let mut cmd = Cli::command();
                let env = shell::ShellEnv::from_process_env()?;
                shell::status(&env, &sh, &mut cmd)
            }
        },
    };

    if let Err(ref e) = result
        && jujutsu::is_stale_error(e)
    {
        eprintln!(
            "(ji)::stale working copy is stale — another tool modified the repo.\n\
             Fix with: jj workspace update-stale"
        );
        std::process::exit(1);
    }
    result
}

/// Resolve source and target workspace names/paths from CLI arguments.
///
/// - Without `--source`: source = current workspace (cwd), target = positional.
/// - With `--source`: both looked up by name from workspace list.
fn resolve_src_tgt(
    repo_root: &Path,
    workspaces: &[jujutsu::Workspace],
    source: Option<&str>,
    target: Option<&str>,
) -> anyhow::Result<(String, PathBuf, String, PathBuf)> {
    let find_ws = |name: &str| -> anyhow::Result<(String, PathBuf)> {
        let ws = workspaces
            .iter()
            .find(|w| w.name == name)
            .with_context(|| format!("workspace '{name}' not found"))?;
        Ok((ws.name.clone(), ws.path.clone()))
    };

    let (src_name, src_path) = if let Some(src) = source {
        find_ws(src)?
    } else {
        // Source is the current workspace.
        let current_root = jujutsu::current_workspace_root()?;
        let src_ws = workspaces
            .iter()
            .find(|w| w.path == current_root)
            .context("current directory is not a jj workspace")?;
        (src_ws.name.clone(), src_ws.path.clone())
    };

    let (tgt_name, tgt_path) = if let Some(tgt) = target {
        find_ws(tgt)?
    } else {
        // No target specified — use the repo root's default workspace.
        let default_ws = workspaces
            .iter()
            .find(|w| w.path == *repo_root)
            .context("no target workspace specified and no default workspace found")?;
        (default_ws.name.clone(), default_ws.path.clone())
    };

    Ok((src_name, src_path, tgt_name, tgt_path))
}

fn cmd_ls() -> anyhow::Result<()> {
    let repo_root = jujutsu::workspace_root()?;
    let current_root = jujutsu::current_workspace_root()?;
    let mut workspaces = jujutsu::list_workspaces(&repo_root)?;
    for ws in &mut workspaces {
        ws.is_current = ws.path == current_root;
    }

    for ws in &workspaces {
        let gutter = if ws.is_current { "@" } else { "+" };
        let short_id = if ws.change_id.len() > 8 {
            &ws.change_id[..8]
        } else {
            &ws.change_id
        };

        let mut bm_parts: Vec<String> = Vec::new();
        for (bm, _) in &ws.bookmarks_at_head {
            bm_parts.push(bm.clone());
        }
        for (bm, _) in &ws.bookmarks_behind {
            bm_parts.push(format!("({bm})"));
        }
        let bookmarks = bm_parts.join(", ");

        let desc = text_utils::truncate_end(&ws.description, 60);

        println!(
            "{gutter} {:<20} {:<10} {:<20} {}",
            ws.name, short_id, bookmarks, desc
        );
    }
    Ok(())
}
