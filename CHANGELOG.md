# Changelog


## [0.1.3] - 2026-06-25

### Added

- Reason-aware shell-integration notes: when `ji switch`/`new`/`close` can't change the shell's directory (the wrapper isn't active), `ji` prints why and how to enable auto-cd, replacing the raw `JI_DIRECTIVE_FILE not set` error.
A `ji switch` that can't `cd` exits non-zero; `ji new` and a `close` that kept your directory exit zero.
Closing the workspace you're standing in *and removing its files* prints a `current directory was removed — run: cd <root>` escape and exits non-zero.
- `ji config shell install` previews its edits and prompts `[y/N/?]` before writing; `--yes`/`-y` skips it (a non-interactive run auto-proceeds).
- `ji close` refuses to close the `default` workspace, matching the TUI.

### Changed

- Homebrew caveats and docs no longer use the "Homebrew won't edit your startup files" framing; the install one-liner now chains `ji config shell install`.

## 0.1.2

### Added

- Homebrew installation path.

## 0.1.1

### Changed

- `ji config shell` now detects the active shell from the parent process (via libproc), stopping at the nearest recognized shell ancestor, instead of the `$SHELL` login shell. It falls back to `$SHELL` only when no shell can be identified. `init`, `install`, and `uninstall` error clearly when the active shell is recognized but unsupported; `status` instead reports an unsupported active shell non-fatally (alongside `$SHELL`).
- Internal refactoring only, with no change to existing behavior: TUI key dispatch now delegates to each dialog's own `handle_key`, single-line text fields share a common `LineEditor`, and the jj-subprocess snapshot policy was consolidated and documented.

### Fixed

- Harden guards for handling of pending working-copy edits made between opening a dialog for a mutating operation (`sync`, `close`, and `transfer`) and executing the command. Previously, a workspace's uncommitted working copy changes could be stranded in a dangling head node. While this resulted in a vocal warning, the new guards should handle the edge cases more robustly.

## 0.1.0

Initial public release.

## [0.1.2] - 2026-06-23

### Changed

- publish: harden Homebrew release flow (adversarial-review fixes)
- publish: add Homebrew tap install path (daeh/jujutsu-gi/ji)
- update publish pipeline

## [0.1.1] - 2026-06-12

### Changed

- bump version
- clean comments
- tui: delegate BookmarksDialog-internal keys to BookmarksDialog::handle_key
- tui: delegate CreateDialog-internal keys to CreateDialog::handle_key
- tui: delegate CloseDialog-internal keys to CloseDialog::handle_key
- tui: extract handle_key mode arms into per-mode methods (pure motion)
- tui: extract shared LineEditor for single-line text fields
- operations: name the workspace triple as WsRef
- record parsing: one records() helper for DELIM_RECORD-framed output
- surplus removal: delete dead code across jj layer, TUI, commands
- snapshot policy: taxonomy + maintained census, doc reconciliation
- snapshot consolidation: probe-layer trim, dead policy deletion, broad fail-loud execution freshness
- freshness gates: close dialog-open baseline race, gate the staleness poll
- add work in progress screen
- update depts
- freshness gates for mutating operations (sync/close/transfer)
- reorder new dialog fields
- publish: port pipeline from zsh to fish

