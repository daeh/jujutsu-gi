# CLI reference

Every ji subcommand, its flags, and its behavior.

> ji commands must be run from inside a [jj](https://jj-vcs.github.io) repository. Most subcommands expect to find a `.jj/` directory somewhere in the ancestor path.

## Terms used on this page

A few terms appear throughout. Full definitions live in [operations.md](operations.md#terminology); brief versions:

- **Default workspace** — the workspace at the repository root, referred to as `default@`. It serves the role of the `main`/`master` branch in git. ji uses it as the default target for `transfer`/`sync` (and for `close` only when `--source` is given) when no positional target is supplied. A bare non-disposal `ji close` requires an explicit target.
- **Effective head** — the tip of a workspace's line of work, skipping over ji-inserted empty "trivial" commits used to keep workspaces from sharing `@`.
- **Adaptive** — a meta-method for close/transfer that inspects the graph relationship between source and target and picks a concrete method (fast-forward, merge, etc.).
- **Disposal methods** — close methods that do not require a target: `detach` and `abandon`.

## Commands

- [`ji`](#ji) — launch the TUI
- [`ji switch`](#ji-switch) — switch to a workspace
- [`ji new`](#ji-new) — create a workspace
- [`ji close`](#ji-close) — close a workspace into a target
- [`ji transfer`](#ji-transfer) — transfer changes between workspaces
- [`ji sync`](#ji-sync) — sync two workspaces
- [`ji ls`](#ji-ls--ji-list) — list workspaces
- [`ji init`](#ji-init) — generate project config
- [`ji config`](#ji-config) — configuration subcommands

---

## `ji`

```
ji
```

Launches the interactive TUI workspace switcher. See [tui.md](tui.md) for the full TUI reference.

Errors if the current directory is not inside a jj repository.

---

## `ji switch`

```
ji switch <target>
```

Switch to an existing workspace. For each workspace, ji checks whether `<target>`:

- exactly matches the workspace name, or
- is a prefix of the workspace's change ID, or
- matches any bookmark on the workspace (at the head or behind it)

All workspaces that satisfy any of those conditions are collected. If exactly one matches, ji switches to it. If zero match, ji errors with `no workspace matching '<target>'`. If more than one matches, ji errors with the ambiguous candidate list.

On success, ji writes a `cd <path>` directive for the [shell wrapper](shell-integration.md), which moves the parent shell into the target workspace directory.

---

## `ji new`

```
ji new <bookmark> [--revision <REVSET>] [--path <PATH>] [--message <MSG>] [--create-if-necessary]
```

Create a new workspace and switch to it.

### Arguments

- **`<bookmark>`** — required. The bookmark name for the new workspace. This name drives:
  - the `{{ bookmark }}` variable in the `workspace-path` template
  - a bookmark ji creates pointing at the new workspace's `@`
  - forward slashes are replaced with hyphens when used in paths

### Flags

| Flag | Type | Default | Description |
|---|---|---|---|
| `-r`, `--revision <REVSET>` | [jj revset](https://docs.jj-vcs.dev/latest/revsets/) resolving to one revision | `@` | Revision the new workspace branches from |
| `-p`, `--path <PATH>` | string | — | Override the `workspace-path` template; path is relative to the repo root |
| `-m`, `--message <MSG>` | string | — | Description for the new workspace's working-copy commit |
| `--create-if-necessary` | flag | off | If a workspace already matches `<bookmark>`, switch to it instead of erroring |

### Branching behavior

If the revision passed to `--revision` is a head commit with real content ("non-trivial"), ji inserts a new empty commit on the source workspace before branching so that the source and the new workspace do not share the same `@`. Branching from an empty (trivial) head skips that step-forward.

### Side effects

After branching, ji:

1. Creates a jj bookmark named `<bookmark>` pointing at the new workspace's `@`
2. Writes any files defined in `[templates]` ([see configuration](configuration.md))
3. Runs `[pre-start]` hooks sequentially, blocking on each
4. Spawns `[post-start]` hooks in the background
5. Writes a `cd` directive for the shell wrapper

If `ji-author` is set in the config, the new revision's author is rewritten via `jj metaedit --author`.

---

## `ji close`

```
ji close [target] [--source <NAME>] [--method <METHOD>] [--delete-files]
```

Close a workspace — integrating its work into a target, or (for the `detach`/`abandon` disposal methods) discarding it — and then forgetting it.

### Arguments and flags

| Argument / Flag | Required | Default | Description |
|---|---|---|---|
| `[target]` | yes, for non-disposal methods (unless `--source` is given) | `default@`, only with `--source` | Target workspace name (positional) |
| `--source <NAME>` | no | current workspace (by cwd) | Workspace to close |
| `--method <METHOD>` | no | `adaptive` | Close method (see table below) |
| `--delete-files` | no | off | Remove the source workspace directory after close |

> **The `default` workspace can't be closed.** `ji close` refuses to close the default workspace (whether as the cwd source or via `--source default`), mirroring the TUI — closing it would remove the repo root.

### Methods

| Method | Target required | Description |
|---|---|---|
| `adaptive` | yes | Picks `fast-forward` or `merge` based on sync state — see [operations.md](operations.md#close-adaptive) |
| `merge` | yes | Creates a merge commit whose parents are the effective heads of both workspaces |
| `squash-merge` | yes | Squashes the source chain into one commit, then merges |
| `fast-forward` | yes | Fast-forwards the target to the source's effective head |
| `detach` | no | Forgets the workspace, keeping meaningful and explicitly bookmarked revisions while removing unreferenced trivial heads |
| `abandon` | no | Forgets the workspace and abandons all its revisions |

For full diagrams and the jj command sequences ji runs for each method, see [operations.md](operations.md).

> **Bookmark handling.** When you close a workspace from the CLI, bookmarks on the source workspace are left alone — ji does not advance or delete them. The singular bookmark (the one whose name matches the workspace's `{{ bookmark }}` value) is still managed automatically. If you want to advance or delete the other bookmarks as part of a close, use the TUI close dialog (`x`), which exposes a per-close bookmark-action toggle. See [operations.md](operations.md#bookmark-actions-on-close).

### Example

```sh
# Close the current workspace into the default workspace with adaptive merge
ji close default

# Squash-merge a specific workspace into another, deleting its files
ji close main --source feature-login --method squash-merge --delete-files

# Abandon a workspace and its revisions
ji close --method abandon
```

---

## `ji transfer`

```
ji transfer [target] [--source <NAME>] [--method <METHOD>]
```

Transfer changes between two workspaces. Both workspaces remain open afterward — this is the difference from `ji close`.

### Arguments and flags

| Argument / Flag | Required | Default | Description |
|---|---|---|---|
| `[target]` | no | `default@` | Target workspace name |
| `--source <NAME>` | no | current workspace (by cwd) | Source workspace |
| `--method <METHOD>` | no | `adaptive` | Transfer method (see table below) |

### Methods

| Method | Description |
|---|---|
| `adaptive` | Picks `fast-forward-target`, `fast-forward-source`, or `merge` based on sync state |
| `merge` | Merge commit with both effective heads as parents; both workspaces step forward past the merge |
| `fast-forward-target` | Fast-forward the target to the source's effective head |
| `fast-forward-source` | Fast-forward the source to the target's effective head |
| `rebase` | Rebase the source's unique chain onto the target — produces linear history |
| `merge-squash` | Squash the source's chain into one commit, then merge with target |

Full diagrams and jj command sequences: [operations.md](operations.md).

---

## `ji sync`

```
ji sync [target] [--source <NAME>]
```

Converge two workspaces. If one side is behind the other on the graph, ji fast-forwards it. If both have new work, ji creates a merge commit (with both effective heads as parents) that becomes the new tip of both workspaces.

Unlike `ji close` and `ji transfer`, `ji sync` does **not** take a `--method` flag — the strategy is always decided from the sync state.

If the two workspaces are already in sync, ji prints `(ji)::sync already in sync` and exits 0.

Defaults: `target` is `default@`; `--source` is the workspace at the current working directory.

---

## `ji ls` / `ji list`

```
ji ls
```

Prints one line per workspace:

```
@ default              abcd1234   main, (feature)   (no description)
+ feature-login        efgh5678   login              wip: login UI
```

Columns:

- **Gutter** — `@` = current workspace, `+` = other
- **Name** — workspace name
- **Change ID** — 8-character prefix of the working-copy commit's change ID
- **Bookmarks** — bookmarks at the workspace's head, followed by bookmarks behind the head in parentheses
- **Description** — first line of the working-copy commit's description, truncated to 60 chars

---

## `ji init`

```
ji init
```

Generates `.config/ji.toml` in the repo root from the annotated template. Errors if the file already exists.

See [configuration.md](configuration.md) for the full field reference.

---

## `ji config`

```
ji config init                       # same as ji init
ji config shell init [SHELL]         # print shell integration to stdout
ji config shell install [SHELL]      # install shell integration
ji config shell uninstall [SHELL]    # remove shell integration
ji config shell status [SHELL]       # report integration state
```

`[SHELL]` is one of `zsh`, `bash`, `fish`; it defaults to the active shell — the one that invoked `ji`, found by walking the process tree — falling back to `$SHELL` when no shell can be identified.

### `ji config init`

Alias for `ji init`.

### `ji config shell init [SHELL]`

Prints the shell wrapper function to stdout.

### `ji config shell install [SHELL]`

Installs the shell wrapper. Idempotent. On a terminal it previews the changes and prompts `[y/N/?]` before writing (`?` re-shows the diff); a non-interactive run (piped/CI) writes directly. With no `[SHELL]` it targets the active shell; `--all` instead installs for **every shell with an existing config** (skipping shells with none, reported per shell). Flags: `--all`, `--dry-run` (print the diff without writing), `--force` (install even if integration is detected elsewhere, or overwrite non-ji-managed files), `--yes`/`-y` (skip the prompt). `--all` and an explicit `[SHELL]` are mutually exclusive. For fish, when a dynamic completion is already provided on `$fish_complete_path` (e.g. Homebrew's `vendor_completions.d/ji.fish`), install relies on it and removes a redundant/stale user-directory copy instead of writing one that would shadow it. See [shell-integration.md](shell-integration.md) for target locations and manual install.

When `ji switch`/`new`/`close` can't change the directory because the wrapper isn't installed, on a terminal it offers to install integration right there (`[y/N/?]`); declining is remembered per shell.

### `ji config shell uninstall [SHELL]`

Removes the wrapper files and rc stanza that `install` added. Flags: `--dry-run`, `--force` (also remove non-ji-managed files).

### `ji config shell status [SHELL]`

Reports install state, drift, any cross-file integration hits, and any bypass aliases — a `ji` alias (or fish `function ji`) that shadows the wrapper, or a differently-named alias that runs the `ji` binary directly. For fish it also reports whether the user completion is shadowing (or being shadowed by) another `ji.fish` on `$fish_complete_path`, and how to resolve it.

---

## Source and target resolution

`close`, `transfer`, and `sync` share the same source/target resolution rules:

1. **Source**
   - With `--source <NAME>`: looked up by exact workspace name.
   - Without `--source`: the workspace whose path matches the current working directory. Errors if cwd is not inside a workspace.
2. **Target**
   - With a positional `target`: looked up by exact workspace name.
   - Without a positional `target`: the default workspace (`default@`) — the workspace at the repository root, which serves the role of the `main`/`master` branch in git. Errors if it can't be found. **Exception:** for `close`, this fallback applies only when `--source` is given; a bare non-disposal `ji close` (no `target`, no `--source`) errors and asks for a target.

## Stale working copy detection

Before a mutating operation, ji detects whether the working copy is stale (another process modified the repo). On a stale error, ji prints:

```
(ji)::stale working copy is stale — another tool modified the repo.
Fix with: jj workspace update-stale
```

and exits 1. See [troubleshooting.md](troubleshooting.md).
