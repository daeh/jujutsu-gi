//! macOS alias / xattr / link fidelity across ji workspace operations.
//!
//! These tests pin the fidelity contract: Finder aliases (regular files
//! whose data fork is bookmark data plus a `com.apple.FinderInfo` xattr
//! carrying the kIsAlias flag), symlinks, and hard-linked file content
//! survive `ji` create/close/transfer, and ji warns where fidelity cannot
//! be preserved. A passing run confirms correct behavior; corruption fails
//! the suite. Hard-link *identity* is unrepresentable in jj's git object
//! model, so those tests assert content preservation plus ji's
//! "hard link broken" warning, never inode identity.
//!
//! Requirements (same as the other jj-backed suites): `jj` and `git` on
//! PATH, macOS, and an xattr-capable `TMPDIR` (APFS — the default on
//! macOS).
//!
//! Synthetic aliases are used throughout: bookmark-magic content plus a
//! programmatically set FinderInfo xattr. Corruption is xattr loss, so
//! nothing here needs Finder or resolvable bookmark data.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

use ji::commands;
use ji::commands::types::{BookmarkAction, CloseMethod, CloseTransferResult, TransferMethod};
use ji::config::Config;
use ji::finder_xattrs::{BOOKMARK_MAGIC, FINDER_INFO_XATTR, finder_info_marks_alias, get_xattr};

// ---------------------------------------------------------------------------
// Test harness (per-binary inline harness, mirroring tests/jj_integration.rs)
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

    fn change_id(&self, revset: &str) -> String {
        jj_r(
            &self.root,
            &["log", "--no-graph", "-r", revset, "-T", "change_id"],
        )
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

// ---------------------------------------------------------------------------
// Alias fixtures
// ---------------------------------------------------------------------------

fn alias_finder_info() -> [u8; 32] {
    let mut info = [0u8; 32];
    info[..8].copy_from_slice(b"alisMACS");
    info[8] = 0x80; // kIsAlias, big-endian finderFlags bit 15
    info
}

fn bookmark_content(payload: &[u8]) -> Vec<u8> {
    let mut content = BOOKMARK_MAGIC.to_vec();
    content.extend_from_slice(payload);
    content
}

/// Write a synthetic Finder alias: bookmark-magic data fork + alias-flagged
/// FinderInfo xattr. No Finder involvement needed.
fn write_synthetic_alias(path: &Path, payload: &[u8]) {
    std::fs::write(path, bookmark_content(payload)).unwrap();
    ji::finder_xattrs::set_xattr(path, FINDER_INFO_XATTR, &alias_finder_info()).unwrap();
}

fn strip_finder_info(path: &Path) {
    let output = Command::new("/usr/bin/xattr")
        .args(["-d", FINDER_INFO_XATTR])
        .arg(path)
        .output()
        .expect("run /usr/bin/xattr -d");
    assert!(
        output.status.success(),
        "xattr -d failed on {}",
        path.display()
    );
}

/// Independent oracle for the FinderInfo xattr: `/usr/bin/xattr -px` hex
/// output, whitespace-normalized. `None` when the attribute is absent.
fn xattr_cli_hex(path: &Path) -> Option<String> {
    let output = Command::new("/usr/bin/xattr")
        .args(["-px", FINDER_INFO_XATTR])
        .arg(path)
        .output()
        .expect("run /usr/bin/xattr -px");
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    })
}

#[track_caller]
fn assert_alias_flag(path: &Path) {
    let info = get_xattr(path, FINDER_INFO_XATTR)
        .unwrap()
        .unwrap_or_else(|| {
            panic!(
                "{} has no FinderInfo xattr (alias corrupted)",
                path.display()
            )
        });
    assert!(
        finder_info_marks_alias(&info),
        "{} FinderInfo lacks the kIsAlias flag: {info:02x?}",
        path.display()
    );
    // Cross-check through the system CLI so the assertion doesn't depend
    // solely on the same crate the implementation uses.
    let hex = xattr_cli_hex(path).expect("xattr CLI must see the FinderInfo attribute");
    assert!(
        hex.starts_with("61 6C 69 73 4D 41 43 53 80"),
        "unexpected FinderInfo via xattr CLI: {hex}"
    );
}

#[track_caller]
fn assert_no_alias_flag(path: &Path) {
    let info = get_xattr(path, FINDER_INFO_XATTR).unwrap();
    assert!(
        info.is_none(),
        "{} unexpectedly carries FinderInfo: {info:02x?}",
        path.display()
    );
}

// ---------------------------------------------------------------------------
// ji operation helpers (library-level, the same entry points the CLI uses)
// ---------------------------------------------------------------------------

fn ji_create(repo: &TestRepo, config: &Config, name: &str) -> commands::types::CreateResult {
    ji_create_at(repo, config, name, "@")
}

fn ji_create_at(
    repo: &TestRepo,
    config: &Config,
    name: &str,
    revision: &str,
) -> commands::types::CreateResult {
    let ws_path = repo.tmp().join(name);
    commands::create::create(
        repo.root(),
        config,
        "repo",
        name,
        revision,
        repo.root(),
        &ws_path,
        &format!("{name} start"),
        false,
    )
    .expect("ji create")
}

fn merge_close(repo: &TestRepo, ws_name: &str, ws_path: &Path) -> CloseTransferResult {
    let params = commands::close::CloseParams {
        repo_root: repo.root(),
        source_name: ws_name,
        source_path: ws_path,
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
        preserve_finder_xattrs: true,
    };
    commands::close::close(&params).expect("merge-close")
}

fn transfer_ff_to(repo: &TestRepo, ws_name: &str, ws_path: &Path) -> CloseTransferResult {
    let params = commands::transfer::TransferParams {
        repo_root: repo.root(),
        source_name: "default",
        source_path: repo.root(),
        target_name: ws_name,
        target_path: ws_path,
        method: TransferMethod::FastForwardTarget,
        workspace_path_template: "",
        repo_name: "repo",
        author: None,
        preserve_finder_xattrs: true,
    };
    commands::transfer::transfer(&params).expect("transfer fast-forward-target")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Regression guard: symlinks are tracked as git mode 120000 and must
/// round-trip through workspace creation as symlinks.
#[test]
fn symlink_survives_workspace_create() {
    let repo = TestRepo::new();
    std::fs::write(repo.root().join("target.txt"), "the target").unwrap();
    std::os::unix::fs::symlink("target.txt", repo.root().join("link-sym")).unwrap();
    jj(repo.root(), &["describe", "-m", "with symlink"]);

    let result = ji_create(&repo, &Config::default(), "feat");

    let ws_link = result.workspace_path.join("link-sym");
    let meta = std::fs::symlink_metadata(&ws_link).expect("symlink must materialize");
    assert!(
        meta.file_type().is_symlink(),
        "link-sym must still be a symlink"
    );
    assert_eq!(
        std::fs::read_link(&ws_link).unwrap(),
        PathBuf::from("target.txt")
    );
}

/// A Finder alias in the source workspace keeps its data fork AND its
/// FinderInfo xattr in the newly created workspace.
#[test]
fn alias_finder_info_survives_workspace_create() {
    assert!(
        Config::default().preserve_finder_xattrs,
        "preserve-finder-xattrs must default to on"
    );
    let repo = TestRepo::new();
    write_synthetic_alias(&repo.root().join("link.alias"), b"points-somewhere");
    jj(repo.root(), &["describe", "-m", "with alias"]);

    let result = ji_create(&repo, &Config::default(), "feat");

    let ws_alias = result.workspace_path.join("link.alias");
    assert_eq!(
        std::fs::read(&ws_alias).unwrap(),
        bookmark_content(b"points-somewhere"),
        "alias data fork must be preserved byte-for-byte"
    );
    assert_alias_flag(&ws_alias);
}

/// An alias retargeted in the workspace (data fork rewritten) merges back
/// into the target with the new data fork and an intact alias flag — even
/// when the workspace copy itself lost its xattr along the way.
#[test]
fn retargeted_alias_survives_merge_close() {
    let repo = TestRepo::new();
    let default_alias = repo.root().join("link.alias");
    write_synthetic_alias(&default_alias, b"old-target");
    jj(repo.root(), &["describe", "-m", "with alias"]);

    let result = ji_create(&repo, &Config::default(), "feat");
    let ws_alias = result.workspace_path.join("link.alias");

    // Retarget in the workspace; strip the workspace copy's xattr to force
    // the content-independent flag-restore rule (no exact-content candidate).
    std::fs::write(&ws_alias, bookmark_content(b"new-target")).unwrap();
    strip_finder_info(&ws_alias);
    jj(
        &result.workspace_path,
        &["describe", "-m", "retarget alias"],
    );

    merge_close(&repo, "feat", &result.workspace_path);

    assert_eq!(
        std::fs::read(&default_alias).unwrap(),
        bookmark_content(b"new-target"),
        "merge-close must land the retargeted data fork in the target"
    );
    assert_alias_flag(&default_alias);
}

/// A brand-new alias created in the workspace arrives in the target with
/// its alias flag (cross-workspace exact-content restore).
#[test]
fn new_alias_created_in_workspace_reaches_target_on_close() {
    let repo = TestRepo::new();
    std::fs::write(repo.root().join("base.txt"), "base").unwrap();
    jj(repo.root(), &["describe", "-m", "base"]);

    let result = ji_create(&repo, &Config::default(), "feat");
    write_synthetic_alias(&result.workspace_path.join("fresh.alias"), b"born-in-ws");
    jj(&result.workspace_path, &["describe", "-m", "add alias"]);

    merge_close(&repo, "feat", &result.workspace_path);

    let default_alias = repo.root().join("fresh.alias");
    assert_eq!(
        std::fs::read(&default_alias).unwrap(),
        bookmark_content(b"born-in-ws")
    );
    assert_alias_flag(&default_alias);
}

/// An alias retargeted in default@ AFTER the workspace was created reaches
/// the workspace with data fork and alias flag intact on transfer.
#[test]
fn alias_changed_in_default_survives_transfer_to_workspace() {
    let repo = TestRepo::new();
    let default_alias = repo.root().join("link.alias");
    write_synthetic_alias(&default_alias, b"old-target");
    jj(repo.root(), &["describe", "-m", "with alias"]);

    let result = ji_create(&repo, &Config::default(), "feat");

    // Retarget in default@ after workspace creation (in-place write keeps
    // default's own xattr; jj sees only the content change).
    std::fs::write(&default_alias, bookmark_content(b"new-target")).unwrap();
    jj(repo.root(), &["describe", "-m", "retarget in default"]);

    transfer_ff_to(&repo, "feat", &result.workspace_path);

    let ws_alias = result.workspace_path.join("link.alias");
    assert_eq!(
        std::fs::read(&ws_alias).unwrap(),
        bookmark_content(b"new-target"),
        "transfer must land the retargeted data fork in the workspace"
    );
    assert_alias_flag(&ws_alias);
}

/// Negative guard: a plain file that REPLACES an alias (fresh file, no
/// bookmark magic) must not come out of a merge-close carrying the alias
/// flag — the restore rules must never over-apply.
#[test]
fn plain_file_replacing_alias_does_not_gain_alias_flag() {
    let repo = TestRepo::new();
    let default_alias = repo.root().join("link.alias");
    write_synthetic_alias(&default_alias, b"old-target");
    jj(repo.root(), &["describe", "-m", "with alias"]);

    let result = ji_create(&repo, &Config::default(), "feat");
    let ws_file = result.workspace_path.join("link.alias");
    // Replace, don't retarget: remove + recreate gives a fresh inode with no
    // inherited xattr, matching how a user would author a plain replacement.
    std::fs::remove_file(&ws_file).unwrap();
    std::fs::write(&ws_file, "plain text now").unwrap();
    jj(
        &result.workspace_path,
        &["describe", "-m", "replace alias with text"],
    );

    merge_close(&repo, "feat", &result.workspace_path);

    assert_eq!(std::fs::read(&default_alias).unwrap(), b"plain text now");
    assert_no_alias_flag(&default_alias);
}

/// Hard-link CONTENT survives workspace creation. Link identity (shared
/// inode) is unrepresentable in jj's git object model and is deliberately
/// not asserted — ji's contract for identity is the warning, tested below.
#[test]
fn hard_link_content_survives_create() {
    let repo = TestRepo::new();
    std::fs::write(repo.root().join("a.sh"), "#!/bin/sh\necho linked\n").unwrap();
    std::fs::hard_link(repo.root().join("a.sh"), repo.root().join("b.sh")).unwrap();
    jj(repo.root(), &["describe", "-m", "with hard link"]);

    let result = ji_create(&repo, &Config::default(), "feat");

    for name in ["a.sh", "b.sh"] {
        assert_eq!(
            std::fs::read_to_string(result.workspace_path.join(name)).unwrap(),
            "#!/bin/sh\necho linked\n",
            "{name} content must be preserved"
        );
    }
}

/// Creating a workspace from a source containing hard-linked tracked files
/// warns that the links were broken (materialized as independent files).
#[test]
fn broken_hard_link_warns_on_create() {
    let repo = TestRepo::new();
    std::fs::write(repo.root().join("a.sh"), "#!/bin/sh\necho linked\n").unwrap();
    std::fs::hard_link(repo.root().join("a.sh"), repo.root().join("b.sh")).unwrap();
    jj(repo.root(), &["describe", "-m", "with hard link"]);

    let result = ji_create(&repo, &Config::default(), "feat");

    for name in ["a.sh", "b.sh"] {
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("hard link broken") && w.contains(name)),
            "expected a broken-hard-link warning for {name}, got: {:?}",
            result.warnings
        );
    }
}

/// Documented limitation: xattrs exist only on disk. An alias that lives
/// only in an old revision — deleted from every current checkout — has no
/// metadata oracle; ji materializes the data fork, warns, and cannot
/// restore the flag.
#[test]
fn alias_without_disk_oracle_warns_on_create() {
    let repo = TestRepo::new();
    write_synthetic_alias(&repo.root().join("link.alias"), b"points-somewhere");
    jj(repo.root(), &["describe", "-m", "with alias"]);
    let r1 = repo.change_id("@");

    jj(repo.root(), &["new", "-m", "remove alias"]);
    std::fs::remove_file(repo.root().join("link.alias")).unwrap();
    jj(repo.root(), &["describe", "-m", "remove alias"]);

    let result = ji_create_at(&repo, &Config::default(), "feat", &r1);

    let ws_alias = result.workspace_path.join("link.alias");
    assert_eq!(
        std::fs::read(&ws_alias).unwrap(),
        bookmark_content(b"points-somewhere"),
        "the data fork always materializes"
    );
    assert_no_alias_flag(&ws_alias);
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("alias flag lost") && w.contains("link.alias")),
        "expected a no-oracle alias warning, got: {:?}",
        result.warnings
    );
}

/// `preserve-finder-xattrs = false` disables the restore writes but keeps
/// the fidelity warnings (detect & warn stands alone).
#[test]
fn fixup_disabled_by_config_still_warns() {
    let repo = TestRepo::new();
    write_synthetic_alias(&repo.root().join("link.alias"), b"points-somewhere");
    jj(repo.root(), &["describe", "-m", "with alias"]);

    let config = Config {
        preserve_finder_xattrs: false,
        ..Config::default()
    };
    let result = ji_create(&repo, &config, "feat");

    let ws_alias = result.workspace_path.join("link.alias");
    assert_no_alias_flag(&ws_alias);
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("preserve-finder-xattrs = false") && w.contains("link.alias")),
        "expected a restore-disabled warning, got: {:?}",
        result.warnings
    );
}
