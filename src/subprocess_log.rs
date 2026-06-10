//! Opt-in subprocess call logging.
//!
//! Set `JI_LOG=1` to log to `.ji/subprocess.log`, or `JI_LOG=<path>`
//! for a custom location. No log rotation — delete the file manually.

use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// Holds the open log file, or `None` if logging is disabled or init failed.
static LOG: OnceLock<Option<Mutex<BufWriter<std::fs::File>>>> = OnceLock::new();

/// Initialize subprocess logging based on the `JI_LOG` environment variable.
///
/// - `JI_LOG=1` → log to `repo_root/.ji/subprocess.log`
/// - `JI_LOG=<path>` → log to that path
/// - Unset or empty → no logging
pub fn init(repo_root: &Path) {
    let val = match std::env::var("JI_LOG") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            let _ = LOG.set(None);
            return;
        }
    };

    let path = if val == "1" {
        if let Err(e) = crate::jj_utils::ensure_ji_dir(repo_root) {
            eprintln!("(ji)::log: failed to create .ji directory: {e:#}");
            let _ = LOG.set(None);
            return;
        }
        repo_root.join(".ji").join("subprocess.log")
    } else {
        std::path::PathBuf::from(val)
    };

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(file) => {
            let _ = LOG.set(Some(Mutex::new(BufWriter::new(file))));
        }
        Err(e) => {
            eprintln!("(ji)::log: cannot open {}: {e}", path.display());
            let _ = LOG.set(None);
        }
    }
}

/// Log a jj subprocess call. No-op if logging is not enabled.
pub fn log_subprocess(label: &str, duration: Duration) {
    let Some(Some(mtx)) = LOG.get() else {
        return;
    };
    let ts = humantime::format_rfc3339_millis(SystemTime::now());
    let ms = duration.as_millis();
    let line = format!("{ts}  {ms:>6}ms  jj {label}\n");
    if let Ok(mut w) = mtx.lock() {
        let _ = w.write_all(line.as_bytes());
        let _ = w.flush();
    }
}

/// Log a hook subprocess call. No-op if logging is not enabled.
pub fn log_hook(hook_label: &str, cmd: &str, duration: Duration) {
    let Some(Some(mtx)) = LOG.get() else {
        return;
    };
    let ts = humantime::format_rfc3339_millis(SystemTime::now());
    let ms = duration.as_millis();
    let line = format!("{ts}  {ms:>6}ms  hook:{hook_label}  {cmd}\n");
    if let Ok(mut w) = mtx.lock() {
        let _ = w.write_all(line.as_bytes());
        let _ = w.flush();
    }
}
