//! Central logging for every `zeron` process.
//!
//! Guarantees:
//! - `RUST_LOG` always wins; otherwise long-running modes default to `info`
//!   (with noisy `loro` internals quieted) and one-shot CLIs to `warn`.
//! - `headed` / `headless` / `mcp` mirror logs to
//!   `{data_dir}/logs/zeron-{mode}-YYYY-MM-DD.log` (kept 7 days, 25 MiB cap
//!   per file). `mcp` never touches stdout — it owns it for JSON-RPC.
//! - Every mode installs a panic hook that logs payload/location/backtrace
//!   through the same layers, so Finder/systemd launches keep diagnostics
//!   in the log file.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const KEEP_DAYS: u64 = 7;
const MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Which process shape is running. Controls the default filter, the log file
/// name, and whether file logging applies.
#[derive(Copy, Clone)]
pub enum LogMode {
    Headed,
    Headless,
    Mcp,
    Cli,
}

impl LogMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Headed => "headed",
            Self::Headless => "headless",
            Self::Mcp => "mcp",
            Self::Cli => "cli",
        }
    }

    fn default_filter(self) -> &'static str {
        match self {
            Self::Headed | Self::Headless => "info,loro_internal=warn,loro=warn",
            Self::Mcp | Self::Cli => "warn",
        }
    }

    /// `mcp` owns stdout for the protocol; one-shot CLIs print to stdout
    /// normally and don't need a file.
    fn writes_file(self) -> bool {
        !matches!(self, Self::Cli)
    }
}

/// Initialise global logging + the crash hook. Call once, first in `main`.
/// The file writer lives inside the subscriber layers (which hold `Arc`
/// clones for the process lifetime), so there is nothing to keep alive.
pub fn init(mode: LogMode, data_dir: &Path) {
    let log_dir = data_dir.join("logs");
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| mode.default_filter().into());

    if !mode.writes_file() {
        // One-shot CLIs create no logs, but inherited logs must still not
        // outlive KEEP_DAYS when only CLI commands ever run.
        prune_old_logs(&log_dir);
    }
    let file_writer = mode
        .writes_file()
        .then(|| open_log_file(&log_dir, mode))
        .flatten()
        .map(Arc::new);

    let registry = tracing_subscriber::registry().with(filter);
    // stderr everywhere: `mcp` owns stdout for JSON-RPC, and headed apps
    // launched from Finder/systemd have no visible console anyway (the
    // file layer is the durable record) — so piped stdout stays clean.
    let console = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let file_layer = file_writer.map(|f| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(f)
    });
    registry.with(console).with(file_layer).init();

    install_panic_hook();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        pid = std::process::id(),
        mode = mode.as_str(),
        data_dir = %data_dir.display(),
        "zeron starting"
    );
}

// ---------------------------------------------------------------------------
// Log files: daily rotation, size cap, flock-safe
// ---------------------------------------------------------------------------

/// `{dir}/zeron-{mode}-YYYY-MM-DD.log`, previous days pruned after KEEP_DAYS.
/// When the canonical file is flock-held by a live process (unix), a
/// pid-suffixed overflow file is used so a second instance never rotates a
/// live writer's log away (2026-08-04 incident). Files over MAX_BYTES rotate
/// to `.1` before append.
fn open_log_file(dir: &Path, mode: LogMode) -> Option<File> {
    fs::create_dir_all(dir).ok()?;
    prune_old_logs(dir);
    let path = daily_path(dir, mode, chrono::Local::now().date_naive());
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // Open the handle we will write through, then lock THAT handle: a
        // racer opens the same inode and fails here, so it can never rotate
        // a live writer's log away (2026-08-04 incident). The check and the
        // writer are one handle — no probe-then-reopen window where a second
        // launch slips between the lock check and the real open.
        let try_lock =
            |f: &File| unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 };
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        if !try_lock(&file) {
            return overflow_log(dir, mode);
        }
        // Only the lock holder rotates, and it renames while still holding
        // the lock — renaming under a live writer would orphan its inode.
        // `file` still points at the rotated inode afterwards, so reopen
        // the canonical path fresh. A racer that slips between the rename
        // and the reopen wins the fresh inode; our lock then fails and we
        // fall back to overflow. Either way nobody shares or orphans a
        // live log.
        if fs::metadata(&path)
            .map(|m| m.len() > MAX_BYTES)
            .unwrap_or(false)
        {
            rotate_if_oversize(&path, MAX_BYTES);
            drop(file);
            let fresh = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()?;
            if !try_lock(&fresh) {
                return overflow_log(dir, mode);
            }
            return Some(fresh);
        }
        Some(file)
    }
    #[cfg(not(unix))]
    {
        rotate_if_oversize(&path, MAX_BYTES);
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
    }
}

/// Pid-suffixed fallback for a launch that raced a live writer for the
/// canonical log; swept by [`prune_old_logs`] after a week (mtime-based).
#[cfg(unix)]
fn overflow_log(dir: &Path, mode: LogMode) -> Option<File> {
    File::create(dir.join(format!(
        "zeron-{}.{}.log",
        mode.as_str(),
        std::process::id()
    )))
    .ok()
}

fn daily_path(dir: &Path, mode: LogMode, date: chrono::NaiveDate) -> PathBuf {
    dir.join(format!(
        "zeron-{}-{}.log",
        mode.as_str(),
        date.format("%Y-%m-%d")
    ))
}

fn rotate_if_oversize(path: &Path, limit: u64) {
    let Ok(meta) = fs::metadata(path) else { return };
    if meta.len() <= limit {
        return;
    }
    let rotated = path.with_extension("log.1");
    let _ = fs::rename(path, rotated);
}

/// Sweep stale logs by mtime: any `zeron-*.log` older than KEEP_DAYS goes.
/// Covers daily files and pid-overflow files from a raced launch alike.
fn prune_old_logs(dir: &Path) {
    let max_age = std::time::Duration::from_secs(KEEP_DAYS * 86400);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(n) = name.to_str() else { continue };
        if !(n.starts_with("zeron-") && (n.ends_with(".log") || n.ends_with(".log.1"))) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Panic hook: same sinks as everything else (stderr + log file)
// ---------------------------------------------------------------------------

/// Panic payloads are `&str` / `String` / opaque — extract anything readable.
fn extract_panic_message(payload: &dyn std::any::Any) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let backtrace = std::backtrace::Backtrace::force_capture();
        let message = extract_panic_message(info.payload());
        tracing::error!(%message, %location, %backtrace, "application panic");
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_log_name_shape() {
        let dir = Path::new("/tmp");
        let date = chrono::NaiveDate::from_ymd_opt(2026, 9, 25).unwrap();
        assert_eq!(
            daily_path(dir, LogMode::Headless, date),
            PathBuf::from("/tmp/zeron-headless-2026-09-25.log")
        );
    }

    #[test]
    fn oversize_log_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zeron-cli-test.log");
        fs::write(&path, vec![b'x'; 64]).unwrap();
        rotate_if_oversize(&path, 16);
        assert!(!path.exists());
        assert!(dir.path().join("zeron-cli-test.log.1").is_file());
        // Under the cap: untouched.
        let small = dir.path().join("zeron-cli-small.log");
        fs::write(&small, vec![b'x'; 8]).unwrap();
        rotate_if_oversize(&small, 16);
        assert!(small.is_file());
    }

    #[test]
    fn old_logs_pruned_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("zeron-headed-2026-09-25.log");
        let stale = dir.path().join("zeron-headed-2026-01-01.log");
        let keeper = dir.path().join("notes.txt");
        fs::write(&fresh, "new").unwrap();
        fs::write(&stale, "old").unwrap();
        fs::write(&keeper, "mine").unwrap();
        let old_time =
            std::time::SystemTime::now() - std::time::Duration::from_secs((KEEP_DAYS + 1) * 86400);
        fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        prune_old_logs(dir.path());
        assert!(fresh.exists());
        assert!(!stale.exists());
        assert!(keeper.exists());
    }

    #[test]
    fn string_panic_payloads_extracted() {
        assert_eq!(extract_panic_message(&"static"), "static");
        assert_eq!(extract_panic_message(&String::from("owned")), "owned");
        assert_eq!(extract_panic_message(&42u32), "<non-string panic payload>");
    }
}
