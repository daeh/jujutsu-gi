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
ji config shell install            # detect $SHELL
ji config shell install zsh        # explicit
ji config shell install bash
ji config shell install fish
```

The installer is idempotent — re-running it leaves already-managed files and the rc stanza in place. `install` writes:

| Shell | What it writes |
|---|---|
| zsh  | wrapper → `~/.config/ji/init.zsh`; a sourcing stanza → `~/.zshrc` |
| bash | wrapper → `~/.config/ji/init.bash`; a sourcing stanza → `~/.bash_profile` (or `~/.bash_login` / `~/.profile`, whichever already exists) |
| fish | wrapper function → `~/.config/fish/functions/ji.fish` (autoloaded; no rc edit) |

After install, start a new shell or source the rc file.

## Print without installing

```sh
ji config shell init               # detect $SHELL
ji config shell init zsh           # explicit
```

Prints the wrapper source to stdout. Use this to inspect the wrapper, install it to a non-standard location, or add it to a dotfiles manager.

## Manual install

**zsh / bash** — add to `~/.zshrc` (zsh) or `~/.bash_profile` (bash):

```sh
if command -v ji >/dev/null 2>&1; then eval "$(command ji config shell init)"; fi
```

**fish** — write the output of `ji config shell init fish` to `~/.config/fish/functions/ji.fish`.

## Supported shells

zsh, bash, fish. The shell is detected from `$SHELL`; pass an explicit argument to override.

## Verification

With the wrapper installed, run:

```sh
ji switch <some-workspace>
pwd
```

`pwd` should print the workspace path. If it does not, the wrapper is not in effect — check that your shell has loaded the rc file, and see the troubleshooting entries in [troubleshooting.md](troubleshooting.md#shell-integration-isnt-changing-directory).

## Editing the fish function

The fish wrapper lives at `~/.config/fish/functions/ji.fish`, not an rc line. If you modify it, note that `ji config shell install fish` checks for an existing function containing `JI_DIRECTIVE_FILE` and skips the overwrite — it will not reapply a newer wrapper on top of your edits. Delete the file first if you want `install` to rewrite it.
