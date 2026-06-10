use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Maximum persisted action records. Oldest entries are dropped when full.
const MAX_RECORDS: usize = 50;

/// A recorded ji action with pre/post jj operation IDs.
#[derive(Serialize, Deserialize)]
pub struct ActionRecord {
    pub label: String,
    pub pre_op_id: String,
    pub post_op_id: String,
}

/// On-disk representation of the action history.
#[derive(Serialize, Deserialize)]
struct PersistedHistory {
    cursor: usize,
    #[serde(default)]
    last_op_head: Option<String>,
    records: Vec<ActionRecord>,
}

/// Undo/redo stack for ji compound actions, persisted across sessions.
///
/// Each ji action (sync, create, close, etc.) runs multiple jj commands.
/// This records the jj op_head before and after, allowing undo/redo of the
/// entire compound action as a single step via `jj op restore`.
#[derive(Default)]
pub struct ActionHistory {
    records: Vec<ActionRecord>,
    /// Cursor into the undo stack. `records[0..cursor]` are applied,
    /// `records[cursor..]` are redo-able.
    cursor: usize,
    /// The op head after the most recent mutation (record/undo/redo).
    /// Used to detect if external jj work happened since the last ji action.
    pub last_op_head: Option<String>,
}

impl ActionHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load from a JSON file. Returns an empty history on any error.
    pub fn load(path: &Path) -> Self {
        let Ok(data) = std::fs::read_to_string(path) else {
            return Self::new();
        };
        let Ok(persisted) = serde_json::from_str::<PersistedHistory>(&data) else {
            return Self::new();
        };
        let cursor = persisted.cursor.min(persisted.records.len());
        Self {
            records: persisted.records,
            cursor,
            last_op_head: persisted.last_op_head,
        }
    }

    /// Persist to a JSON file. Writes atomically via a temp file rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        let persisted = PersistedHistory {
            cursor: self.cursor,
            last_op_head: self.last_op_head.clone(),
            records: self
                .records
                .iter()
                .map(|r| ActionRecord {
                    label: r.label.clone(),
                    pre_op_id: r.pre_op_id.clone(),
                    post_op_id: r.post_op_id.clone(),
                })
                .collect(),
        };
        let json =
            serde_json::to_string_pretty(&persisted).context("failed to serialize history")?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json).context("failed to write history temp file")?;
        std::fs::rename(&tmp, path).context("failed to rename history temp file")?;
        Ok(())
    }

    /// Record a completed action. Truncates any redo-able entries.
    pub fn record(&mut self, label: String, pre: String, post: String) {
        self.records.truncate(self.cursor);
        self.last_op_head = Some(post.clone());
        self.records.push(ActionRecord {
            label,
            pre_op_id: pre,
            post_op_id: post,
        });
        self.cursor = self.records.len();
        // Enforce cap: drop oldest when full.
        if self.records.len() > MAX_RECORDS {
            self.records.remove(0);
            self.cursor = self.cursor.saturating_sub(1);
        }
    }

    /// Record if both pre/post are `Some` and differ. No-ops otherwise.
    pub fn maybe_record(
        &mut self,
        label: impl Into<String>,
        pre: Option<String>,
        post: Option<String>,
    ) {
        if let (Some(pre), Some(post)) = (pre, post)
            && pre != post
        {
            self.record(label.into(), pre, post);
        }
    }

    /// Undo: returns `(op_id_to_restore, label)`.
    pub fn undo(&mut self) -> Option<(&str, &str)> {
        if self.cursor == 0 {
            return None;
        }
        self.cursor -= 1;
        let r = &self.records[self.cursor];
        Some((&r.pre_op_id, &r.label))
    }

    /// Redo: returns `(op_id_to_restore, label)`.
    pub fn redo(&mut self) -> Option<(&str, &str)> {
        if self.cursor >= self.records.len() {
            return None;
        }
        let r = &self.records[self.cursor];
        self.cursor += 1;
        Some((&r.post_op_id, &r.label))
    }

    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_redo(&self) -> bool {
        self.cursor < self.records.len()
    }

    /// The applied (undo-able) action records.
    pub fn applied_records(&self) -> &[ActionRecord] {
        &self.records[..self.cursor]
    }
}
