//! Binary-level integration tests for `ji`'s parent-shell cd behavior.
//!
//! Each test runs the real `ji` binary (`CARGO_BIN_EXE_ji`) in a temp jj repo
//! with an isolated shell env (HOME/XDG_CONFIG_HOME/ZDOTDIR/SHELL, null stdin,
//! and a controlled `JI_DIRECTIVE_FILE`), so a developer's real installed
//! wrapper can't affect the result. These assert exit status / target path /
//! quiet stderr / rescue wording — not the exact diagnosed reason (the
//! process-tree walk sees the real ancestor shell), which is covered by the
//! pure `cd_reason` unit tests in `src/shell.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

fn jj(dir: &Path, args: &[&str]) {
    let out = Command::new("jj")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run jj");
    assert!(
        out.status.success(),
        "jj {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Repo {
    _tmp: TempDir,
    home: PathBuf,
    default_ws: PathBuf,
    feature_ws: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let default_ws = home.join("repo");
        std::fs::create_dir_all(&default_ws).unwrap();
        jj(&default_ws, &["git", "init"]);
        jj(
            &default_ws,
            &["config", "set", "--repo", "user.name", "Test"],
        );
        jj(
            &default_ws,
            &["config", "set", "--repo", "user.email", "t@t.com"],
        );
        std::fs::write(default_ws.join("README"), "hi").unwrap();
        jj(&default_ws, &["describe", "-m", "init"]);
        // Second workspace named "feature" (jj creates the directory), with a
        // distinct change so an adaptive close into default has something to do.
        let feature_ws = home.join("feature");
        jj(
            &default_ws,
            &[
                "workspace",
                "add",
                "--name",
                "feature",
                feature_ws.to_str().unwrap(),
            ],
        );
        std::fs::write(feature_ws.join("feat.txt"), "x").unwrap();
        jj(&feature_ws, &["describe", "-m", "feature work"]);
        Self {
            _tmp: tmp,
            home,
            default_ws,
            feature_ws,
        }
    }

    /// Run `ji` from `cwd` with isolated env. `directive` controls
    /// `JI_DIRECTIVE_FILE`: `None` = unset, `Some(path)` = set (wrapper active),
    /// `Some("")` = empty.
    fn run(&self, cwd: &Path, args: &[&str], directive: Option<&str>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_ji"));
        c.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null()) // non-TTY → install never prompts/hangs
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("ZDOTDIR", &self.home)
            .env("SHELL", "/bin/zsh");
        match directive {
            None => {
                c.env_remove("JI_DIRECTIVE_FILE");
            }
            Some(v) => {
                c.env("JI_DIRECTIVE_FILE", v);
            }
        }
        c.output().expect("run ji")
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn switch_unwrapped_exits_nonzero_with_clean_cd_note() {
    let r = Repo::new();
    let o = r.run(&r.default_ws, &["switch", "feature"], None);
    let e = stderr(&o);
    assert!(!o.status.success(), "expected non-zero; stderr: {e}");
    assert!(e.contains("(ji)::cd"), "missing cd note: {e}");
    assert!(
        e.contains("feature"),
        "hint should name the target path: {e}"
    );
    assert!(
        !e.contains("Error:"),
        "should be a clean exit, not an anyhow dump: {e}"
    );
}

#[test]
fn switch_wrapped_writes_cd_directive() {
    let r = Repo::new();
    let directive = r.home.join("directive");
    let o = r.run(
        &r.default_ws,
        &["switch", "feature"],
        Some(directive.to_str().unwrap()),
    );
    assert!(o.status.success(), "expected 0; stderr: {}", stderr(&o));
    let written = std::fs::read_to_string(&directive).unwrap_or_default();
    assert!(
        written.starts_with("cd '"),
        "expected a cd directive, got: {written:?}"
    );
    assert!(
        written.contains("feature"),
        "directive should target feature: {written:?}"
    );
}

#[test]
fn new_unwrapped_exits_zero_with_hint() {
    let r = Repo::new();
    let o = r.run(
        &r.default_ws,
        &[
            "new",
            "scratch",
            "--path",
            r.home.join("scratch").to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "new should exit 0; stderr: {}",
        stderr(&o)
    );
    assert!(
        stderr(&o).contains("(ji)::cd"),
        "expected a cd hint: {}",
        stderr(&o)
    );
}

#[test]
fn empty_directive_on_switch_is_unavailable_nonzero() {
    let r = Repo::new();
    let o = r.run(&r.default_ws, &["switch", "feature"], Some(""));
    assert!(
        !o.status.success(),
        "empty JI_DIRECTIVE_FILE must not be a silent success; stderr: {}",
        stderr(&o)
    );
    assert!(stderr(&o).contains("(ji)::cd"));
}

#[test]
fn close_default_is_refused() {
    let r = Repo::new();
    // From the default workspace, source resolves to the cwd (default) → refused.
    let o = r.run(&r.default_ws, &["close", "feature"], None);
    let e = stderr(&o);
    assert!(
        !o.status.success(),
        "closing default must be refused; stderr: {e}"
    );
    assert!(
        e.contains("refusing to close the default workspace"),
        "stderr: {e}"
    );
    // The guard fires before any mutation — nothing is removed.
    assert!(
        r.feature_ws.exists() && r.default_ws.join("README").exists(),
        "the refused close must remove nothing"
    );
}

#[test]
fn rescue_when_current_workspace_deleted() {
    let r = Repo::new();
    // Standing in the feature workspace, close it into default and delete its files.
    let o = r.run(&r.feature_ws, &["close", "--delete-files", "default"], None);
    let e = stderr(&o);
    assert!(
        !o.status.success(),
        "rescue (stranded cwd) must exit non-zero; stderr: {e}"
    );
    assert!(
        e.contains("current directory was removed"),
        "expected rescue escape; stderr: {e}"
    );
    assert!(
        e.contains("run: cd '"),
        "rescue should print a quoted cd target; stderr: {e}"
    );
}

#[test]
fn benign_close_keeps_dir_exits_zero_with_hint() {
    let r = Repo::new();
    // Close feature into default without --delete-files: the dir survives → benign.
    let o = r.run(&r.feature_ws, &["close", "default"], None);
    let e = stderr(&o);
    assert!(
        o.status.success(),
        "benign close should exit 0; stderr: {e}"
    );
    assert!(e.contains("(ji)::cd"), "expected a cd hint: {e}");
    assert!(
        r.feature_ws.exists(),
        "the workspace dir should survive a close without --delete-files"
    );
}

#[test]
fn install_with_yes_writes_without_prompt() {
    let r = Repo::new();
    let o = r.run(
        &r.default_ws,
        &["config", "shell", "install", "zsh", "--yes"],
        None,
    );
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(r.home.join(".config/ji/init.zsh").exists());
}

#[test]
fn install_non_tty_does_not_hang_or_prompt() {
    let r = Repo::new();
    let o = r.run(&r.default_ws, &["config", "shell", "install", "zsh"], None);
    assert!(
        o.status.success(),
        "install should succeed non-interactively; stderr: {}",
        stderr(&o)
    );
    // It wrote the managed file rather than waiting for input.
    assert!(r.home.join(".config/ji/init.zsh").exists());
}

#[test]
fn install_single_shell_output_is_stable() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    std::fs::write(home.join(".zshrc"), "").unwrap();

    let o = Command::new(env!("CARGO_BIN_EXE_ji"))
        .args(["config", "shell", "install", "zsh", "--dry-run"])
        .current_dir(home)
        .stdin(Stdio::null())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ZDOTDIR", home)
        .env("SHELL", "/bin/zsh")
        .env_remove("JI_DIRECTIVE_FILE")
        .output()
        .expect("run ji");

    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("--- "), "stdout: {s}");
    assert!(s.contains("+++ "), "stdout: {s}");
    assert!(s.contains("# >>> ji shell integration >>>"), "stdout: {s}");
}
