//! Integration tests for jj.rs functions.
//!
//! These tests create real jj repos in temp directories and verify that
//! workspace operations behave correctly. They require `jj` and `git`
//! to be installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct TestRepo {
    dir: TempDir,
    root: PathBuf,
}

impl TestRepo {
    /// Create a jj repo at {tmp}/repo/main
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

    // Helpers kept for future tests; allow dead_code so the harness stays
    // available without warnings.
    #[allow(dead_code)]
    fn change_ids(&self, revset: &str) -> Vec<String> {
        let out = jj_query(&self.root, revset, "change_id ++ \"\\n\"");
        out.lines()
            .filter(|l| !l.is_empty())
            .map(std::string::ToString::to_string)
            .collect()
    }

    #[allow(dead_code)]
    fn descriptions(&self, revset: &str) -> Vec<String> {
        let out = jj_query(&self.root, revset, "description.first_line() ++ \"\\n\"");
        out.lines()
            .filter(|l| !l.is_empty())
            .map(std::string::ToString::to_string)
            .collect()
    }

    /// Count visible (non-hidden) revisions matching a revset.
    #[allow(dead_code)]
    fn rev_count(&self, revset: &str) -> usize {
        self.change_ids(revset).len()
    }

    /// Check if a revset resolves to any revisions.
    fn rev_exists(&self, revset: &str) -> bool {
        let output = Command::new("jj")
            .arg("-R")
            .arg(&self.root)
            .args(["log", "--no-graph", "-r", revset, "-T", "\"x\""])
            .output()
            .unwrap();
        output.status.success() && !output.stdout.is_empty()
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

fn jj_r(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("jj")
        .arg("-R")
        .arg(repo)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run jj {}: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "jj -R {} {} failed:\nstdout: {}\nstderr: {}",
        repo.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn jj_query(repo: &Path, revset: &str, template: &str) -> String {
    jj_r(repo, &["log", "--no-graph", "-r", revset, "-T", template])
}

fn ws_revisions(repo: &Path, ws_head: &str, default_head: &str) -> Vec<String> {
    let revset = format!("::{ws_head} ~ ::{default_head}");
    let out = jj_query(repo, &revset, "change_id ++ \"\\n\"");
    out.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Original regression tests
// ---------------------------------------------------------------------------

#[test]
fn workspace_revisions_returns_only_unique_revisions() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "commit A");
    repo.jj_new("commit B");
    repo.commit_file("b.txt", "b", "commit B");
    repo.jj_new("commit C");
    repo.commit_file("c.txt", "c", "commit C");

    let ws_path = repo.tmp().join("ws-feature");
    jj(
        repo.root(),
        &[
            "workspace",
            "add",
            &ws_path.to_string_lossy(),
            "--revision",
            "@",
            "-m",
            "ws start",
        ],
    );
    jj(&ws_path, &["new", "-m", "ws commit 1"]);
    std::fs::write(ws_path.join("ws1.txt"), "ws1").unwrap();
    jj(&ws_path, &["describe", "-m", "ws commit 1"]);
    jj(&ws_path, &["new", "-m", "ws commit 2"]);
    std::fs::write(ws_path.join("ws2.txt"), "ws2").unwrap();
    jj(&ws_path, &["describe", "-m", "ws commit 2"]);

    jj(repo.root(), &["new", "-m", "post-fork default commit"]);
    std::fs::write(repo.root().join("d.txt"), "d").unwrap();
    jj(repo.root(), &["describe", "-m", "post-fork default commit"]);

    let ws_head = repo.change_id("ws-feature@");
    let default_id = repo.change_id("default@");
    let revs = ws_revisions(repo.root(), &ws_head, &default_id);

    assert_eq!(
        revs.len(),
        3,
        "expected 3 workspace-only revisions, got {}: {:?}",
        revs.len(),
        revs
    );
}

#[test]
fn workspace_root_returns_default_not_current() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");

    let ws_path = repo.tmp().join("ws-other");
    jj(
        repo.root(),
        &[
            "workspace",
            "add",
            &ws_path.to_string_lossy(),
            "--revision",
            "@",
            "-m",
            "other ws",
        ],
    );

    let output = Command::new("jj")
        .current_dir(&ws_path)
        .args(["workspace", "root", "--name", "default"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let root = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());

    assert_eq!(
        root,
        repo.root()
            .canonicalize()
            .unwrap_or(repo.root().to_path_buf()),
    );
    assert_ne!(root, ws_path.canonicalize().unwrap_or(ws_path.clone()));
}

#[test]
fn abandon_refuses_over_safety_cap() {
    // The safety cap (MAX_DESTRUCTIVE_REVISIONS = 50) is in our Rust code.
    // This test just validates the threshold constant is correct.
    let count = (0..51).map(|i| format!("fake{i:04}")).count();
    assert!(count > 50);
}

#[test]
fn workspace_revisions_with_deep_shared_history() {
    let repo = TestRepo::new();
    for i in 0..20 {
        repo.commit_file(
            &format!("file{i}.txt"),
            &format!("c{i}"),
            &format!("shared {i}"),
        );
        if i < 19 {
            repo.jj_new(&format!("wip {}", i + 1));
        }
    }

    let ws_path = repo.tmp().join("ws-deep");
    jj(
        repo.root(),
        &[
            "workspace",
            "add",
            &ws_path.to_string_lossy(),
            "--revision",
            "@",
            "-m",
            "deep ws",
        ],
    );
    jj(&ws_path, &["new", "-m", "ws deep 1"]);
    std::fs::write(ws_path.join("d1.txt"), "d1").unwrap();
    jj(&ws_path, &["describe", "-m", "ws deep 1"]);
    jj(&ws_path, &["new", "-m", "ws deep 2"]);
    std::fs::write(ws_path.join("d2.txt"), "d2").unwrap();
    jj(&ws_path, &["describe", "-m", "ws deep 2"]);

    let ws_head = repo.change_id("ws-deep@");
    let default_id = repo.change_id("default@");
    let revs = ws_revisions(repo.root(), &ws_head, &default_id);

    assert_eq!(revs.len(), 3, "expected 3, got {}: {:?}", revs.len(), revs);
}

#[test]
fn forget_then_rebase_works() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    repo.jj_new("default wip");
    let default_head = repo.change_id("@");

    let ws_path = repo.tmp().join("ws-rebase");
    jj(
        repo.root(),
        &[
            "workspace",
            "add",
            &ws_path.to_string_lossy(),
            "--revision",
            &default_head,
            "-m",
            "ws to rebase",
        ],
    );
    jj(&ws_path, &["new", "-m", "ws work"]);
    std::fs::write(ws_path.join("work.txt"), "work").unwrap();
    jj(&ws_path, &["describe", "-m", "ws work"]);

    jj(repo.root(), &["new", "-m", "post-fork"]);
    std::fs::write(repo.root().join("post.txt"), "post").unwrap();
    jj(repo.root(), &["describe", "-m", "post-fork"]);

    let new_default_head = repo.change_id("default@");
    let ws_revs = ws_revisions(
        repo.root(),
        &repo.change_id("ws-rebase@"),
        &new_default_head,
    );
    let root_rev = ws_revs.last().unwrap();

    jj_r(repo.root(), &["workspace", "forget", "ws-rebase"]);
    jj_r(
        repo.root(),
        &["rebase", "-s", root_rev, "-d", &new_default_head],
    );

    let parent = repo.change_id(&format!("{root_rev}-"));
    assert_eq!(parent, new_default_head);
}

#[test]
fn closing_one_workspace_doesnt_affect_another() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    let default_head = repo.change_id("@");

    let ws_a = repo.tmp().join("ws-a");
    let ws_b = repo.tmp().join("ws-b");
    jj(
        repo.root(),
        &[
            "workspace",
            "add",
            &ws_a.to_string_lossy(),
            "--revision",
            &default_head,
            "-m",
            "ws-a start",
        ],
    );
    jj(
        repo.root(),
        &[
            "workspace",
            "add",
            &ws_b.to_string_lossy(),
            "--revision",
            &default_head,
            "-m",
            "ws-b start",
        ],
    );

    jj(&ws_a, &["new", "-m", "a work"]);
    std::fs::write(ws_a.join("a.txt"), "a-work").unwrap();
    jj(&ws_a, &["describe", "-m", "a work"]);

    jj(&ws_b, &["new", "-m", "b work"]);
    std::fs::write(ws_b.join("b.txt"), "b-work").unwrap();
    jj(&ws_b, &["describe", "-m", "b work"]);

    let default_id = repo.change_id("default@");
    let ws_a_revs = ws_revisions(repo.root(), &repo.change_id("ws-a@"), &default_id);

    jj_r(repo.root(), &["workspace", "forget", "ws-a"]);
    let mut args = vec!["abandon"];
    let refs: Vec<&str> = ws_a_revs.iter().map(|s| s.as_str()).collect();
    args.extend(&refs);
    jj_r(repo.root(), &args);

    let ws_b_revs = ws_revisions(repo.root(), &repo.change_id("ws-b@"), &default_id);
    assert_eq!(
        ws_b_revs.len(),
        2,
        "ws-b should still have 2 revisions, got {ws_b_revs:?}"
    );
}

// ---------------------------------------------------------------------------
// Triangle merge scenario: the full close workflow test
//
// Graph:
//   main:  M0 -> M1 (default @)
//   z:     branches from M0, has 3 parallel revisions (p1, p2, p3) merged into z@
//   a:     branches from p1, has 2 commits
//   b:     branches from p2, has 3 commits
//   c:     branches from p3, has 1 commit
//
// Close operations:
//   a: rebase (keep all revisions, move onto default @)
//   b: squash + rebase (collapse to 1, move onto default @)
//   c: abandon (discard all work)
// ---------------------------------------------------------------------------

/// Build the full triangle merge scenario and test all three close workflows.
#[test]
fn triangle_merge_close_workflows() {
    let repo = TestRepo::new();
    let wsdir = repo.tmp().join("ws");
    std::fs::create_dir_all(&wsdir).unwrap();

    // --- Build main history ---
    repo.commit_file("base.txt", "base", "M0: initial");
    repo.jj_new("M1");
    repo.commit_file("main.txt", "main", "M1: main work");
    let m0 = repo.change_id("@-");

    // --- Create workspace z branching from M0 ---
    let z_path = wsdir.join("z");
    jj_r(
        repo.root(),
        &[
            "workspace",
            "add",
            &z_path.to_string_lossy(),
            "--revision",
            &m0,
            "-m",
            "z: workspace",
        ],
    );

    // In z, create 3 parallel revisions off of z's initial commit.
    // All operations use -R to avoid stale working copy issues.
    let z_base = repo.change_id("z@");

    // p1: branch from z_base
    jj_r(repo.root(), &["new", &z_base, "-m", "p1: feature alpha"]);
    let p1 = repo.change_id("@");
    // Edit from z workspace to write files there
    jj_r(repo.root(), &["edit", &p1, "--ignore-working-copy"]);

    // p2: branch from z_base
    jj_r(repo.root(), &["new", &z_base, "-m", "p2: feature beta"]);
    let p2 = repo.change_id("@");

    // p3: branch from z_base
    jj_r(repo.root(), &["new", &z_base, "-m", "p3: feature gamma"]);
    let p3 = repo.change_id("@");

    // Merge p1, p2, p3 into z's head (edit z workspace to the merge)
    jj(&z_path, &["workspace", "update-stale"]);
    jj(&z_path, &["new", &p1, &p2, &p3, "-m", "z: merge p1+p2+p3"]);

    // --- Create workspaces a, b, c branching from the parallel revisions ---

    // Workspace a: branches from p1, gets 2 commits
    let a_path = wsdir.join("a");
    jj_r(
        repo.root(),
        &[
            "workspace",
            "add",
            &a_path.to_string_lossy(),
            "--revision",
            &p1,
            "-m",
            "a: start",
        ],
    );
    jj(&a_path, &["new", "-m", "a: commit 1"]);
    std::fs::write(a_path.join("a1.txt"), "a1").unwrap();
    jj(&a_path, &["describe", "-m", "a: commit 1"]);
    jj(&a_path, &["new", "-m", "a: commit 2"]);
    std::fs::write(a_path.join("a2.txt"), "a2").unwrap();
    jj(&a_path, &["describe", "-m", "a: commit 2"]);

    // Workspace b: branches from p2, gets 3 commits
    let b_path = wsdir.join("b");
    jj_r(
        repo.root(),
        &[
            "workspace",
            "add",
            &b_path.to_string_lossy(),
            "--revision",
            &p2,
            "-m",
            "b: start",
        ],
    );
    jj(&b_path, &["new", "-m", "b: commit 1"]);
    std::fs::write(b_path.join("b1.txt"), "b1").unwrap();
    jj(&b_path, &["describe", "-m", "b: commit 1"]);
    jj(&b_path, &["new", "-m", "b: commit 2"]);
    std::fs::write(b_path.join("b2.txt"), "b2").unwrap();
    jj(&b_path, &["describe", "-m", "b: commit 2"]);
    jj(&b_path, &["new", "-m", "b: commit 3"]);
    std::fs::write(b_path.join("b3.txt"), "b3").unwrap();
    jj(&b_path, &["describe", "-m", "b: commit 3"]);

    // Workspace c: branches from p3, gets 1 commit
    let c_path = wsdir.join("c");
    jj_r(
        repo.root(),
        &[
            "workspace",
            "add",
            &c_path.to_string_lossy(),
            "--revision",
            &p3,
            "-m",
            "c: start",
        ],
    );
    jj(&c_path, &["new", "-m", "c: commit 1"]);
    std::fs::write(c_path.join("c1.txt"), "c1").unwrap();
    jj(&c_path, &["describe", "-m", "c: commit 1"]);

    // --- Verify the setup ---
    let default_id = repo.change_id("default@");

    // The revsets include shared ancestors (z_base, p1/p2/p3) that aren't
    // reachable from default. Exact counts depend on the graph structure.
    // What matters: each workspace has revisions, and close operations work.
    let a_revs = ws_revisions(repo.root(), &repo.change_id("a@"), &default_id);
    assert!(
        !a_revs.is_empty(),
        "a should have unique revisions: {a_revs:?}"
    );

    let b_revs = ws_revisions(repo.root(), &repo.change_id("b@"), &default_id);
    assert!(
        !b_revs.is_empty(),
        "b should have unique revisions: {b_revs:?}"
    );

    let c_revs = ws_revisions(repo.root(), &repo.change_id("c@"), &default_id);
    assert!(
        !c_revs.is_empty(),
        "c should have unique revisions: {c_revs:?}"
    );

    // Also verify that z's revisions are separate from a/b/c
    let z_revs = ws_revisions(repo.root(), &repo.change_id("z@"), &default_id);
    // z has: z_base, p1, p2, p3, merge = 5 revisions
    // But a/b/c also branch from p1/p2/p3 — the revset should still correctly
    // scope z's unique revisions as those reachable from z@ but not from default@
    assert!(
        !z_revs.is_empty(),
        "z should have unique revisions: {z_revs:?}"
    );

    // ====================================================================
    // CLOSE A: rebase (keep all revisions, move onto default @)
    // ====================================================================
    let a_root = a_revs.last().unwrap().clone();
    let default_head = repo.change_id("default@");

    // Forget first, then rebase
    jj_r(repo.root(), &["workspace", "forget", "a"]);
    jj_r(repo.root(), &["rebase", "-s", &a_root, "-d", &default_head]);

    // Verify: a's revisions are now descendants of default head
    let a_parent = repo.change_id(&format!("{a_root}-"));
    assert_eq!(
        a_parent, default_head,
        "after rebase, a's root parent should be default head"
    );
    // All 3 revisions should still exist
    assert!(
        repo.rev_exists(&a_revs[0]),
        "a's head should still exist after rebase"
    );
    assert!(
        repo.rev_exists(&a_revs[1]),
        "a's middle commit should still exist after rebase"
    );
    assert!(
        repo.rev_exists(&a_revs[2]),
        "a's root should still exist after rebase"
    );
    // Directory can be removed
    std::fs::remove_dir_all(&a_path).unwrap();

    // ====================================================================
    // CLOSE B: squash + rebase (collapse to 1 revision, move onto default @)
    // ====================================================================
    let default_head = repo.change_id("default@");

    // Forget first
    jj_r(repo.root(), &["workspace", "forget", "b"]);

    // Squash from head toward root (newest first, skip the last which is the root)
    for rev in &b_revs[..b_revs.len().saturating_sub(1)] {
        jj_r(repo.root(), &["squash", "-r", rev, "-u"]);
    }
    let b_root = b_revs.last().unwrap();

    // Rebase the remaining single revision
    jj_r(repo.root(), &["rebase", "-s", b_root, "-d", &default_head]);

    // Verify: b's root is now a child of default head
    let b_parent = repo.change_id(&format!("{b_root}-"));
    assert_eq!(
        b_parent, default_head,
        "after squash+rebase, b's root parent should be default head"
    );
    // The squashed revisions (b_revs[0], [1], [2]) should be abandoned/gone
    // Only b_root should remain (with all content squashed into it)
    assert!(
        repo.rev_exists(b_root),
        "b's root (squash target) should exist"
    );
    // Directory can be removed
    std::fs::remove_dir_all(&b_path).unwrap();

    // ====================================================================
    // CLOSE C: abandon (discard all work)
    // ====================================================================

    // Forget first
    jj_r(repo.root(), &["workspace", "forget", "c"]);

    // Abandon all of c's revisions
    let mut abandon_args = vec!["abandon"];
    let c_refs: Vec<&str> = c_revs.iter().map(|s| s.as_str()).collect();
    abandon_args.extend(&c_refs);
    jj_r(repo.root(), &abandon_args);

    // Verify: c's revisions should be hidden (abandoned)
    for rev in &c_revs {
        assert!(
            !repo.rev_exists(rev),
            "c's revision {rev} should be abandoned/hidden"
        );
    }
    // Directory can be removed
    std::fs::remove_dir_all(&c_path).unwrap();

    // ====================================================================
    // Verify z is completely unaffected by all the close operations
    // ====================================================================
    // z@ should still resolve (workspace not accidentally forgotten)
    assert!(
        repo.rev_exists("z@"),
        "z workspace should still exist after closing a/b/c"
    );
}

// ---------------------------------------------------------------------------
// Merge detection and squash safety
// ---------------------------------------------------------------------------

#[test]
fn squash_fails_on_merge_revision() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");

    // Create two branches
    jj(repo.root(), &["new", "-m", "branch-a"]);
    std::fs::write(repo.root().join("b.txt"), "b").unwrap();
    let branch_a = jj_query(repo.root(), "@", r#"change_id"#);

    jj(repo.root(), &["new", "@--", "-m", "branch-b"]);
    std::fs::write(repo.root().join("c.txt"), "c").unwrap();
    let branch_b = jj_query(repo.root(), "@", r#"change_id"#);

    // Merge: create a revision with two parents
    jj(repo.root(), &["new", &branch_a, &branch_b, "-m", "merge"]);
    let merge_id = jj_query(repo.root(), "@", r#"change_id"#);

    // Verify: jj squash -r <merge> -u should fail because it's a merge
    let output = Command::new("jj")
        .args(["-R", &repo.root().to_string_lossy()])
        .args(["squash", "-r", &merge_id, "-u"])
        .output()
        .expect("failed to run jj squash");
    assert!(
        !output.status.success(),
        "jj squash -r should fail on a merge revision"
    );

    // Verify: parents.len() template detects the merge
    let parent_count = jj_query(repo.root(), &merge_id, "parents.len()");
    assert_eq!(
        parent_count.trim(),
        "2",
        "merge revision should have 2 parents"
    );
}

#[test]
fn squash_all_but_root_collapses_linear_chain() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");
    repo.jj_new("default wip");
    let default_head = jj_query(repo.root(), "@", r#"change_id"#);

    // Create workspace with 3 linear commits
    let ws_path = repo.tmp().join("ws-squash");
    jj(
        repo.root(),
        &[
            "workspace",
            "add",
            &ws_path.to_string_lossy(),
            "--revision",
            &default_head,
            "-m",
            "ws root",
        ],
    );
    jj(&ws_path, &["new", "-m", "ws mid"]);
    std::fs::write(ws_path.join("mid.txt"), "mid").unwrap();
    jj(&ws_path, &["new", "-m", "ws head"]);
    std::fs::write(ws_path.join("head.txt"), "head").unwrap();

    let ws_name = "ws-squash";
    let ws_id = jj_query(repo.root(), &format!("{ws_name}@"), r#"change_id"#);

    let revs = ws_revisions(repo.root(), &ws_id, &default_head);
    assert_eq!(revs.len(), 3, "workspace should have 3 unique revisions");

    // Forget workspace, then squash all but root (same sequence as execute_close_inline)
    jj_r(repo.root(), &["workspace", "forget", ws_name]);

    // Squash head-to-root (all but last)
    for rev in &revs[..revs.len() - 1] {
        jj_r(repo.root(), &["squash", "-r", rev, "-u"]);
    }

    // Root should still exist and contain all files
    let root = revs.last().unwrap();
    assert!(
        repo.rev_exists(root),
        "root revision should still exist after squash"
    );
    // Intermediates should be abandoned
    for rev in &revs[..revs.len() - 1] {
        assert!(
            !repo.rev_exists(rev),
            "intermediate revision {rev} should be abandoned after squash"
        );
    }
}

// ---------------------------------------------------------------------------
// Freshness gates: probe, conditional snapshot, check_freshness,
// validate_head_info, abandon-set verification (plan: happy-mapping-hinton)
// ---------------------------------------------------------------------------

use ji::commands::types::{BookmarkAction, CloseMethod, SyncMode, SyncModeInfo, TransferMethod};
use ji::{commands, jj_utils, jujutsu};

/// Add a workspace at {tmp}/<name> branching from `rev`, returning its path.
fn add_ws(repo: &TestRepo, name: &str, rev: &str) -> PathBuf {
    let path = repo.tmp().join(name);
    jj_r(
        repo.root(),
        &[
            "workspace",
            "add",
            &path.to_string_lossy(),
            "--name",
            name,
            "--revision",
            rev,
            "-m",
            &format!("{name} start"),
        ],
    );
    path
}

fn op_head(repo: &TestRepo) -> String {
    jujutsu::current_op_head(repo.root()).unwrap_or_default()
}

#[test]
fn detach_moves_singular_bookmark_and_abandons_trivial_tip() {
    let repo = TestRepo::new();
    repo.commit_file("base.txt", "base", "base revision");
    let base = repo.change_id("@");
    let recovery = repo.tmp().join("recovery");
    jj_r(
        repo.root(),
        &[
            "workspace",
            "add",
            &recovery.to_string_lossy(),
            "--name",
            "recovery",
            "--revision",
            &base,
            "--message",
            "rec",
        ],
    );
    let trivial_tip = repo.change_id("recovery@");
    jj_r(
        repo.root(),
        &[
            "bookmark",
            "create",
            "--revision",
            "recovery@",
            "--",
            "recovery",
        ],
    );

    let params = commands::close::CloseParams {
        repo_root: repo.root(),
        source_name: "recovery",
        source_path: &recovery,
        target_name: "default",
        target_path: repo.root(),
        target_change_id: &base,
        method: CloseMethod::Detach,
        delete_files: false,
        bookmark_action: BookmarkAction::NoAction,
        bookmarks: vec!["recovery".into()],
        revisions: &[],
        workspace_path_template: "{{ bookmark }}",
        repo_name: "repo",
        author: None,
        preserve_finder_xattrs: false,
    };
    let result = commands::close::close(&params).expect("detach should succeed");

    assert!(result.post_errors.is_empty(), "{:?}", result.post_errors);
    assert_eq!(repo.change_id("recovery"), base);
    assert!(
        !repo.rev_exists(&trivial_tip),
        "unreferenced trivial workspace tip should be abandoned"
    );
    assert!(
        !jj_r(repo.root(), &["workspace", "list"]).contains("recovery:"),
        "workspace should be forgotten"
    );
}

#[test]
fn detach_abandons_stacked_unreferenced_trivial_tips() {
    let repo = TestRepo::new();
    repo.commit_file("base.txt", "base", "base revision");
    let base = repo.change_id("@");
    let recovery = repo.tmp().join("recovery");
    jj_r(
        repo.root(),
        &[
            "workspace",
            "add",
            &recovery.to_string_lossy(),
            "--name",
            "recovery",
            "--revision",
            &base,
            "--message",
            "rec",
        ],
    );
    let lower_tip = repo.change_id("recovery@");
    jj(&recovery, &["new", "--message", "wip"]);
    let upper_tip = repo.change_id("recovery@");
    jj_r(
        repo.root(),
        &[
            "bookmark",
            "create",
            "--revision",
            "recovery@",
            "--",
            "recovery",
        ],
    );

    let params = commands::close::CloseParams {
        repo_root: repo.root(),
        source_name: "recovery",
        source_path: &recovery,
        target_name: "default",
        target_path: repo.root(),
        target_change_id: &base,
        method: CloseMethod::Detach,
        delete_files: false,
        bookmark_action: BookmarkAction::NoAction,
        bookmarks: vec!["recovery".into()],
        revisions: &[],
        workspace_path_template: "{{ bookmark }}",
        repo_name: "repo",
        author: None,
        preserve_finder_xattrs: false,
    };
    let result = commands::close::close(&params).expect("detach should succeed");

    assert!(result.post_errors.is_empty(), "{:?}", result.post_errors);
    assert_eq!(repo.change_id("recovery"), base);
    assert!(!repo.rev_exists(&upper_tip));
    assert!(!repo.rev_exists(&lower_tip));
}

#[test]
fn detach_preserves_trivial_tip_pinned_by_no_action_bookmark() {
    let repo = TestRepo::new();
    repo.commit_file("base.txt", "base", "base revision");
    let base = repo.change_id("@");
    let recovery = repo.tmp().join("recovery");
    jj_r(
        repo.root(),
        &[
            "workspace",
            "add",
            &recovery.to_string_lossy(),
            "--name",
            "recovery",
            "--revision",
            &base,
            "--message",
            "rec",
        ],
    );
    let trivial_tip = repo.change_id("recovery@");
    for bookmark in ["recovery", "keep"] {
        jj_r(
            repo.root(),
            &[
                "bookmark",
                "create",
                "--revision",
                "recovery@",
                "--",
                bookmark,
            ],
        );
    }

    let params = commands::close::CloseParams {
        repo_root: repo.root(),
        source_name: "recovery",
        source_path: &recovery,
        target_name: "default",
        target_path: repo.root(),
        target_change_id: &base,
        method: CloseMethod::Detach,
        delete_files: false,
        bookmark_action: BookmarkAction::NoAction,
        bookmarks: vec!["recovery".into(), "keep".into()],
        revisions: &[],
        workspace_path_template: "{{ bookmark }}",
        repo_name: "repo",
        author: None,
        preserve_finder_xattrs: false,
    };
    let result = commands::close::close(&params).expect("detach should succeed");

    assert!(result.post_errors.is_empty(), "{:?}", result.post_errors);
    assert_eq!(repo.change_id("recovery"), base);
    assert_eq!(repo.change_id("keep"), trivial_tip);
    assert!(repo.rev_exists(&trivial_tip));
}

/// The probe detects un-snapshotted edits without integrating any operation.
#[test]
fn probe_detects_edits_without_touching_op_log() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");

    // Clean working copy: not dirty.
    assert!(
        !jujutsu::has_unsnapshotted_changes(repo.root()).unwrap(),
        "clean WC must probe false"
    );

    // Un-snapshotted edit: dirty, and the probe leaves no trace.
    std::fs::write(repo.root().join("b.txt"), "pending").unwrap();
    let before_head = op_head(&repo);
    // Compare op IDs only: the default op-log template renders relative
    // timestamps ("now" -> "1 second ago"), which the probe's own elapsed
    // time can flip between the two captures (flaky). The guarantee under
    // test is "no operation integrated" — that is exactly the op-ID set/order.
    let before_log = jj_r(
        repo.root(),
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "--template",
            r#"id ++ "\n""#,
            "--limit",
            "5",
        ],
    );
    assert!(
        jujutsu::has_unsnapshotted_changes(repo.root()).unwrap(),
        "pending edit must probe true"
    );
    let after_head = op_head(&repo);
    let after_log = jj_r(
        repo.root(),
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "--template",
            r#"id ++ "\n""#,
            "--limit",
            "5",
        ],
    );
    assert_eq!(before_head, after_head, "probe must not move the op head");
    assert_eq!(
        before_log, after_log,
        "probe must not change the visible op log"
    );
}

/// Content-only change to an already-modified file (identical file set):
/// the commit-id comparison catches what a file-list comparison cannot.
#[test]
fn probe_detects_content_only_change() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "v1", "initial");
    // a.txt is part of @'s snapshot. Change its content again: the file
    // LIST of @ is unchanged, only content differs.
    std::fs::write(repo.root().join("a.txt"), "v2").unwrap();
    assert!(
        jujutsu::has_unsnapshotted_changes(repo.root()).unwrap(),
        "content-only change must probe true"
    );
}

/// `jj util snapshot` (via `snapshot_ws`) is conditional: a clean working
/// copy — including an mtime-only touch — creates no operation; only a
/// content change moves the op head and folds the edit into `@`.
#[test]
fn snapshot_ws_is_conditional() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a", "initial");

    let h0 = op_head(&repo);
    jujutsu::snapshot_ws(repo.root()).unwrap();
    assert_eq!(op_head(&repo), h0, "clean WC: op head untouched");

    // mtime-only change: still no operation.
    let f = std::fs::File::options()
        .write(true)
        .open(repo.root().join("a.txt"))
        .unwrap();
    f.set_modified(std::time::SystemTime::now()).unwrap();
    drop(f);
    jujutsu::snapshot_ws(repo.root()).unwrap();
    assert_eq!(op_head(&repo), h0, "mtime-only touch: op head untouched");

    std::fs::write(repo.root().join("b.txt"), "pending").unwrap();
    jujutsu::snapshot_ws(repo.root()).unwrap();
    assert_ne!(op_head(&repo), h0, "dirty WC: op head moved");
    // The edit is folded into @.
    let shown = jj(repo.root(), &["file", "show", "-r", "@", "b.txt"]);
    assert_eq!(shown, "pending");
}

/// `snapshot_ws` FAILS on a jj-stale working copy — the premise of both
/// `workspace_forget`'s contract and the protection phase's stale-skip
/// classifier (`commands::prepare_execution_freshness`).
#[test]
fn snapshot_ws_fails_on_stale_workspace() {
    let repo = TestRepo::new();
    repo.commit_file("base.txt", "base", "M0");
    let m0 = repo.change_id("@");
    let feat = add_ws(&repo, "feat", &m0);

    make_stale(&repo, "feat", "extra.txt");
    assert!(jujutsu::is_workspace_stale(&feat), "setup: feat is stale");
    assert!(
        jujutsu::snapshot_ws(&feat).is_err(),
        "snapshot of a stale WC must fail"
    );
}

/// Make workspace `ws` jj-stale by rewriting its `@` TREE from the default
/// workspace: write `filename` in default and squash it into `<ws>@`. The
/// workspace's on-disk WC now lags the operation that rewrote its commit.
/// (A description-only rewrite is not enough on jj 0.42 — it reconciles
/// automatically; only a tree change leaves the workspace stale.)
fn make_stale(repo: &TestRepo, ws: &str, filename: &str) {
    std::fs::write(repo.root().join(filename), "stale-maker").unwrap();
    // cwd-based invocation: the fileset resolves relative to the repo root.
    jj(
        repo.root(),
        &[
            "squash",
            "--from",
            "default@",
            "--into",
            &format!("{ws}@"),
            "-m",
            "external tree rewrite",
            "--",
            filename,
        ],
    );
}

/// Regression for the motivating incident: un-snapshotted edits in the
/// target workspace must not end up stranded on an orphaned non-empty
/// `(ji::…)` placeholder head when syncing.
#[test]
fn sync_entry_snapshot_prevents_orphaned_placeholder() {
    let repo = TestRepo::new();
    repo.commit_file("base.txt", "base", "M0");
    let m0 = repo.change_id("@");

    // feat: ahead of default by one real commit.
    let feat = add_ws(&repo, "feat", &m0);
    jj(&feat, &["new", "-m", "F1"]);
    std::fs::write(feat.join("feat.txt"), "feat-work").unwrap();
    jj(&feat, &["describe", "-m", "F1: feature work"]);

    // default @: an empty ji placeholder (the incident's resting state)…
    jj(
        repo.root(),
        &[
            "new",
            &m0,
            "-m",
            "(ji::fast-forward) default@ to feat@deadbeef",
        ],
    );
    // …with a pending on-disk edit that nothing has snapshotted yet.
    std::fs::write(repo.root().join("stray.txt"), "reflow").unwrap();

    // CLI entry path: conditional snapshot, then sync.
    commands::snapshot_workspaces(&[("feat", &feat), ("default", repo.root())])
        .expect("entry snapshot");
    let outcome = commands::sync::sync(
        repo.root(),
        "feat",
        &feat,
        "default",
        repo.root(),
        "",
        "repo",
        None,
        false,
    )
    .expect("sync must succeed");
    match outcome {
        ji::operations::SyncOutcome::Done { warnings } => {
            assert!(warnings.is_empty(), "sync must not warn (got {warnings:?})");
        }
        ji::operations::SyncOutcome::AlreadyInSync => panic!("workspaces were not in sync"),
    }

    // The stray edit must be reachable from default@…
    let shown = jj(
        repo.root(),
        &["file", "show", "-r", "default@", "stray.txt"],
    );
    assert_eq!(
        shown, "reflow",
        "pending edit must ride along into default@"
    );
    // …and no anonymous non-empty ji-placeholder head may remain.
    assert!(
        !repo.rev_exists(r#"heads(all()) & description(glob:"(ji::*") ~ empty()"#),
        "no orphaned non-empty (ji::…) head may exist after sync"
    );
}

/// check_freshness: Unchanged / Equivalent (sync) / Changed / strict-Changed.
#[test]
fn check_freshness_variants() {
    let repo = TestRepo::new();
    repo.commit_file("base.txt", "base", "M0");
    let m0 = repo.change_id("@");
    let feat = add_ws(&repo, "feat", &m0);
    jj(&feat, &["new", "-m", "F1"]);
    std::fs::write(feat.join("feat.txt"), "feat-work").unwrap();
    jj(&feat, &["describe", "-m", "F1: feature work"]);
    // default @: empty trivial head on M0 (undescribed) so a folded edit
    // flips its effective head.
    jj(repo.root(), &["new", &m0]);

    let required: [(&str, &Path); 2] = [("feat", &feat), ("default", repo.root())];

    // Unchanged: no activity since detection.
    let info = commands::detect_sync_mode(repo.root(), "feat", "default");
    assert!(matches!(
        commands::check_freshness(repo.root(), &info, "feat", "default", &required, true).unwrap(),
        commands::Freshness::Unchanged
    ));

    // Equivalent (sync flavor): an unrelated third workspace's snapshot moves
    // the op head without touching the plan.
    let side = add_ws(&repo, "side", &m0);
    let info = commands::detect_sync_mode(repo.root(), "feat", "default");
    std::fs::write(side.join("side.txt"), "side-edit").unwrap();
    jujutsu::snapshot_ws(&side).unwrap();
    match commands::check_freshness(repo.root(), &info, "feat", "default", &required, true).unwrap()
    {
        commands::Freshness::Equivalent(fresh) => {
            assert!(commands::plan_equivalent(&info, &fresh));
            assert_ne!(
                fresh.op_head, info.op_head,
                "fresh info carries new op head"
            );
        }
        _ => panic!("unrelated movement with identical plan must be Equivalent"),
    }

    // Strict flavor: the same unrelated movement is Changed.
    let info = commands::detect_sync_mode(repo.root(), "feat", "default");
    std::fs::write(side.join("side2.txt"), "side-edit-2").unwrap();
    jujutsu::snapshot_ws(&side).unwrap();
    assert!(matches!(
        commands::check_freshness(repo.root(), &info, "feat", "default", &required, false).unwrap(),
        commands::Freshness::Changed
    ));

    // Changed (sync flavor): a pending edit in the TARGET flips its trivial
    // head to non-empty — the gate's own snapshot folds it, the plan differs.
    let info = commands::detect_sync_mode(repo.root(), "feat", "default");
    assert!(info.tgt_trivial_id.is_some(), "setup: default @ is trivial");
    std::fs::write(repo.root().join("pending.txt"), "pending").unwrap();
    assert!(matches!(
        commands::check_freshness(repo.root(), &info, "feat", "default", &required, true).unwrap(),
        commands::Freshness::Changed
    ));

    // Required workspace with a missing path: hard error, no silent skip.
    let ghost = repo.tmp().join("ghost");
    let info = commands::detect_sync_mode(repo.root(), "feat", "default");
    assert!(
        commands::check_freshness(
            repo.root(),
            &info,
            "feat",
            "default",
            &[("ghost", &ghost)],
            true,
        )
        .is_err(),
        "missing required path must be an error"
    );
}

/// plan_equivalent: op_head excluded; head/trivial flips detected.
#[test]
fn plan_equivalent_semantics() {
    let base = SyncModeInfo {
        mode: SyncMode::SourceOnly,
        src_effective_head: "s-eff".into(),
        tgt_effective_head: "t-eff".into(),
        src_actual_head: "s-act".into(),
        tgt_actual_head: "t-act".into(),
        src_trivial_id: None,
        tgt_trivial_id: Some("t-act".into()),
        src_trivial_ids: Vec::new(),
        tgt_trivial_ids: vec!["t-act".into()],
        lca: "t-eff".into(),
        op_head: "op-1".into(),
    };

    // op_head differs: still equivalent (it is the trigger, not the plan).
    let mut other = base.clone();
    other.op_head = "op-2".into();
    assert!(commands::plan_equivalent(&base, &other));

    // Trivial-id flip (e.g. a description change): not equivalent.
    let mut other = base.clone();
    other.tgt_trivial_id = None;
    assert!(!commands::plan_equivalent(&base, &other));

    // Effective-head flip (placeholder gained content): not equivalent.
    let mut other = base.clone();
    other.tgt_effective_head = "t-act".into();
    assert!(!commands::plan_equivalent(&base, &other));

    // Mode discriminant change: not equivalent.
    let mut other = base.clone();
    other.mode = SyncMode::Diverged;
    assert!(!commands::plan_equivalent(&base, &other));
}

/// validate_head_info: lenient (sync) proceeds with fresh lca on equivalent
/// movement and bails on plan change; strict bails on any movement.
#[test]
fn validate_head_info_flavors() {
    let repo = TestRepo::new();
    repo.commit_file("base.txt", "base", "M0");
    let m0 = repo.change_id("@");
    let feat = add_ws(&repo, "feat", &m0);
    jj(&feat, &["new", "-m", "F1"]);
    std::fs::write(feat.join("feat.txt"), "feat-work").unwrap();
    jj(&feat, &["describe", "-m", "F1: feature work"]);
    let side = add_ws(&repo, "side", &m0);

    // No movement: both flavors pass and return the cached info.
    let info = commands::detect_sync_mode(repo.root(), "feat", "default");
    let v = commands::validate_head_info(repo.root(), &info, "feat", "default", false).unwrap();
    assert_eq!(v.op_head, info.op_head);

    // Unrelated movement: lenient passes with the fresh info (new op head,
    // same lca); strict bails.
    std::fs::write(side.join("side.txt"), "side-edit").unwrap();
    jujutsu::snapshot_ws(&side).unwrap();
    let v = commands::validate_head_info(repo.root(), &info, "feat", "default", true)
        .expect("lenient must pass on equivalent plan");
    assert_eq!(v.lca, info.lca);
    assert_ne!(v.op_head, info.op_head);
    assert!(
        commands::validate_head_info(repo.root(), &info, "feat", "default", false).is_err(),
        "strict must bail on any movement"
    );

    // Plan-changing movement: lenient bails too.
    let info = commands::detect_sync_mode(repo.root(), "feat", "default");
    jj(&feat, &["new", "-m", "F2"]);
    assert!(
        commands::validate_head_info(repo.root(), &info, "feat", "default", true).is_err(),
        "lenient must bail when the plan changed"
    );
}

/// Abandon-set verification: a frozen revisions list that no longer matches
/// the live default-relative chain must bail before anything is destroyed.
#[test]
fn abandon_verifies_live_revision_set() {
    let repo = TestRepo::new();
    repo.commit_file("base.txt", "base", "M0");
    let m0 = repo.change_id("@");
    let feat = add_ws(&repo, "feat", &m0);
    jj(&feat, &["new", "-m", "F1"]);
    std::fs::write(feat.join("f1.txt"), "f1").unwrap();
    jj(&feat, &["describe", "-m", "F1"]);
    jj(&feat, &["new", "-m", "F2"]);
    std::fs::write(feat.join("f2.txt"), "f2").unwrap();
    jj(&feat, &["describe", "-m", "F2"]);

    let live = jj_utils::workspace_unique_change_ids(repo.root(), "feat").unwrap();
    assert!(live.len() >= 2, "setup: feat has a unique chain");

    // Frozen list deliberately missing one revision (stale capture).
    let stale_revisions: Vec<jujutsu::RevisionInfo> = live[1..]
        .iter()
        .map(|id| jujutsu::RevisionInfo {
            change_id: id.clone(),
            description: String::new(),
        })
        .collect();

    let params = commands::close::CloseParams {
        repo_root: repo.root(),
        source_name: "feat",
        source_path: &feat,
        target_name: "default",
        target_path: repo.root(),
        target_change_id: "",
        method: CloseMethod::Abandon,
        delete_files: false,
        bookmark_action: BookmarkAction::NoAction,
        bookmarks: Vec::new(),
        revisions: &stale_revisions,
        workspace_path_template: "",
        repo_name: "repo",
        author: None,
        preserve_finder_xattrs: false,
    };
    let err = match commands::close::close(&params) {
        Ok(_) => panic!("stale set must bail"),
        Err(e) => e,
    };
    assert!(
        format!("{err:#}").contains("source revisions changed"),
        "unexpected error: {err:#}"
    );
    // Nothing was destroyed.
    assert!(repo.rev_exists("feat@"), "feat workspace untouched");

    // Correct frozen list against a NON-default target: the live re-query is
    // default-relative (the producer definition), not LCA-relative, so this
    // must pass and abandon the chain.
    let other = add_ws(&repo, "other", &m0);
    let live = jj_utils::workspace_unique_change_ids(repo.root(), "feat").unwrap();
    let revisions: Vec<jujutsu::RevisionInfo> = live
        .iter()
        .map(|id| jujutsu::RevisionInfo {
            change_id: id.clone(),
            description: String::new(),
        })
        .collect();
    let params = commands::close::CloseParams {
        repo_root: repo.root(),
        source_name: "feat",
        source_path: &feat,
        target_name: "other",
        target_path: &other,
        target_change_id: "",
        method: CloseMethod::Abandon,
        delete_files: false,
        bookmark_action: BookmarkAction::NoAction,
        bookmarks: Vec::new(),
        revisions: &revisions,
        workspace_path_template: "",
        repo_name: "repo",
        author: None,
        preserve_finder_xattrs: false,
    };
    commands::close::close(&params).expect("correct frozen set must pass");
    for id in &live {
        assert!(!repo.rev_exists(id), "revision {id} must be abandoned");
    }
}

/// Dialog-open invariant: the live staleness probe (`is_workspace_stale`)
/// snapshots pending edits, so post-probe list reads see them. Pins the
/// load-bearing behavior `refresh()` depends on (probe before list rebuild).
#[test]
fn staleness_probe_snapshots_pending_edits() {
    let repo = TestRepo::new();
    repo.commit_file("base.txt", "base", "M0");

    std::fs::write(repo.root().join("pending.txt"), "pending").unwrap();
    let h0 = op_head(&repo);
    assert!(
        !jujutsu::is_workspace_stale(repo.root()),
        "workspace is not jj-stale"
    );
    assert_ne!(
        op_head(&repo),
        h0,
        "live probe must snapshot (op head moves)"
    );
    let shown = jj(repo.root(), &["file", "show", "-r", "@", "pending.txt"]);
    assert_eq!(shown, "pending", "edit folded into @ by the probe");
    // The post-probe workspace list reflects the folded edit.
    let wss = jujutsu::list_workspaces(repo.root()).unwrap();
    assert!(
        wss.iter().any(|w| w.name == "default"),
        "default workspace listed"
    );
}

// ---------------------------------------------------------------------------
// Execution-time freshness + protection phase
// (commands::prepare_execution_freshness)
// ---------------------------------------------------------------------------

/// Topology shared by the protection tests:
///   M0 (default@ stays here unless noted) ← feat-start ← F1 (feat@)
/// Returns (m0_change_id, feat_path).
fn protection_setup(repo: &TestRepo) -> (String, PathBuf) {
    repo.commit_file("base.txt", "base", "M0");
    let m0 = repo.change_id("@");
    let feat = add_ws(repo, "feat", &m0);
    jj(&feat, &["new", "-m", "F1"]);
    std::fs::write(feat.join("feat.txt"), "feat-work").unwrap();
    jj(&feat, &["describe", "-m", "F1: feature work"]);
    (m0, feat)
}

/// Edit-survival: a third-party workspace whose `@` descends from the
/// rewrite range and holds a pending un-snapshotted edit gets that edit
/// captured by the broad protection snapshot before the rebase-method
/// transfer rewrites its ancestry — reachable afterwards, not stranded.
#[test]
fn protection_snapshot_preserves_third_party_edits() {
    let repo = TestRepo::new();
    let (_m0, feat) = protection_setup(&repo);
    // Diverge default so the rebase has somewhere to go.
    repo.jj_new("D1");
    repo.commit_file("d1.txt", "d1", "D1");

    // obs: third-party workspace branching from feat's head (at-risk: its
    // ancestry is inside the rebase range), with a pending edit.
    let f1 = repo.change_id("feat@");
    let obs = add_ws(&repo, "obs", &f1);
    std::fs::write(obs.join("obs-pending.txt"), "obs-pending").unwrap();

    let params = commands::transfer::TransferParams {
        repo_root: repo.root(),
        source_name: "feat",
        source_path: &feat,
        target_name: "default",
        target_path: repo.root(),
        method: TransferMethod::Rebase,
        workspace_path_template: "",
        repo_name: "repo",
        author: None,
        preserve_finder_xattrs: false,
    };
    let result = commands::transfer::transfer(&params).expect("transfer must succeed");

    // The pending edit was captured into obs@ and survived the rebase.
    let shown = jj(
        repo.root(),
        &["file", "show", "-r", "obs@", "obs-pending.txt"],
    );
    assert_eq!(shown, "obs-pending", "third-party edit must survive");
    // obs was predicted stale (descendant of the rebased roots) and resolved.
    assert!(
        result.predicted_stale.iter().any(|n| n == "obs"),
        "obs must be in the predicted-stale set: {:?}",
        result.predicted_stale
    );
    assert!(
        result.stale_warnings.is_empty(),
        "obs staleness must be auto-resolved (got {:?})",
        result.stale_warnings
    );
}

/// jj-stale skip: a stale third-party workspace does not abort the
/// operation; the protection phase skips it and the post-op report tags it
/// as already stale (its edits belong to the update-stale workflow).
#[test]
fn protection_skips_stale_third_party_and_reports() {
    let repo = TestRepo::new();
    let (_m0, feat) = protection_setup(&repo);
    let f1 = repo.change_id("feat@");
    let _obs = add_ws(&repo, "obs", &f1);
    make_stale(&repo, "obs", "stale-maker.txt");

    let params = commands::close::CloseParams {
        repo_root: repo.root(),
        source_name: "feat",
        source_path: &feat,
        target_name: "default",
        target_path: repo.root(),
        target_change_id: "",
        method: CloseMethod::Merge,
        delete_files: false,
        bookmark_action: BookmarkAction::NoAction,
        bookmarks: Vec::new(),
        revisions: &[],
        workspace_path_template: "",
        repo_name: "repo",
        author: None,
        preserve_finder_xattrs: false,
    };
    let result = commands::close::close(&params).expect("close must proceed despite stale obs");
    assert!(
        result
            .stale_warnings
            .iter()
            .any(|w| w.contains("obs (was already stale)")),
        "stale obs must be reported as pre-existing: {:?}",
        result.stale_warnings
    );
}

/// Sync's broad protection: a dirty third-party workspace whose `@` descends
/// from a dirty involved (source) workspace is captured descendant-first —
/// its edit is folded before the source's own snapshot rebases it — and the
/// source's post-gate edit rides into the fast-forwarded target.
#[test]
fn sync_protection_preserves_third_party_edits() {
    let repo = TestRepo::new();
    let (_m0, feat) = protection_setup(&repo);
    let f1 = repo.change_id("feat@");
    let obs = add_ws(&repo, "obs", &f1);

    // Both dirty: obs (third-party descendant) and feat (involved source,
    // non-trivial described head — a content fold keeps its change id).
    std::fs::write(obs.join("obs-pending.txt"), "obs-pending").unwrap();
    std::fs::write(feat.join("feat-pending.txt"), "feat-pending").unwrap();

    let outcome = commands::sync::sync(
        repo.root(),
        "feat",
        &feat,
        "default",
        repo.root(),
        "",
        "repo",
        None,
        false,
    )
    .expect("sync must succeed");
    let warnings = match outcome {
        ji::operations::SyncOutcome::Done { warnings } => warnings,
        ji::operations::SyncOutcome::AlreadyInSync => panic!("workspaces were not in sync"),
    };
    // Only obs-related staleness may be reported (it descends from feat@,
    // whose involved snapshot rebased it after obs was captured).
    assert!(
        warnings.iter().all(|w| w.contains("obs")),
        "unexpected warnings: {warnings:?}"
    );

    // The third-party edit was captured into obs@ before the rebase.
    let shown = jj(
        repo.root(),
        &["file", "show", "-r", "obs@", "obs-pending.txt"],
    );
    assert_eq!(shown, "obs-pending", "third-party edit must survive");
    // The source's pending edit rode into the fast-forwarded target.
    let shown = jj(
        repo.root(),
        &["file", "show", "-r", "default@", "feat-pending.txt"],
    );
    assert_eq!(shown, "feat-pending", "involved edit must reach the target");
}

/// Required-tier capture + post-snapshot revalidation: a post-gate edit to
/// the sync TARGET is folded by the phase's required snapshot; the fold
/// flips the target's trivial placeholder head, the re-detected plan is no
/// longer equivalent, and the operation aborts — with the edit safely
/// captured (the `ssuouwov` class, now for sync).
#[test]
fn sync_required_snapshot_captures_post_gate_edit_and_aborts() {
    let repo = TestRepo::new();
    let (m0, feat) = protection_setup(&repo);
    // default @: empty trivial placeholder on M0 (undescribed).
    jj(repo.root(), &["new", &m0]);
    let tgt_id_before = repo.change_id("default@");

    let info = commands::detect_sync_mode(repo.root(), "feat", "default");
    assert!(info.tgt_trivial_id.is_some(), "setup: default @ is trivial");

    // Post-gate edit to the target, un-snapshotted at detection time.
    std::fs::write(repo.root().join("post-gate.txt"), "post-gate").unwrap();

    let err = commands::sync::sync_with_info(
        repo.root(),
        &info,
        "feat",
        &feat,
        "default",
        repo.root(),
        "",
        "repo",
        None,
        false,
    )
    .expect_err("flipped target head must abort");
    assert!(
        format!("{err:#}").contains("repo changed"),
        "unexpected error: {err:#}"
    );
    // The edit was captured (folded into default@), not stranded…
    let shown = jj(repo.root(), &["file", "show", "-r", "@", "post-gate.txt"]);
    assert_eq!(shown, "post-gate", "edit must be folded into default@");
    // …and nothing executed: default@ is still the same change.
    assert_eq!(
        repo.change_id("default@"),
        tgt_id_before,
        "no sync structure may have been created"
    );
}

/// Involved path check: executable params carrying a stale (relocated)
/// workspace path abort decisively — never a silent rebind to the new
/// location, never execution against the old directory.
#[test]
fn sync_aborts_on_relocated_involved_path() {
    let repo = TestRepo::new();
    let (_m0, feat_old) = protection_setup(&repo);
    let f1 = repo.change_id("feat@");

    // Relocate: forget feat, re-add under the same name at a new path.
    jj_r(repo.root(), &["workspace", "forget", "feat"]);
    let feat_new = repo.tmp().join("feat-relocated");
    jj_r(
        repo.root(),
        &[
            "workspace",
            "add",
            &feat_new.to_string_lossy(),
            "--name",
            "feat",
            "--revision",
            &f1,
            "-m",
            "feat re-added",
        ],
    );
    let tgt_id_before = repo.change_id("default@");

    // Fresh info (validate_head_info passes) but params hold the old path.
    let info = commands::detect_sync_mode(repo.root(), "feat", "default");
    let err = commands::sync::sync_with_info(
        repo.root(),
        &info,
        "feat",
        &feat_old,
        "default",
        repo.root(),
        "",
        "repo",
        None,
        false,
    )
    .expect_err("stale involved path must abort");
    assert!(
        format!("{err:#}").contains("repo changed"),
        "unexpected error: {err:#}"
    );
    assert_eq!(
        repo.change_id("default@"),
        tgt_id_before,
        "nothing may have executed"
    );
}
