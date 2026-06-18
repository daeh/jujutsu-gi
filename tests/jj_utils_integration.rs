//! Integration tests for jj_utils functions.
//!
//! These tests call ji's Rust functions against real jj repos in temp
//! directories. They require `jj` and `git` to be installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

use ji::jj_utils;

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct TestRepo {
    dir: TempDir,
    root: PathBuf,
}

impl TestRepo {
    fn new() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let root = dir.path().join("repo").join("main");
        std::fs::create_dir_all(&root).unwrap();

        jj(&root, &["git", "init"]);
        jj(&root, &["config", "set", "--repo", "user.name", "Test"]);
        jj(&root, &["config", "set", "--repo", "user.email", "t@t.com"]);

        Self { dir, root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn tmp(&self) -> &Path {
        self.dir.path()
    }

    fn commit_file(&self, filename: &str, content: &str, msg: &str) {
        std::fs::write(self.root.join(filename), content).unwrap();
        jj(&self.root, &["describe", "-m", msg]);
    }

    fn jj_new(&self, msg: &str) {
        jj(&self.root, &["new", "-m", msg]);
    }

    fn change_id(&self, revset: &str) -> String {
        jj_query(&self.root, revset, "change_id")
    }

    fn add_workspace(&self, name: &str, revision: &str) -> PathBuf {
        let ws_path = self.tmp().join(name);
        jj(
            self.root(),
            &[
                "workspace",
                "add",
                &ws_path.to_string_lossy(),
                "--revision",
                revision,
                "-m",
                &format!("{name}: start"),
            ],
        );
        ws_path
    }
}

fn jj(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("jj")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run jj {}: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "jj {} failed:\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn jj_query(repo: &Path, revset: &str, template: &str) -> String {
    let output = Command::new("jj")
        .arg("-R")
        .arg(repo)
        .args(["log", "--no-graph", "-r", revset, "-T", template])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "jj query failed for revset {revset}"
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

// ---------------------------------------------------------------------------
// is_head
// ---------------------------------------------------------------------------

#[test]
fn is_head_true_for_head() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");

    assert!(
        jj_utils::is_head(repo.root(), "@").unwrap(),
        "@ with no children should be a head"
    );
}

#[test]
fn is_head_false_for_interior() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    let first = repo.change_id("@");
    repo.jj_new("second");
    repo.commit_file("b.txt", "b", "second commit");

    assert!(
        !jj_utils::is_head(repo.root(), &first).unwrap(),
        "revision with children should not be a head"
    );
}

#[test]
fn is_head_true_for_multiple_heads() {
    // Two branches from the same parent — both should be heads.
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    let base = repo.change_id("@");

    repo.jj_new("branch-a");
    let branch_a = repo.change_id("@");

    jj(repo.root(), &["new", &base, "-m", "branch-b"]);
    let branch_b = repo.change_id("@");

    assert!(jj_utils::is_head(repo.root(), &branch_a).unwrap());
    assert!(jj_utils::is_head(repo.root(), &branch_b).unwrap());
    assert!(
        !jj_utils::is_head(repo.root(), &base).unwrap(),
        "base with two children should not be a head"
    );
}

// ---------------------------------------------------------------------------
// is_trivial_head
// ---------------------------------------------------------------------------

#[test]
fn is_trivial_head_empty_wip() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    repo.jj_new("wip");

    // @ is empty with a single-word description — trivial
    assert!(
        jj_utils::is_trivial_head(repo.root(), "@").unwrap(),
        "empty revision with single-word description should be trivial"
    );
}

#[test]
fn is_trivial_head_empty_no_description() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    repo.jj_new("");

    assert!(
        jj_utils::is_trivial_head(repo.root(), "@").unwrap(),
        "empty revision with no description should be trivial"
    );
}

#[test]
fn is_trivial_head_false_with_content() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    repo.jj_new("work in progress");
    std::fs::write(repo.root().join("b.txt"), "b").unwrap();

    assert!(
        !jj_utils::is_trivial_head(repo.root(), "@").unwrap(),
        "non-empty revision should not be trivial"
    );
}

#[test]
fn is_trivial_head_false_for_interior() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    let first = repo.change_id("@");
    repo.jj_new("second");

    assert!(
        !jj_utils::is_trivial_head(repo.root(), &first).unwrap(),
        "interior revision should not be a trivial head"
    );
}

#[test]
fn is_trivial_head_ji_marker() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    repo.jj_new("(ji::step-forward)");

    assert!(
        jj_utils::is_trivial_head(repo.root(), "@").unwrap(),
        "empty revision with ji:: marker should be trivial"
    );
}

#[test]
fn is_trivial_head_false_multiword_description() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    repo.jj_new("important feature work");

    // Empty but with a meaningful multi-word description — not trivial
    assert!(
        !jj_utils::is_trivial_head(repo.root(), "@").unwrap(),
        "empty revision with meaningful multi-word description should not be trivial"
    );
}

// ---------------------------------------------------------------------------
// check_trivial_head
// ---------------------------------------------------------------------------

#[test]
fn check_trivial_head_returns_change_id() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    repo.jj_new("wip");
    let expected_id = repo.change_id("@");

    let result = jj_utils::check_trivial_head(repo.root(), "@").unwrap();
    assert_eq!(
        result,
        Some(expected_id),
        "should return the change_id of the trivial head"
    );
}

#[test]
fn check_trivial_head_none_for_non_trivial() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    repo.jj_new("real work here");
    std::fs::write(repo.root().join("b.txt"), "b").unwrap();

    let result = jj_utils::check_trivial_head(repo.root(), "@").unwrap();
    assert_eq!(result, None, "should return None for non-trivial revision");
}

#[test]
fn check_trivial_head_none_for_interior() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    let first = repo.change_id("@");
    repo.jj_new("second");

    let result = jj_utils::check_trivial_head(repo.root(), &first).unwrap();
    assert_eq!(result, None, "should return None for interior revision");
}

// ---------------------------------------------------------------------------
// find_effective_head
// ---------------------------------------------------------------------------

#[test]
fn find_effective_head_skips_trivial_wip() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    let ws_path = repo.add_workspace("feature", "@");

    // Create a real commit, then a trivial WIP on top
    jj(&ws_path, &["new", "-m", "real work"]);
    std::fs::write(ws_path.join("f.txt"), "f").unwrap();
    jj(&ws_path, &["describe", "-m", "real work"]);
    let real_id = repo.change_id("feature@");

    jj(&ws_path, &["new", "-m", "wip"]);
    // feature@ is now the empty "wip" revision

    let effective = jj_utils::find_effective_head(repo.root(), "feature").unwrap();
    assert_eq!(
        effective, real_id,
        "effective head should skip the trivial WIP and return the real work"
    );
}

#[test]
fn find_effective_head_returns_at_when_not_trivial() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    let ws_path = repo.add_workspace("feature", "@");

    jj(&ws_path, &["new", "-m", "real work"]);
    std::fs::write(ws_path.join("f.txt"), "f").unwrap();
    jj(&ws_path, &["describe", "-m", "real work"]);
    let expected = repo.change_id("feature@");

    let effective = jj_utils::find_effective_head(repo.root(), "feature").unwrap();
    assert_eq!(
        effective, expected,
        "effective head should be @ when it's not trivial"
    );
}

// ---------------------------------------------------------------------------
// step_head
// ---------------------------------------------------------------------------

#[test]
fn step_head_creates_trivial_wip() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    let ws_path = repo.add_workspace("feature", "@");

    jj(&ws_path, &["new", "-m", "real work"]);
    std::fs::write(ws_path.join("f.txt"), "f").unwrap();
    jj(&ws_path, &["describe", "-m", "real work"]);
    let before = repo.change_id("feature@");

    let new_head = jj_utils::step_head(repo.root(), "feature", &ws_path, None, None).unwrap();
    assert_ne!(new_head, before, "step_head should create a new revision");

    // The new head should be trivial
    assert!(
        jj_utils::is_trivial_head(repo.root(), &new_head).unwrap(),
        "newly created step head should be trivial"
    );
}

#[test]
fn step_head_noop_when_already_trivial() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    let ws_path = repo.add_workspace("feature", "@");

    jj(&ws_path, &["new", "-m", "wip"]);
    let before = repo.change_id("feature@");

    let result = jj_utils::step_head(repo.root(), "feature", &ws_path, None, None).unwrap();
    assert_eq!(
        result, before,
        "step_head should be a no-op when already trivial"
    );
}

// ---------------------------------------------------------------------------
// make_desc
// ---------------------------------------------------------------------------

#[test]
fn make_desc_without_detail() {
    let desc = jj_utils::make_desc(jj_utils::Op::Step, None);
    assert_eq!(desc, "(ji::step-forward)");
}

#[test]
fn make_desc_with_detail() {
    let desc = jj_utils::make_desc(jj_utils::Op::FastForward, Some("default@ to feature@abc"));
    assert_eq!(desc, "(ji::fast-forward) default@ to feature@abc");
}

#[test]
fn make_desc_merge() {
    let desc = jj_utils::make_desc(jj_utils::Op::Merge, Some("a into b"));
    assert_eq!(desc, "(ji::merge) a into b");
}

#[test]
fn make_desc_empty_detail_same_as_none() {
    let a = jj_utils::make_desc(jj_utils::Op::Step, None);
    let b = jj_utils::make_desc(jj_utils::Op::Step, Some(""));
    assert_eq!(a, b);
}

// ---------------------------------------------------------------------------
// make_head
// ---------------------------------------------------------------------------

#[test]
fn make_head_creates_revision_on_parent() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    let ws_path = repo.add_workspace("feature", "@");

    jj(&ws_path, &["new", "-m", "work"]);
    std::fs::write(ws_path.join("f.txt"), "f").unwrap();
    jj(&ws_path, &["describe", "-m", "work"]);
    let work_id = repo.change_id("feature@");

    let new_id = jj_utils::make_head(
        repo.root(),
        "feature",
        &ws_path,
        Some(&work_id),
        jj_utils::Op::FastForward,
        Some("test"),
        None,
    )
    .unwrap();

    assert_ne!(new_id, work_id);
    // The new revision's parent should be work_id
    let parent = repo.change_id(&format!("{new_id}-"));
    assert_eq!(
        parent, work_id,
        "new head should be on top of the specified parent"
    );
}
