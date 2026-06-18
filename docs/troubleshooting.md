# Troubleshooting

## Shell integration isn't changing directory

**Symptom:** `ji switch <name>` succeeds but the shell's `pwd` does not change.

**Cause:** The shell wrapper is not installed, or the current shell hasn't loaded it.

**Fix:**

```sh
ji config shell install
exec $SHELL   # or open a new terminal
```

See [shell-integration.md](shell-integration.md) for manual installation.

---

## "JI_DIRECTIVE_FILE not set — did you run `ji config shell install`?"

**Symptom:** `ji switch` prints this error.

**Cause:** `ji` was invoked without the shell wrapper active — either because the wrapper is not installed, or because the invocation bypassed the wrapper (e.g., running `command ji switch ...` or calling the binary by full path).

**Fix:** Install the wrapper and use `ji` (not `command ji`).

---

## "working copy is stale — another tool modified the repo"

**Symptom:** CLI prints `(ji)::stale ...` and exits 1; the TUI shows a stale-alert modal.

**Cause:** Another process (you, another agent, another editor) changed the repository state since the workspace's last snapshot.

**Fix:**

- **TUI:** press `u` on a stale workspace to open the update-stale dialog, inspect pending changes, and resolve. Or press `r` in the stale-alert modal to save all stale diffs and resolve.
- **CLI:** `jj workspace update-stale`.

Saved diffs, when the TUI writes them, land in `.ji/diffs/`.

---

## "no workspace matching '<target>'"

**Symptom:** `ji switch <target>` or a CLI source/target argument errors.

**Cause:** The target does not match any workspace name, change ID prefix, or bookmark (at-head or behind).

**Fix:** run `ji ls` to list workspaces and pick a valid name, change ID prefix, or bookmark.

---

## "ambiguous target '<target>': matches ..."

**Symptom:** `ji switch` errors with a list of candidates.

**Cause:** The target argument is a prefix that matches multiple workspaces (e.g., a 1-char change ID prefix that several workspaces share).

**Fix:** use a longer, unambiguous prefix, or the exact workspace name.

---

## Adaptive operation is unavailable

**Symptom:** `ji close <target> --method adaptive` or `ji transfer --method adaptive` fails, or the adaptive entry is missing from the TUI dialog.

**Cause:** The sync mode does not support adaptive resolution:

- `InSync` — nothing to do
- `TargetOnly` (close only) — source has no new work to move

**Fix:** pick a specific method, or let the sync mode be `SourceOnly` or `Diverged`.

See [operations.md](operations.md#sync-mode-detection).

---

## "repo changed externally, please retry" (or similar)

**Symptom:** A TUI operation fails immediately after confirmation.

**Cause:** The repo's operation head advanced between when ji computed the sync mode and when it tried to execute the operation — usually because another process committed in parallel.

**Fix:** retry the operation. ji will recompute the sync mode against the fresh repo state.

---

## Config warning modal appears on Create

**Symptom:** Pressing `n` in the TUI opens a `ConfigWarning` modal instead of the Create dialog.

**Cause:** `.config/ji.toml` has template warnings (unknown variables, malformed `{{...}}` delimiters).

**Fix:** edit the config file to fix the flagged template strings. See [configuration.md](configuration.md#template-validation) for the rules.

---

## Workspace directory not removed after close

**Symptom:** `ji close` succeeds but the workspace directory is still on disk.

**Cause:** `--delete-files` was not passed (CLI) or `k` was not toggled (TUI), or the directory was already missing.

**Fix:** pass `--delete-files` on the next close, or `rm -rf <path>` manually. `ji close` refuses to delete the directory if it cannot locate it.

---

## Background hooks outlive a closed workspace

**Symptom:** After closing a workspace, a server started by a `[post-start]` hook keeps running.

**Cause:** `[post-start]` hooks are fire-and-forget by design — ji does not track them and cannot stop them when the workspace is closed.

**Fix:** stop the process manually (e.g., `pkill -f`). If you need lifecycle-managed processes, run them under a supervisor such as `launchd`, `tmux`, or a project runner.

---

## Undo went too far

**Symptom:** Pressing `Z` in the TUI undid more than you intended.

**Fix:**

- Press `Y` to redo.
- Or open the op log pane (`o`), select the desired operation, and press `R` to restore to it.
- From the shell, `jj op log` and `jj op restore <id>` expose the same history.

**Note:** `Z` / `Y` only traverse actions that ji itself initiated — recorded in a persisted history (`.ji/action-history.json`) — not operations made outside ji (by another shell or another tool). If you want to undo something that wasn't made via ji, use the op log pane (`o`) or `jj op restore` directly.

---

## Create errors with "bookmark already exists"

**Symptom:** `ji new <bookmark>` fails because a bookmark with that name already exists elsewhere in the repo.

**Fix:** choose a different bookmark name, or delete the existing bookmark first (`jj bookmark delete <name>`). If you wanted to resume work on that bookmark instead of creating a new workspace, use `ji new <bookmark> --create-if-necessary`, which switches to an existing workspace when the bookmark name already matches one.

---

## Build errors

**Symptom:** `task build` or `task release` fails.

**Fixes to try, in order:**

1. `task clean && task build` — clears caches and rebuilds from scratch
2. `rustup update stable` — refresh the Rust toolchain
3. `task update` — refresh Rust toolchain and all dependencies

If the error persists, open an issue with the full output of `task build:clean`.
