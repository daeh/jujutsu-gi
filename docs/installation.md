# Installation

## Supported platform

macOS only. ji uses macOS-specific behavior (codesigning on install, clipboard access via `pbcopy`) and is not tested elsewhere.

## Prerequisites

- **[jj](https://jj-vcs.github.io) v0.40 or later** on `PATH`. Verify with `jj --version`.
- **Rust toolchain** (edition 2024). The repository's `rust-toolchain.toml` selects the stable channel (with `clippy` and `rustfmt`); `rustup` installs it automatically on first build.
- **[Task](https://taskfile.dev)** (recommended). Every build and maintenance command in this project is a Task target.

## Build and install

```sh
task release
```

This builds a universal (Apple Silicon + Intel) release binary, generates the `ji(1)` man page and shell completions, installs the binary to `~/.local/bin/ji` (codesigned with an ad-hoc signature) and the man page to `~/.local/share/man/man1/`, and installs the shell integration for fish and zsh. `~/.local/bin` must be on your `PATH`.

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

After installing the binary, install the shell wrapper so `ji` can change your shell's current directory when you switch workspaces:

```sh
ji config shell install
```

See [shell-integration.md](shell-integration.md) for details and for manual installation.

## Upgrading

Pull the latest source and re-run `task release`. The build is incremental. To force a clean rebuild, run `task release:clean` (equivalent to `task clean && task release`).

## Uninstall

Remove the shell integration first, then the binary:

```sh
ji config shell uninstall
rm ~/.local/bin/ji
```

`ji config shell uninstall` removes the wrapper files and the rc stanza that `install` added. Pass a shell name to target one shell; otherwise it uses `$SHELL`. See [shell-integration.md](shell-integration.md).
