//! Finder-metadata fidelity across jj working-copy materializations.
//!
//! jj (like git) stores only blob content and file mode; extended attributes
//! are never versioned, and every materialization is a fresh inode (jj's
//! checkout path removes the old file and recreates it with `create_new`).
//! A macOS Finder alias is a regular file whose data fork is bookmark data
//! (see [`BOOKMARK_MAGIC`]) and whose alias-ness lives in a
//! `com.apple.FinderInfo` xattr (`alisMACS` + the kIsAlias Finder flag), so
//! any jj rewrite silently downgrades it to a plain document. Hard links are
//! likewise unrepresentable: a rewrite or fresh materialization always
//! produces an independent inode.
//!
//! [`XattrGuard`] captures Finder-relevant xattrs and hard-link identity
//! from on-disk workspaces before an operation that may materialize files,
//! then restores the xattrs and emits warnings afterwards. Restore applies
//! the merged capture to every workspace, so it also heals a previously
//! stripped copy of an alias in a workspace the operation didn't touch.
//! The capture allowlist is exactly `com.apple.FinderInfo` +
//! `com.apple.ResourceFork`; `com.apple.provenance` and
//! `com.apple.quarantine` are system-managed and must not be copied.
//!
//! Known limitations (by design):
//! - xattrs exist only on disk, so an alias present only in a revision no
//!   current checkout materializes has no oracle — detected and warned, not
//!   restored. `jj op restore`/undo flows are uncovered for the same reason.
//! - Conflict-materialized content matches neither restore rule (no exact
//!   hash, no bookmark magic), so a conflicted alias stays unrestored until
//!   the user resolves the conflict.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::jujutsu;

pub const FINDER_INFO_XATTR: &str = "com.apple.FinderInfo";
pub const RESOURCE_FORK_XATTR: &str = "com.apple.ResourceFork";

/// First 16 bytes of macOS bookmark data (a Finder alias's data fork).
pub const BOOKMARK_MAGIC: &[u8; 16] = b"book\x00\x00\x00\x00mark\x00\x00\x00\x00";

/// Whether a `com.apple.FinderInfo` blob carries the kIsAlias Finder flag
/// (bit 15 of the big-endian finderFlags u16 at offset 8).
pub fn finder_info_marks_alias(info: &[u8]) -> bool {
    info.len() == 32 && info[8] & 0x80 != 0
}

pub fn get_xattr(path: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    xattr::get(path, name).with_context(|| format!("reading xattr {name} on {}", path.display()))
}

pub fn set_xattr(path: &Path, name: &str, value: &[u8]) -> Result<()> {
    xattr::set(path, name, value)
        .with_context(|| format!("writing xattr {name} on {}", path.display()))
}

/// One workspace's pre-operation record of a Finder-metadata-bearing file.
struct Candidate {
    ws_name: String,
    finder_info: Option<Vec<u8>>,
    resource_fork: Option<Vec<u8>>,
    content_hash: blake3::Hash,
    is_alias: bool,
}

/// Pre-operation hard-link identity of a tracked file (`st_nlink > 1`).
struct HardLink {
    dev: u64,
    ino: u64,
}

/// Pre-operation capture of Finder metadata + hard-link identity, restored
/// (and turned into warnings) after the operation's materializations.
pub struct XattrGuard {
    /// Config gate (`preserve-finder-xattrs`): gates the xattr writes only —
    /// detection warnings are always produced.
    restore_enabled: bool,
    /// workspace-relative path -> candidates, in capture (workspace-list) order.
    candidates: BTreeMap<String, Vec<Candidate>>,
    /// (workspace name, workspace-relative path) -> identity at capture time.
    hard_links: BTreeMap<(String, String), HardLink>,
}

impl XattrGuard {
    /// Pre-operation capture across every listed workspace: one
    /// `jj file list` per workspace, then a metadata/xattr scan. Content is
    /// hashed only for the (usually tiny) set of files that carry an
    /// allowlisted xattr. Best-effort by contract: enumeration or read
    /// failures shrink the capture, never fail the surrounding operation.
    pub fn capture(ws_paths: &[(String, PathBuf)], restore_enabled: bool) -> Self {
        let mut guard = Self {
            restore_enabled,
            candidates: BTreeMap::new(),
            hard_links: BTreeMap::new(),
        };
        for (ws_name, ws_path) in ws_paths {
            let Ok(files) = jujutsu::tracked_files(ws_path) else {
                continue;
            };
            for rel in files {
                guard.capture_file(ws_name, ws_path, &rel);
            }
        }
        guard
    }

    fn capture_file(&mut self, ws_name: &str, ws_path: &Path, rel: &str) {
        let path = ws_path.join(rel);
        let Ok(meta) = path.symlink_metadata() else {
            return;
        };
        if !meta.is_file() {
            return;
        }
        if meta.nlink() > 1 {
            self.hard_links.insert(
                (ws_name.to_string(), rel.to_string()),
                HardLink {
                    dev: meta.dev(),
                    ino: meta.ino(),
                },
            );
        }
        let finder_info = xattr::get(&path, FINDER_INFO_XATTR).ok().flatten();
        let resource_fork = xattr::get(&path, RESOURCE_FORK_XATTR).ok().flatten();
        if finder_info.is_none() && resource_fork.is_none() {
            return;
        }
        let Ok(content_hash) = hash_file(&path) else {
            return;
        };
        let is_alias = finder_info.as_deref().is_some_and(finder_info_marks_alias);
        self.candidates
            .entry(rel.to_string())
            .or_default()
            .push(Candidate {
                ws_name: ws_name.to_string(),
                finder_info,
                resource_fork,
                content_hash,
                is_alias,
            });
    }

    /// Post-operation restore over the merged candidate set, applied to every
    /// listed workspace, followed by the hard-link identity check. Returns
    /// sorted, deduplicated, non-fatal warnings.
    pub fn restore(&self, ws_paths: &[(String, PathBuf)]) -> Vec<String> {
        let mut warnings = Vec::new();
        for (ws_name, ws_path) in ws_paths {
            for rel in self.candidates.keys() {
                self.process_file(ws_name, ws_path, rel, &mut warnings);
            }
        }
        for ((ws_name, rel), captured) in &self.hard_links {
            let Some((_, ws_path)) = ws_paths.iter().find(|(name, _)| name == ws_name) else {
                continue;
            };
            let path = ws_path.join(rel);
            let Ok(meta) = path.symlink_metadata() else {
                continue;
            };
            // An inode change means this path was rewritten; an nlink drop
            // means its twin was. Either way the link is severed.
            if meta.is_file()
                && (meta.dev() != captured.dev || meta.ino() != captured.ino || meta.nlink() == 1)
            {
                warnings.push(hard_link_warning(ws_name, rel));
            }
        }
        finish_warnings(warnings)
    }

    /// Restore pass for a freshly created workspace: same rules, but the scan
    /// set is the workspace's full tracked-file list (everything was just
    /// materialized), so alias-magic files with no oracle are detected. Every
    /// captured hard-linked path present here is by definition an
    /// independent inode and warns.
    pub fn restore_new_workspace(&self, ws_name: &str, ws_path: &Path) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut rels: BTreeSet<String> = self.candidates.keys().cloned().collect();
        if let Ok(files) = jujutsu::tracked_files(ws_path) {
            rels.extend(files);
        }
        for rel in &rels {
            self.process_file(ws_name, ws_path, rel, &mut warnings);
        }
        for (_, rel) in self.hard_links.keys() {
            if ws_path
                .join(rel)
                .symlink_metadata()
                .is_ok_and(|m| m.is_file())
            {
                warnings.push(hard_link_warning(ws_name, rel));
            }
        }
        finish_warnings(warnings)
    }

    fn process_file(&self, ws_name: &str, ws_path: &Path, rel: &str, warnings: &mut Vec<String>) {
        let path = ws_path.join(rel);
        let Ok(meta) = path.symlink_metadata() else {
            return;
        };
        if !meta.is_file() {
            // Never write xattrs through a symlink; a mode-120000 entry
            // round-trips by itself.
            return;
        }
        // Presence short-circuit: jj's rewrite is always remove + create-new,
        // stripping every xattr, so existing Finder metadata means jj never
        // touched this file — leave it alone.
        if matches!(xattr::get(&path, FINDER_INFO_XATTR), Ok(Some(_))) {
            return;
        }
        let candidates = self.candidates.get(rel).map_or(&[][..], Vec::as_slice);
        let Ok(mut file) = std::fs::File::open(&path) else {
            return;
        };
        let mut prefix = Vec::with_capacity(BOOKMARK_MAGIC.len());
        if Read::by_ref(&mut file)
            .take(BOOKMARK_MAGIC.len() as u64)
            .read_to_end(&mut prefix)
            .is_err()
        {
            return;
        }
        let is_bookmark = prefix == BOOKMARK_MAGIC;
        // Hash only when a captured candidate could match.
        if !candidates.is_empty() {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&prefix);
            if hasher.update_reader(&mut file).is_err() {
                return;
            }
            let hash = hasher.finalize();
            if let Some(cand) = pick_exact(candidates, ws_name, &hash) {
                // Byte-identical content: the capture describes this exact
                // file — restore its full Finder metadata.
                self.apply(&path, ws_name, rel, cand, false, warnings);
                return;
            }
        }
        if is_bookmark {
            if let Some(cand) = pick_alias(candidates, ws_name) {
                // Bookmark data with a changed hash: a retargeted alias. The
                // kIsAlias flag is content-independent, but the captured
                // ResourceFork belonged to the old data fork — FinderInfo only.
                self.apply(&path, ws_name, rel, cand, true, warnings);
            } else {
                warnings.push(format!(
                    "finder alias: {ws_name}:{rel} has bookmark data but no restorable Finder metadata (alias flag lost)"
                ));
            }
        }
        // Anything else is a genuinely different file (e.g. an alias replaced
        // by a plain document) and must not regain Finder metadata.
    }

    fn apply(
        &self,
        path: &Path,
        ws_name: &str,
        rel: &str,
        cand: &Candidate,
        finder_info_only: bool,
        warnings: &mut Vec<String>,
    ) {
        if !self.restore_enabled {
            warnings.push(format!(
                "finder metadata not restored (preserve-finder-xattrs = false): {ws_name}:{rel}"
            ));
            return;
        }
        let mut attrs: Vec<(&str, &[u8])> = Vec::new();
        if let Some(info) = cand.finder_info.as_deref() {
            attrs.push((FINDER_INFO_XATTR, info));
        }
        if !finder_info_only && let Some(fork) = cand.resource_fork.as_deref() {
            attrs.push((RESOURCE_FORK_XATTR, fork));
        }
        for (name, value) in attrs {
            if let Err(err) = set_xattr(path, name, value) {
                warnings.push(format!(
                    "finder metadata restore failed: {ws_name}:{rel}: {err:#}"
                ));
            }
        }
    }
}

/// The candidate whose captured content matches `hash`, preferring the same
/// workspace, then capture order (deterministic).
fn pick_exact<'a>(
    candidates: &'a [Candidate],
    ws_name: &str,
    hash: &blake3::Hash,
) -> Option<&'a Candidate> {
    let matching = || candidates.iter().filter(|c| c.content_hash == *hash);
    matching()
        .find(|c| c.ws_name == ws_name)
        .or_else(|| matching().next())
}

/// The alias-flagged candidate to take FinderInfo from, preferring the same
/// workspace, then capture order.
fn pick_alias<'a>(candidates: &'a [Candidate], ws_name: &str) -> Option<&'a Candidate> {
    let alias = || {
        candidates
            .iter()
            .filter(|c| c.is_alias && c.finder_info.is_some())
    };
    alias()
        .find(|c| c.ws_name == ws_name)
        .or_else(|| alias().next())
}

fn hard_link_warning(ws_name: &str, rel: &str) -> String {
    format!(
        "hard link broken: {ws_name}:{rel} is now an independent file (jj cannot represent hard links)"
    )
}

fn finish_warnings(mut warnings: Vec<String>) -> Vec<String> {
    warnings.sort();
    warnings.dedup();
    warnings
}

fn hash_file(path: &Path) -> Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_reader(&mut std::fs::File::open(path)?)
        .with_context(|| format!("hashing {}", path.display()))?;
    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alias_finder_info() -> Vec<u8> {
        let mut info = vec![0u8; 32];
        info[..8].copy_from_slice(b"alisMACS");
        info[8] = 0x80;
        info
    }

    fn candidate(ws_name: &str, content: &[u8], is_alias: bool) -> Candidate {
        Candidate {
            ws_name: ws_name.to_string(),
            finder_info: is_alias.then(alias_finder_info),
            resource_fork: None,
            content_hash: blake3::hash(content),
            is_alias,
        }
    }

    #[test]
    fn finder_info_alias_flag_detected() {
        assert!(finder_info_marks_alias(&alias_finder_info()));
    }

    #[test]
    fn finder_info_without_flag_rejected() {
        let mut info = vec![0u8; 32];
        info[..8].copy_from_slice(b"alisMACS");
        assert!(!finder_info_marks_alias(&info));
    }

    #[test]
    fn finder_info_wrong_length_rejected() {
        assert!(!finder_info_marks_alias(b"alisMACS"));
        assert!(!finder_info_marks_alias(&[0x80u8; 64]));
    }

    #[test]
    fn pick_exact_prefers_same_workspace() {
        let cands = vec![
            candidate("other", b"data", true),
            candidate("mine", b"data", true),
        ];
        let hash = blake3::hash(b"data");
        assert_eq!(pick_exact(&cands, "mine", &hash).unwrap().ws_name, "mine");
        assert_eq!(
            pick_exact(&cands, "absent", &hash).unwrap().ws_name,
            "other"
        );
        assert!(pick_exact(&cands, "mine", &blake3::hash(b"else")).is_none());
    }

    #[test]
    fn pick_alias_skips_non_alias_candidates() {
        let cands = vec![candidate("a", b"x", false), candidate("b", b"y", true)];
        assert_eq!(pick_alias(&cands, "a").unwrap().ws_name, "b");
        assert!(pick_alias(&cands[..1], "a").is_none());
    }

    // Restore-rule tests on real files (no jj repo needed): guards are built
    // via struct literals, exercising process_file/apply directly.

    fn guard_with(rel: &str, cands: Vec<Candidate>, restore_enabled: bool) -> XattrGuard {
        XattrGuard {
            restore_enabled,
            candidates: BTreeMap::from([(rel.to_string(), cands)]),
            hard_links: BTreeMap::new(),
        }
    }

    fn bookmark_content(payload: &[u8]) -> Vec<u8> {
        let mut content = BOOKMARK_MAGIC.to_vec();
        content.extend_from_slice(payload);
        content
    }

    #[test]
    fn restore_applies_finder_info_on_exact_content_match() {
        let dir = tempfile::tempdir().unwrap();
        let content = bookmark_content(b"payload");
        std::fs::write(dir.path().join("a"), &content).unwrap();
        let guard = guard_with("a", vec![candidate("ws", &content, true)], true);
        let warnings = guard.restore(&[("ws".to_string(), dir.path().to_path_buf())]);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        let info = get_xattr(&dir.path().join("a"), FINDER_INFO_XATTR)
            .unwrap()
            .unwrap();
        assert!(finder_info_marks_alias(&info));
    }

    #[test]
    fn restore_applies_flag_only_on_retargeted_bookmark() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), bookmark_content(b"new-target")).unwrap();
        let mut cand = candidate("ws", &bookmark_content(b"old-target"), true);
        cand.resource_fork = Some(b"stale fork".to_vec());
        let guard = guard_with("a", vec![cand], true);
        let warnings = guard.restore(&[("ws".to_string(), dir.path().to_path_buf())]);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        let path = dir.path().join("a");
        assert!(finder_info_marks_alias(
            &get_xattr(&path, FINDER_INFO_XATTR).unwrap().unwrap()
        ));
        assert_eq!(get_xattr(&path, RESOURCE_FORK_XATTR).unwrap(), None);
    }

    #[test]
    fn restore_leaves_plain_replacement_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"plain text now").unwrap();
        let guard = guard_with(
            "a",
            vec![candidate("ws", &bookmark_content(b"old"), true)],
            true,
        );
        let warnings = guard.restore(&[("ws".to_string(), dir.path().to_path_buf())]);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(
            get_xattr(&dir.path().join("a"), FINDER_INFO_XATTR).unwrap(),
            None
        );
    }

    #[test]
    fn restore_disabled_warns_instead_of_writing() {
        let dir = tempfile::tempdir().unwrap();
        let content = bookmark_content(b"payload");
        std::fs::write(dir.path().join("a"), &content).unwrap();
        let guard = guard_with("a", vec![candidate("ws", &content, true)], false);
        let warnings = guard.restore(&[("ws".to_string(), dir.path().to_path_buf())]);
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(
            warnings[0].contains("preserve-finder-xattrs = false"),
            "got: {warnings:?}"
        );
        assert_eq!(
            get_xattr(&dir.path().join("a"), FINDER_INFO_XATTR).unwrap(),
            None
        );
    }

    #[test]
    fn restore_skips_file_with_existing_finder_info() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        std::fs::write(&path, bookmark_content(b"payload")).unwrap();
        let mut user_set = alias_finder_info();
        user_set[9] = 0x01; // distinguishable from the captured value
        set_xattr(&path, FINDER_INFO_XATTR, &user_set).unwrap();
        let guard = guard_with(
            "a",
            vec![candidate("ws", &bookmark_content(b"payload"), true)],
            true,
        );
        let warnings = guard.restore(&[("ws".to_string(), dir.path().to_path_buf())]);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(
            get_xattr(&path, FINDER_INFO_XATTR).unwrap().unwrap(),
            user_set
        );
    }

    #[test]
    fn orphan_bookmark_data_warns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), bookmark_content(b"payload")).unwrap();
        let guard = guard_with("a", Vec::new(), true);
        let warnings = guard.restore(&[("ws".to_string(), dir.path().to_path_buf())]);
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(warnings[0].contains("alias flag lost"), "got: {warnings:?}");
    }
}
