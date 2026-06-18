# Installation

## Supported platform

macOS only. ji uses macOS-specific behavior (codesigning on install, clipboard access via `pbcopy`) and is not tested elsewhere.

## Prerequisites

- **[jj](https://jj-vcs.github.io) v0.42 or later** on `PATH`. Verify with `jj --version`.
- **Rust toolchain** (edition 2024). The repository's `rust-toolchain.toml` selects the stable channel (with `clippy` and `rustfmt`); `rustup` installs it automatically on first build.
- **[Task](https://taskfile.dev)** (recommended). Every build and maintenance command in this project is a Task target.

## Build and install

```sh
task release
```

This builds a universal (Apple Silicon + Intel) release binary, generates the `ji(1)` man page and shell completions, installs the binary to `~/.local/bin/ji` (codesigned with an ad-hoc signature) and the man page to `~/.local/share/man/man1/`, and installs the shell integration for the active shell (falling back to `$SHELL`). `~/.local/bin` must be on your `PATH`.

Verify the install:

```sh
ji --version
```

Manual alternative (without Task):

```sh
cargo build --release
cp target/release/ji ~/.local/bin/
codesign -f -s - ~/.local/bin/ji
```

## Shell integration

`task release` already installs the wrapper for the active shell (falling back to `$SHELL`). If you used the manual build above, or want integration for an additional shell, run:

```sh
ji config shell install [SHELL]
```

With no argument it targets the active shell (falling back to `$SHELL`); pass `zsh`, `bash`, or `fish` to install another. The wrapper lets `ji` change your shell's current directory when you switch workspaces. See [shell-integration.md](shell-integration.md) for details and for manual installation.

## Upgrading

Pull the latest source and re-run `task release`. The build is incremental. To force a clean rebuild, run `task release:clean` (equivalent to `task clean && task release`).

## Uninstall

Remove the shell integration first, then the binary:

```sh
ji config shell uninstall
rm ~/.local/bin/ji
```

`ji config shell uninstall` removes the wrapper files and the rc stanza that `install` added. Pass a shell name to target one shell; otherwise it uses the active shell (falling back to `$SHELL`). See [shell-integration.md](shell-integration.md).
