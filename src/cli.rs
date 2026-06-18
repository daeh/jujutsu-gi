//! `clap` definitions for the `ji` binary. Lives in the library crate so the
//! `xtask` man-page/completion generator can build the `clap::Command` tree
//! from the same source.

use clap::builder::styling::{AnsiColor, Color, Style, Styles};
use clap::{Parser, Subcommand};

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
        target: String,
    },

    /// Create a new workspace
    New {
        /// Bookmark name for the workspace
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
        target: Option<String>,

        /// Source workspace (defaults to current workspace)
        #[arg(long)]
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
        target: Option<String>,

        /// Source workspace (defaults to current workspace)
        #[arg(long)]
        source: Option<String>,

        /// Transfer method
        #[arg(long, value_enum, default_value = "adaptive")]
        method: commands::types::TransferMethod,
    },

    /// Sync current workspace with a target
    Sync {
        /// Target workspace (positional unless --source is used)
        target: Option<String>,

        /// Source workspace (defaults to current workspace)
        #[arg(long)]
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

        /// Print the diff that would be applied, don't write
        #[arg(long)]
        dry_run: bool,

        /// Install even if integration is detected elsewhere, or overwrite
        /// non-ji-managed files
        #[arg(long)]
        force: bool,
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
