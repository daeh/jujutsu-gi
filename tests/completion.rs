//! Binary-level integration tests for dynamic workspace completion.
//!
//! Each test invokes the real `ji` binary through one of clap_complete's shell
//! protocols from an isolated temporary jj repository. This deliberately
//! asserts candidate output in addition to exit status because completion is
//! fail-soft: a lookup regression can otherwise return success with no output.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

const EXPECTED_WORKSPACES: [&str; 4] = ["alpha", "beta", "default", "gamma"];

#[derive(Clone, Copy)]
enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
        }
    }

    fn configure(self, command: &mut Command) {
        command.env("COMPLETE", self.name());
        match self {
            Self::Zsh => {
                command
                    .env("_CLAP_COMPLETE_INDEX", "2")
                    .env("_CLAP_IFS", "\n");
            }
            Self::Bash => {
                command
                    .env("_CLAP_COMPLETE_INDEX", "2")
                    .env("_CLAP_COMPLETE_COMP_TYPE", "9")
                    .env("_CLAP_COMPLETE_SPACE", "false")
                    .env("_CLAP_IFS", "\u{b}");
            }
            Self::Fish => {}
        }
    }

    fn candidate_names(self, stdout: &str) -> Vec<&str> {
        let records: Vec<_> = match self {
            Self::Bash => stdout
                .split('\u{b}')
                .filter(|line| !line.is_empty())
                .collect(),
            Self::Zsh | Self::Fish => stdout.lines().filter(|line| !line.is_empty()).collect(),
        };

        records
            .into_iter()
            .map(|record| match self {
                Self::Zsh => {
                    let (name, help) = record.split_once(':').unwrap_or_else(|| {
                        panic!("malformed zsh completion candidate: {record:?}")
                    });
                    assert!(!help.is_empty(), "zsh candidate has no help: {record:?}");
                    name
                }
                Self::Bash => record,
                Self::Fish => {
                    let (name, help) = record.split_once('\t').unwrap_or_else(|| {
                        panic!("malformed fish completion candidate: {record:?}")
                    });
                    assert!(!help.is_empty(), "fish candidate has no help: {record:?}");
                    name
                }
            })
            .collect()
    }
}

struct CompletionRepo {
    tmp: TempDir,
    home: PathBuf,
    root: PathBuf,
}

impl CompletionRepo {
    fn new() -> Self {
        let tmp = TempDir::new().expect("create completion fixture directory");
        let home = tmp.path().join("home");
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(home.join(".config")).expect("create isolated config directory");
        std::fs::create_dir_all(&root).expect("create fixture repository directory");

        let repo = Self { tmp, home, root };
        repo.jj(&["git", "init"]);
        repo.jj(&["config", "set", "--repo", "user.name", "Completion Test"]);
        repo.jj(&[
            "config",
            "set",
            "--repo",
            "user.email",
            "completion@example.com",
        ]);
        std::fs::write(repo.root.join("README"), "completion fixture\n")
            .expect("write fixture file");
        repo.jj(&["describe", "--message", "fixture root"]);

        for name in ["alpha", "beta", "gamma"] {
            let workspace = repo.tmp.path().join(name);
            let workspace = workspace.to_str().expect("workspace path is valid UTF-8");
            let message = format!("{name} fixture");
            repo.jj(&[
                "workspace",
                "add",
                workspace,
                "--name",
                name,
                "--revision",
                "@",
                "--message",
                &message,
            ]);
        }

        repo
    }

    fn jj(&self, args: &[&str]) {
        let mut command = Command::new("jj");
        self.configure_env(&mut command);
        let output = command
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("failed to run jj {args:?}: {error}"));
        assert!(
            output.status.success(),
            "jj {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn complete(&self, shell: Shell) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ji"));
        self.configure_env(&mut command);
        for variable in [
            "COMPLETE",
            "_CLAP_COMPLETE_INDEX",
            "_CLAP_COMPLETE_COMP_TYPE",
            "_CLAP_COMPLETE_SPACE",
            "_CLAP_IFS",
        ] {
            command.env_remove(variable);
        }
        shell.configure(&mut command);
        command
            .args(["--", "ji", "switch", ""])
            .env_remove("JI_DIRECTIVE_FILE")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("failed to run {} completion: {error}", shell.name()))
    }

    fn configure_env(&self, command: &mut Command) {
        command
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env_remove("JJ_CONFIG");
    }
}

fn assert_switch_completion(shell: Shell) {
    let repo = CompletionRepo::new();
    let output = repo.complete(shell);
    let stdout = String::from_utf8(output.stdout).expect("completion stdout is UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("completion stderr is UTF-8");
    assert!(
        output.status.success(),
        "{} completion failed:\n{stderr}",
        shell.name()
    );
    assert!(
        stderr.is_empty(),
        "{} completion stderr: {stderr}",
        shell.name()
    );

    let mut actual = shell.candidate_names(&stdout);
    actual.sort_unstable();
    assert_eq!(
        actual,
        EXPECTED_WORKSPACES,
        "{} completion output did not contain the fixture workspaces:\n{stdout}",
        shell.name()
    );
}

#[test]
fn zsh_switch_completion_lists_fixture_workspaces() {
    assert_switch_completion(Shell::Zsh);
}

#[test]
fn bash_switch_completion_lists_fixture_workspaces() {
    assert_switch_completion(Shell::Bash);
}

#[test]
fn fish_switch_completion_lists_fixture_workspaces() {
    assert_switch_completion(Shell::Fish);
}
