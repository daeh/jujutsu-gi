//! `clap` definitions for the `ji` binary. Lives in the library crate so the
//! `xtask` man-page/completion generator can build the `clap::Command` tree
//! from the same source.

use clap::builder::styling::{AnsiColor, Color, Style, Styles};
use clap::{CommandFactory, Parser, Subcommand};

use crate::commands;

pub fn help_styles() -> Styles {
    let green = Style::new()
        .bold()
        .fg_color(Some(Color::Ansi(AnsiColor::Green)));
    let cyan_bold = Style::new()
        .bold()
        .fg_color(Some(Color::Ansi(AnsiColor::Cyan)));
    let cyan = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan)));

    Styles::styled()
        .header(green)
        .usage(green)
        .literal(cyan_bold)
        .placeholder(cyan)
        .error(
            Style::new()
                .bold()
                .fg_color(Some(Color::Ansi(AnsiColor::Red))),
        )
        .valid(green)
        .invalid(
            Style::new()
                .bold()
                .fg_color(Some(Color::Ansi(AnsiColor::Yellow))),
        )
}

#[derive(Parser)]
#[command(name = "ji", about = "Jujutsu workspace utilities")]
#[command(version)]
#[command(styles = help_styles())]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Switch to a workspace
    Switch {
        /// Workspace name, bookmark, or change ID prefix
        #[arg(add = crate::completion::workspace_completer())]
        target: String,
    },

    /// Create a new workspace
    New {
        /// Bookmark name for the workspace
        #[arg(add = crate::completion::new_bookmark_completer())]
        bookmark: String,

        /// Revision to branch from (default: @)
        #[arg(short, long)]
        revision: Option<String>,

        /// Workspace path (overrides workspace-path template)
        #[arg(short, long)]
        path: Option<String>,

        /// Description for the new workspace
        #[arg(short, long)]
        message: Option<String>,

        /// Create workspace if it doesn't exist, switch if it does
        #[arg(long)]
        create_if_necessary: bool,
    },

    /// Close a workspace into a target
    Close {
        /// Target workspace (positional unless --source is used)
        #[arg(add = crate::completion::workspace_completer())]
        target: Option<String>,

        /// Source workspace (defaults to current workspace)
        #[arg(long, add = crate::completion::workspace_completer())]
        source: Option<String>,

        /// Close method
        #[arg(long, value_enum, default_value = "adaptive")]
        method: commands::types::CloseMethod,

        /// Delete workspace files after close
        #[arg(long)]
        delete_files: bool,
    },

    /// Transfer changes between workspaces
    Transfer {
        /// Target workspace (positional unless --source is used)
        #[arg(add = crate::completion::workspace_completer())]
        target: Option<String>,

        /// Source workspace (defaults to current workspace)
        #[arg(long, add = crate::completion::workspace_completer())]
        source: Option<String>,

        /// Transfer method
        #[arg(long, value_enum, default_value = "adaptive")]
        method: commands::types::TransferMethod,
    },

    /// Sync current workspace with a target
    Sync {
        /// Target workspace (positional unless --source is used)
        #[arg(add = crate::completion::workspace_completer())]
        target: Option<String>,

        /// Source workspace (defaults to current workspace)
        #[arg(long, add = crate::completion::workspace_completer())]
        source: Option<String>,
    },

    /// List workspaces
    #[command(visible_alias = "list")]
    Ls,

    /// Generate project config template
    Init,

    /// Run configured hooks
    #[command(hide = true)]
    Hook,

    /// Manage user & project configs
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// Generate project config template (alias of `ji init`)
    Init,

    /// Shell integration
    Shell {
        #[command(subcommand)]
        command: ShellCommands,
    },
}

#[derive(Subcommand)]
pub enum ShellCommands {
    /// Print shell integration code
    Init {
        /// Shell to generate for (defaults to the active shell, falls back to $SHELL)
        shell: Option<String>,
    },

    /// Add shell integration to shell config
    Install {
        /// Shell to install for (defaults to the active shell, falls back to $SHELL)
        shell: Option<String>,

        /// Install for every shell with an existing config (skips shells with none)
        #[arg(long, conflicts_with = "shell")]
        all: bool,

        /// Print the diff that would be applied, don't write
        #[arg(long)]
        dry_run: bool,

        /// Install even if integration is detected elsewhere, or overwrite
        /// non-ji-managed files
        #[arg(long)]
        force: bool,

        /// Skip the confirmation prompt (assume yes)
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Remove shell integration
    Uninstall {
        /// Shell to uninstall for (defaults to the active shell, falls back to $SHELL)
        shell: Option<String>,

        /// Print the diff that would be applied, don't write
        #[arg(long)]
        dry_run: bool,

        /// Remove non-ji-managed files too
        #[arg(long)]
        force: bool,
    },

    /// Report shell integration state
    Status {
        /// Shell to inspect (defaults to the active shell, falls back to $SHELL)
        shell: Option<String>,
    },
}

/// `Cli::command()` adjusted for the completion tree: hide every non-positional
/// option on each (sub)command so a bare `<TAB>` offers only the positional
/// candidates (workspaces, bookmarks), not flags. clap_complete still surfaces
/// the hidden flags once the token starts with `-`. clap's built-in `--help` /
/// `--version` are added too late for `mut_args` to see, so we disable them and
/// re-add our own for the hide pass to catch. Only the completion tree changes;
/// `Cli::parse` is untouched. Passed to `CompleteEnv::with_factory`.
pub fn completion_command() -> clap::Command {
    use clap::{Arg, ArgAction};

    fn process(cmd: clap::Command, is_root: bool) -> clap::Command {
        // Re-add --help so the hide pass below can see it (clap's built-in is
        // added too late).
        let cmd = cmd.disable_help_flag(true).arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help("Print help"),
        );
        // --version exists only on the root command.
        let cmd = if is_root {
            cmd.disable_version_flag(true).arg(
                Arg::new("version")
                    .short('V')
                    .long("version")
                    .action(ArgAction::Version)
                    .help("Print version"),
            )
        } else {
            cmd
        };
        // Hide every non-positional (including the help/version we just re-added):
        // clap_complete drops hidden args when completing a positional, but still
        // offers them after `--`.
        let cmd = cmd.mut_args(|arg| {
            if arg.is_positional() || arg.is_hide_set() {
                arg
            } else {
                arg.hide(true)
            }
        });
        cmd.mut_subcommands(|sub| process(sub, false))
    }

    process(Cli::command(), true)
}
