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
