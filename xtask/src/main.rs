//! Build-side helper for the `ji` crate. Currently supports:
//!   - `gen-man`         — write the ji(1) man page to target/man/ji.1
//!   - `gen-completions` — write dynamic shell completion registrations to target/completions/
//!
//! Run via `cargo xtask <sub>` (alias in .cargo/config.toml) or
//! `cargo run -p xtask -- <sub>`.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::CommandFactory;
use ji::cli::Cli;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let sub = args
        .next()
        .context("usage: cargo xtask <gen-man|gen-completions>")?;
    match sub.as_str() {
        "gen-man" => gen_man(),
        "gen-completions" => gen_completions(),
        other => bail!("unknown xtask subcommand: {other}"),
    }
}

fn out_dir(sub: &str) -> Result<PathBuf> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // xtask/ → workspace root
    p.push("target");
    p.push(sub);
    fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))?;
    Ok(p)
}

fn gen_man() -> Result<()> {
    let dir = out_dir("man")?;
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd);
    let mut buf: Vec<u8> = Vec::new();
    man.render(&mut buf).context("rendering man page")?;
    let path = dir.join("ji.1");
    fs::write(&path, &buf).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn gen_completions() -> Result<()> {
    let dir = out_dir("completions")?;
    // Dynamic registrations shared with `ji config shell install` (via
    // ji::shell::packaged_completion), so packaged completions are dynamic, not
    // stale static. Filenames match the Homebrew formula's install lines.
    for (shell, file) in [("bash", "ji.bash"), ("zsh", "_ji"), ("fish", "ji.fish")] {
        let body = ji::shell::packaged_completion(shell)
            .with_context(|| format!("generating {shell} completion"))?;
        let path = dir.join(file);
        fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}
