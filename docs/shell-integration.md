# Shell integration

`ji switch <target>` and the TUI's switch action both change the current shell's working directory. Because `ji` runs as a child process, it cannot modify the parent shell's cwd directly. A shell wrapper bridges the gap.

## Mechanism

The wrapper function:

1. Creates an empty temp file (`mktemp`) — this is the **directive file**.
2. Exports its path in the `JI_DIRECTIVE_FILE` environment variable and runs the real `ji` binary.
3. If the file is non-empty after `ji` exits, sources it. ji will have written a `cd '<path>'` line whenever a command should change the shell's directory.
4. Removes the temp file.
5. Returns ji's original exit code.

## Install

```sh
ji config shell install            # detect the active shell
ji config shell install zsh        # explicit
ji config shell install bash
ji config shell install fish
ji config shell install --all      # every shell with an existing config
```

The installer is idempotent — re-running it leaves already-managed files and the rc stanza in place. On a terminal it previews the changes and prompts `[y/N/?]` before writing (`?` re-shows the diff); pass `--yes` to skip the prompt, or `--dry-run` to preview without writing. A non-interactive run (piped/CI) writes directly. `--all` configures every shell that already has a config and reports the rest as skipped (it never creates a config for a shell you don't use). `install` writes:

| Shell | What it writes |
|---|---|
| zsh  | wrapper → `~/.config/ji/init.zsh`; a sourcing stanza → `~/.zshrc` |
| bash | wrapper → `~/.config/ji/init.bash`; a sourcing stanza → `~/.bash_profile` (or `~/.bash_login` / `~/.profile`, whichever already exists) |
| fish | wrapper function → `~/.config/fish/functions/ji.fish` (autoloaded; no rc edit); completions → `~/.config/fish/completions/ji.fish` |

Paths shown assume the defaults: the wrapper directory follows `$XDG_CONFIG_HOME`, and zsh's rc file follows `$ZDOTDIR`.

After install, start a new shell or source the rc file.

If you run `ji switch`/`new`/`close` on a terminal and the wrapper isn't installed, `ji` offers to install it then and there (`[y/N/?]`, configuring the shell you're in plus any other shell with a config). A decline is remembered per shell; `ji config shell install` re-enables the offer.

## Bypass aliases

`ji config shell status` reports anything that prevents the wrapper from running. In zsh/bash a `ji` *alias* that points at the binary (e.g. `alias ji=/opt/homebrew/bin/ji` or `alias ji='command ji'`) shadows the wrapper function, so `ji` runs the bare binary and auto-cd silently won't happen; in fish, an `alias ji …` or a user `function ji` does the same. A differently-named alias that runs the binary (e.g. `alias j=/opt/homebrew/bin/ji`) doesn't shadow `ji`, but `j` won't change directory. `status` lists these so you can remove them. (A zsh/bash `ji() { … }` *function* is not detected.)

## Print without installing

```sh
ji config shell init               # detect the active shell
ji config shell init zsh           # explicit
```

Prints the wrapper source to stdout. Use this to inspect the wrapper, install it to a non-standard location, or add it to a dotfiles manager.

## Manual install

**zsh / bash** — add to `~/.zshrc` (zsh) or `~/.bash_profile` (bash):

```sh
if command -v ji >/dev/null 2>&1; then eval "$(command ji config shell init)"; fi
```

**fish** — write the output of `ji config shell init fish` to `~/.config/fish/functions/ji.fish`.

## Tab completion

Installing the integration also registers dynamic tab-completion for workspace and bookmark arguments. `ji switch <TAB>` — and `close`, `transfer`, `sync`, including their `--source` flag — completes live workspace targets; `ji new <TAB>` completes existing local bookmarks. Each candidate carries a relative last-modified time, and workspace candidates add a status marker: `(2h)` was touched two hours ago, `(2h)[*]` is the current workspace, `(1y)[x]` is orphaned (its directory is gone).

zsh and fish show the annotations, ordered most-recent-first (orphaned workspaces last); bash completes plain names. Candidates come from a single non-mutating `jj` query per `<TAB>` under a bounded timeout, so completion never snapshots the working copy or stalls the prompt.

## Supported shells

zsh, bash, fish. The shell is detected from the active shell — the one that invoked `ji`, by walking the process tree — falling back to `$SHELL` when it can't be identified; pass an explicit argument to override.

## Verification

With the wrapper installed, run:

```sh
ji switch <some-workspace>
pwd
```

`pwd` should print the workspace path. If it does not, the wrapper is not in effect — check that your shell has loaded the rc file, and see the troubleshooting entries in [troubleshooting.md](troubleshooting.md#shell-integration-isnt-changing-directory).

## Editing the fish function

The fish wrapper lives at `~/.config/fish/functions/ji.fish`, not an rc line. If you modify it, note that `ji config shell install fish` checks for an existing function containing `JI_DIRECTIVE_FILE` and skips the overwrite — it will not reapply a newer wrapper on top of your edits. Delete the file first if you want `install` to rewrite it.
