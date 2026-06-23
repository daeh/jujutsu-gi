# ji — Jujutsu workspace manager

`ji` is a convenience utility for managing [Jujutsu (jj)](https://jj-vcs.github.io) workspaces. (_Workspaces_ are the jj equivalent of git _worktrees_.)

`ji` creates, switches, syncs, merges, and cleans up workspaces. It includes:
- An interactive TUI workspace switcher with a live commit graph and one-key workspace operations
- A non-interactive CLI exposing the same core workspace operations for scripts and agentic workflows
- Per-project configuration: workspace-path templates, pre/post-start hooks, and file templates
- Graph-aware operations that adapt to how the workspaces relate — auto-selecting fast-forward or merge, with rebase, squash, and other methods available explicitly
- One-shot undo/redo of `ji` actions, via `ji`-managed bulk operations in the `jj op log`

Currently, only macOS is fully supported, but cross-platform support is on the roadmap.

> [!NOTE]
> `ji` is a lightweight wrapper (a *gi*) for jujutsu. `ji` is a contraction of ***j***(j-g)***i***.


## A handrail for learning jj

Every `ji` operation is a short sequence of jj commands. The TUI close/transfer dialog shows the jj command sequence it will run *before* you confirm (and the dialog's `c` key copies them to the clipboard). If you are still building intuition for jj's primitives, `ji` is a useful way to watch what fast-forward, merge, rebase, and squash look like as jj command sequences on real repository state.

## Requirements

- [jj](https://jj-vcs.github.io) v0.42.0 or later

## Install

```sh
brew tap daeh/jujutsu-gi
brew install ji
```

This builds `ji` from source (via Homebrew's `rust`) and installs `jj` as a dependency. macOS only. Enable the optional shell integration (see below), then verify in a new terminal:

```sh
ji --version
```

<details>
<summary>Install without Homebrew</summary>

Build and install the release binary directly:

```sh
cargo build --release && cp target/release/ji ~/.local/bin/ && codesign -f -s - ~/.local/bin/ji
```

Or install with cargo from the repository:

```sh
cargo install --git https://github.com/daeh/jujutsu-gi ji
```

Make sure `~/.local/bin` (or `~/.cargo/bin`) is on your `PATH`.
</details>

## Shell integration

```sh
ji config shell install
```

This installs a wrapper function for zsh, bash, or fish (detected from the shell you ran it in). For zsh and bash it adds a sourcing line to your shell startup; for fish it installs an autoloaded function (no startup-file edit). The wrapper lets `ji` change the current shell's working directory when you switch workspaces — a child process cannot do that on its own.

See [`docs/shell-integration.md`](docs/shell-integration.md) for the files it touches, the mechanism, and manual installation.

## First use

```sh
cd /path/to/your/jj-repo/
ji init                    # generate .config/ji.toml
ji                         # launch the TUI
```

The TUI shows a workspace list on the left and the commit graph on the right. Press `Enter` to switch to a workspace, `n` to create a new one, `?` for the in-app help, and `q` to quit.

## CLI

```
ji                               # launch the TUI
ji switch <target>               # switch to a workspace (by name, bookmark, or change ID)
ji new <bookmark>                # create a new workspace
ji close [target]                # close a workspace into a target
ji transfer [target]             # transfer changes between workspaces (both stay open)
ji sync [target]                 # sync two workspaces
ji ls                            # list workspaces
ji init                          # generate .config/ji.toml
ji config shell install [SHELL]  # install shell integration
ji config shell init    [SHELL]  # print shell integration to stdout
```

Every command accepts `--help`. Full flag reference and behavior: [`docs/cli.md`](docs/cli.md).

### Flag summaries

```
ji new <bookmark> [--revision <REV>] [--path <PATH>] [--message <MSG>] [--create-if-necessary]

ji close [target] [--source <NAME>] [--delete-files]
                  [--method {adaptive, merge, squash-merge, fast-forward, detach, abandon}]

ji transfer [target] [--source <NAME>]
                     [--method {adaptive, merge, fast-forward-target, fast-forward-source,
                                merge-abandon-old, rebase, merge-squash}]

ji sync [target] [--source <NAME>]
```

`ji sync` has no `--method` flag — it always resolves the two workspaces with whichever strategy fits the sync state (fast-forward if one side is behind, merge if diverged).

`ji close` needs a target for the non-disposal methods (`adaptive`, `merge`, `squash-merge`, `fast-forward`): pass a positional `target`, or use `--source <NAME>` (which closes that source into `default@`). A bare `ji close` errors. The disposal methods `detach` and `abandon` take no target.

Operations, diagrams, and when to use each method: [`docs/operations.md`](docs/operations.md).

## TUI keybindings

| Key | Action |
|---|---|
| `↑` `↓` | Navigate the workspace list (or graph, if focused) |
| `→` `←` | Step through a workspace's revision chain |
| `Enter` | Switch to the selected workspace |
| `Tab` | Toggle focus between workspace list and commit graph |
| `n` | New workspace |
| `x` | Close workspace |
| `s` | Sync with another workspace |
| `t` | Transfer changes between workspaces |
| `v` | Pick a revision to split |
| `b` | Manage bookmarks |
| `c` | Copy workspace info to clipboard |
| `i` | Toggle description / changed-files view |
| `r` | Cycle sort mode (log order → alphabetical → last modified) |
| `u` | Update a stale workspace |
| `o` | Open the jj operation log pane |
| `Z` / `Y` | Undo / redo ji actions |
| `?` / `/` | Toggle the help pane |
| `q` | Quit |

Full TUI reference including dialog keybindings: [`docs/tui.md`](docs/tui.md).

## Configuration

Project config lives at `.config/ji.toml` in the repository root (checked into version control). Generate a starter with `ji init`, then uncomment the sections you need.

```toml
# Where new workspaces are created (relative to the repo root).
workspace-path = "../{{ repo }}.{{ bookmark }}"

# Blocking setup hooks run after workspace creation, in key order.
[pre-start]
deps = "uv sync"

# Background hooks spawned after pre-start completes.
[post-start]
server = "npm run dev"

# Files generated per workspace.
[templates]
".envrc" = 'export PROJECT_ROOT="{{ workspace_path }}"'
```

Full field reference, template variables, and hook semantics: [`docs/configuration.md`](docs/configuration.md).

## Documentation

| Page | What it covers |
|---|---|
| [docs/installation.md](docs/installation.md) | Prerequisites, build, install, upgrade |
| [docs/shell-integration.md](docs/shell-integration.md) | The shell wrapper and how to install it |
| [docs/configuration.md](docs/configuration.md) | Full `.config/ji.toml` reference |
| [docs/cli.md](docs/cli.md) | Every subcommand and flag |
| [docs/tui.md](docs/tui.md) | TUI layout, keybindings, dialogs, modes |
| [docs/operations.md](docs/operations.md) | Sync, transfer, close — diagrams and command sequences |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Problem / solution pairs |

## License

MIT
