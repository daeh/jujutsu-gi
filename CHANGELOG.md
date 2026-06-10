# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-06-09

Initial public release.

### Added

- Interactive TUI workspace switcher with a live commit graph, one-key
  workspace operations, and an in-app operation-log pane.
- Non-interactive CLI exposing the same operations: `switch`, `new`, `close`,
  `transfer`, `sync`, `ls`, `init`.
- Graph-aware sync, transfer, and close that pick a strategy — fast-forward,
  merge, rebase, or squash — to fit how the workspaces relate, and show the
  exact `jj` command sequence before running it.
- Shell integration for zsh, bash, and fish (`ji config shell
  install|uninstall|status`), letting `ji` change the shell's working
  directory on workspace switch.
- Per-project configuration (`.config/ji.toml`): workspace-path templates,
  blocking pre-start and background post-start hooks, and per-workspace file
  templates.
- One-shot undo/redo of ji actions via ji-managed bulk operations in the
  `jj op log`.
- Man page and shell-completion generation (`cargo xtask gen-man`,
  `cargo xtask gen-completions`).
