//! Private local policy storage. Reading never creates files; writing never runs a process action.
//!
//! Lock files keep stable inodes. The locks protect independent state, journal, and future
//! execution lanes; they are released by the OS on process exit and are never replayed.

use crate::policy::{PolicyState, RuleChange};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs::{self, DirBuilder, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_STATE_BYTES: usize = 1024 * 1024;
pub const MAX_JOURNAL_BYTES: usize = 10 * 1024 * 1024;
pub const JOURNAL_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// State and journal contention must return control to the interactive caller.
pub const LOCK_WAIT_MS: u64 = 250;
const STATE: &str = "state.json";
const JOURNAL: &str = "journal.jsonl";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreProbe {
    pub root: PathBuf,
    pub state_path: PathBuf,
    pub journal_path: PathBuf,
    pub root_status: String,
    pub state_status: String,
    pub journal_status: String,
    pub revision: Option<u64>,
    pub rule_count: Option<usize>,
    pub error: Option<String>,
    /// A read-only doctor does not promise that a later write will succeed.
    pub write_status: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub schema_version: u32,
    pub timestamp_unix_ms: u64,
    pub event: String,
    pub data: serde_json::Value,
}

/// Holding this value owns the single execution lane. Dropping it releases the lock.
#[derive(Debug)]
pub struct ExecutionGuard {
    _lock: FileLock,
}

impl Store {
    pub fn from_env() -> Result<Self, String> {
        let root = match std::env::var_os("BREE_DATA_DIR") {
            Some(path) => PathBuf::from(path),
            None => {
                let home = std::env::var_os("HOME").ok_or_else(|| {
                    "Cannot determine the user directory; set BREE_DATA_DIR to an absolute path"
                        .to_string()
                })?;
                PathBuf::from(home).join("Library/Application Support/Bree")
            }
        };
        if !root.is_absolute() {
            return Err("Bree data directory must be an absolute path".to_string());
        }
        Ok(Self::at(root))
    }

    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Missing state is the only condition that yields an empty default.
    pub fn load(&self) -> Result<PolicyState, String> {
        let Some(dir) = self.open_root(false)? else {
            return Ok(PolicyState::default());
        };
        load_state(&dir)
    }

    /// Re-read under the configuration lock to avoid lost updates across CLI instances.
    pub fn change(&self, change: RuleChange) -> Result<PolicyState, String> {
        let dir = self.required_root()?;
        let _lock = FileLock::acquire(&dir, "state.lock", false)?;
        let current = load_state(&dir)?;
        let next = current.apply(change, now_ms()?)?;
        next.validate()?;
        let mut encoded =
            serde_json::to_vec_pretty(&next).map_err(|e| format!("Failed to encode rules: {e}"))?;
        encoded.push(b'\n');
        if encoded.len() > MAX_STATE_BYTES {
            return Err("Rule file exceeds the 1 MiB limit".to_string());
        }
        // A durable attempt is required before changing state. It is deliberately not a
        // success event: an atomic state write can still fail afterwards.
        self.append_record_at(
            &dir,
            "rules_change_attempt",
            serde_json::json!({ "revision": next.revision, "rule_count": next.rules.len() }),
            now_ms()?,
        )?;
        dir.atomic_write(STATE, &encoded)?;
        Ok(next)
    }

    /// Read the latest policy under the same lock used by `change`, and retain that
    /// lock for the entire operation. A caller that also writes a necessary journal
    /// record must keep the established state → journal acquisition order.
    ///
    /// This method does not change state or dispatch any action itself. Contention
    /// or invalid state returns an error before the closure can run.
    pub fn with_state_lock<T>(
        &self,
        operation: impl FnOnce(&PolicyState) -> Result<T, String>,
    ) -> Result<T, String> {
        let dir = self.required_root()?;
        let _lock = FileLock::acquire(&dir, "state.lock", false)?;
        let state = load_state(&dir)?;
        operation(&state)
    }

    pub fn execution_lock(&self) -> Result<ExecutionGuard, String> {
        let dir = self.required_root()?;
        Ok(ExecutionGuard {
            _lock: FileLock::acquire(&dir, "execution.lock", true)?,
        })
    }

    /// Callers supply only policy/preview summaries, never command lines, environment,
    /// prompts, chat content, or application document content.
    pub fn append_record(&self, event: &str, data: serde_json::Value) -> Result<(), String> {
        let dir = self.required_root()?;
        self.append_record_at(&dir, event, data, now_ms()?)
    }

    /// Return retained records in reverse append order. A read never creates locks
    /// or rewrites expired records. All existing lines are checked before limiting,
    /// so a small limit (including zero) cannot hide a corrupt or partial journal.
    pub fn records(&self, limit: usize) -> Result<Vec<Record>, String> {
        let Some(dir) = self.open_root(false)? else {
            return Ok(Vec::new());
        };
        self.records_at(&dir, limit, now_ms()?)
    }

    fn records_at(&self, dir: &RootDir, limit: usize, now: u64) -> Result<Vec<Record>, String> {
        let Some(bytes) = dir.read_file(JOURNAL, MAX_JOURNAL_BYTES)? else {
            return Ok(Vec::new());
        };
        let cutoff = now.saturating_sub(JOURNAL_RETENTION_MS);
        journal_lines(&bytes)?
            .into_iter()
            .enumerate()
            .rev()
            .filter(|(_, (_, timestamp))| *timestamp >= cutoff)
            .take(limit)
            .map(|(index, (line, _))| decode_record(line, index + 1))
            .collect()
    }

    /// Inspect the store without creating a directory, a lock, or a write probe.
    pub fn probe(&self) -> StoreProbe {
        let mut probe = StoreProbe {
            root: self.root.clone(),
            state_path: self.root.join(STATE),
            journal_path: self.root.join(JOURNAL),
            root_status: "missing".to_string(),
            state_status: "default".to_string(),
            journal_status: "missing".to_string(),
            revision: Some(0),
            rule_count: Some(0),
            error: None,
            write_status: "not_probed".to_string(),
        };
        let dir = match self.open_root(false) {
            Ok(None) => return probe,
            Ok(Some(dir)) => {
                probe.root_status = "ready".to_string();
                dir
            }
            Err(e) => {
                probe.root_status = "unreadable".to_string();
                probe.state_status = "unreadable".to_string();
                probe.journal_status = "unreadable".to_string();
                probe.revision = None;
                probe.rule_count = None;
                probe.error = Some(e);
                return probe;
            }
        };
        match dir.read_file(STATE, MAX_STATE_BYTES) {
            Ok(None) => {}
            Ok(Some(bytes)) => match decode_state(&bytes) {
                Ok(state) => {
                    probe.state_status = "valid".to_string();
                    probe.revision = Some(state.revision);
                    probe.rule_count = Some(state.rules.len());
                }
                Err(e) => {
                    probe.state_status = "invalid".to_string();
                    probe.revision = None;
                    probe.rule_count = None;
                    probe.error = Some(e);
                }
            },
            Err(e) => {
                probe.state_status = "unreadable".to_string();
                probe.revision = None;
                probe.rule_count = None;
                probe.error = Some(e);
            }
        }
        match dir.read_file(JOURNAL, MAX_JOURNAL_BYTES) {
            Ok(None) => {}
            Ok(Some(bytes)) => match journal_lines(&bytes) {
                Ok(_) => probe.journal_status = "valid".to_string(),
                Err(e) => {
                    probe.journal_status = "invalid".to_string();
                    probe.error.get_or_insert(e);
                }
            },
            Err(e) => {
                probe.journal_status = "unreadable".to_string();
                probe.error.get_or_insert(e);
            }
        }
        probe
    }

    fn open_root(&self, create: bool) -> Result<Option<RootDir>, String> {
        if !self.root.is_absolute() {
            return Err("Bree data directory must be an absolute path".to_string());
        }
        match fs::symlink_metadata(&self.root) {
            Ok(meta) => check_private(&meta, true, "Bree data directory")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&self.root)
                    .map_err(|e| format!("Cannot create the Bree data directory: {e}"))?;
            }
            Err(e) => return Err(format!("Cannot read the Bree data directory: {e}")),
        }
        let path = CString::new(self.root.as_os_str().as_encoded_bytes())
            .map_err(|_| "Data directory contains invalid characters".to_string())?;
        // SAFETY: path is NUL-terminated, flags reject a symlink at the managed root;
        // the returned descriptor is exclusively transferred into File on success.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(format!(
                "Cannot open the Bree data directory: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: open returned an owned, valid descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        check_private(
            &file
                .metadata()
                .map_err(|e| format!("Cannot inspect the data directory: {e}"))?,
            true,
            "Bree data directory",
        )?;
        Ok(Some(RootDir { file }))
    }

    fn required_root(&self) -> Result<RootDir, String> {
        self.open_root(true)?
            .ok_or_else(|| "Cannot open the Bree data directory".to_string())
    }

    fn append_record_at(
        &self,
        dir: &RootDir,
        event: &str,
        data: serde_json::Value,
        now: u64,
    ) -> Result<(), String> {
        if event.is_empty() || event.len() > 128 || event.chars().any(char::is_control) {
            return Err("Journal event must be a printable name of 1–128 bytes".to_string());
        }
        let record = Record {
            schema_version: 1,
            timestamp_unix_ms: now,
            event: event.to_string(),
            data,
        };
        let mut encoded = serde_json::to_vec(&record)
            .map_err(|e| format!("Failed to encode the journal record: {e}"))?;
        encoded.push(b'\n');
        if encoded.len() > MAX_JOURNAL_BYTES {
            return Err("One journal record exceeds the 10 MiB limit".to_string());
        }
        let _lock = FileLock::acquire(dir, "journal.lock", false)?;
        let previous = dir
            .read_file(JOURNAL, MAX_JOURNAL_BYTES)?
            .unwrap_or_default();
        let output = rotate_journal(&previous, &encoded, now)?;
        dir.atomic_write(JOURNAL, &output)
    }
}

fn check_private(meta: &fs::Metadata, directory: bool, label: &str) -> Result<(), String> {
    if meta.file_type().is_symlink()
        || if directory {
            !meta.is_dir()
        } else {
            !meta.is_file()
        }
    {
        return Err(format!(
            "{label} must be a regular {}, not a symbolic link",
            if directory { "directory" } else { "file" }
        ));
    }
    // SAFETY: geteuid takes no arguments and returns the current effective user ID.
    let own_uid = unsafe { libc::geteuid() };
    if meta.uid() != own_uid || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{label} has unsafe ownership or permissions; directories require 0700 and files require 0600"
        ));
    }
    if !directory && meta.nlink() > 1 {
        return Err(format!("{label} must not be a hard link"));
    }
    Ok(())
}

struct RootDir {
    file: File,
}

impl RootDir {
    fn open_file(&self, name: &str, flags: i32) -> Result<Option<File>, String> {
        let name_c = CString::new(name).map_err(|_| "Invalid file name".to_string())?;
        let mut creation_retries = 0;
        let fd = loop {
            // SAFETY: the root descriptor is a checked private directory. name is a
            // constant/generated single path component, and O_NOFOLLOW rejects symlinks.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name_c.as_ptr(),
                    flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                    0o600,
                )
            };
            if fd >= 0 {
                break fd;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::NotFound && flags & libc::O_CREAT == 0 {
                return Ok(None);
            }
            // The tested macOS can return ENOENT during simultaneous creation of a
            // lock leaf. Retry only this observed race, with the same root and flags.
            if e.kind() == std::io::ErrorKind::NotFound
                && flags & libc::O_CREAT != 0
                && creation_retries < 2
            {
                creation_retries += 1;
                continue;
            }
            return Err(format!("Cannot open {name}: {e}"));
        };
        // SAFETY: openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        check_private(
            &file
                .metadata()
                .map_err(|e| format!("Cannot inspect {name}: {e}"))?,
            false,
            name,
        )?;
        Ok(Some(file))
    }

    fn read_file(&self, name: &str, maximum: usize) -> Result<Option<Vec<u8>>, String> {
        let Some(file) = self.open_file(name, libc::O_RDONLY)? else {
            return Ok(None);
        };
        let mut data = Vec::new();
        file.take((maximum + 1) as u64)
            .read_to_end(&mut data)
            .map_err(|e| format!("Cannot read {name}: {e}"))?;
        if data.len() > maximum {
            return Err(format!("{name} exceeds the {}-byte limit", maximum));
        }
        Ok(Some(data))
    }

    fn atomic_write(&self, target: &str, bytes: &[u8]) -> Result<(), String> {
        // Reject unsafe existing targets before rename would otherwise replace them.
        let _ = self.open_file(target, libc::O_RDONLY)?;
        let name = format!(
            ".{target}.{}.{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let mut file = self
            .open_file(&name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?
            .ok_or_else(|| "Cannot create a temporary file".to_string())?;
        let mut temp = TempFile {
            dir: self,
            name,
            committed: false,
        };
        file.write_all(bytes)
            .map_err(|e| format!("Cannot write {target}: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("Cannot sync {target}: {e}"))?;
        let source = CString::new(temp.name.as_str())
            .map_err(|_| "Invalid temporary file name".to_string())?;
        let destination =
            CString::new(target).map_err(|_| "Invalid target file name".to_string())?;
        // SAFETY: both single-component names are valid C strings relative to the
        // same checked root descriptor. renameat atomically replaces only target.
        if unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                source.as_ptr(),
                self.file.as_raw_fd(),
                destination.as_ptr(),
            )
        } != 0
        {
            return Err(format!(
                "Cannot replace {target}: {}",
                std::io::Error::last_os_error()
            ));
        }
        temp.committed = true;
        self.file.sync_all().map_err(|e| {
            format!(
                "{target} was replaced, but directory sync failed; read it again to confirm: {e}"
            )
        })
    }
}

struct TempFile<'a> {
    dir: &'a RootDir,
    name: String,
    committed: bool,
}

impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        if !self.committed
            && let Ok(name) = CString::new(self.name.as_str())
        {
            // SAFETY: remove only our generated temporary leaf in the checked root.
            unsafe { libc::unlinkat(self.dir.file.as_raw_fd(), name.as_ptr(), 0) };
        }
    }
}

#[derive(Debug)]
struct FileLock {
    file: File,
}

impl FileLock {
    fn acquire(dir: &RootDir, name: &str, nonblocking: bool) -> Result<Self, String> {
        let file = dir
            .open_file(name, libc::O_RDWR | libc::O_CREAT)?
            .ok_or_else(|| format!("Cannot create {name}"))?;
        let operation = libc::LOCK_EX | libc::LOCK_NB;
        let started = Instant::now();
        let timeout = Duration::from_millis(LOCK_WAIT_MS);
        loop {
            // SAFETY: flock operates on the owned regular lock-file descriptor.
            if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
                return Ok(Self { file });
            }
            let e = std::io::Error::last_os_error();
            if nonblocking && e.kind() == std::io::ErrorKind::WouldBlock {
                return Err(
                    "Another Bree cleanup or dry run is in progress; wait for it to finish"
                        .to_string(),
                );
            }
            if !nonblocking
                && matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                )
            {
                let elapsed = started.elapsed();
                if elapsed >= timeout {
                    return Err(format!(
                        "{name} is busy; stopped after waiting {LOCK_WAIT_MS} ms. Try again later"
                    ));
                }
                // A short bounded wait replaces blocking flock, so external lock
                // holders cannot indefinitely stop Q / Ctrl-C handling in the UI.
                std::thread::sleep((timeout - elapsed).min(Duration::from_millis(10)));
                continue;
            }
            return Err(format!("Cannot lock {name}: {e}"));
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: this guard owns the descriptor and only releases its own lock.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn load_state(dir: &RootDir) -> Result<PolicyState, String> {
    match dir.read_file(STATE, MAX_STATE_BYTES)? {
        None => Ok(PolicyState::default()),
        Some(bytes) => decode_state(&bytes),
    }
}

fn decode_state(bytes: &[u8]) -> Result<PolicyState, String> {
    let state: PolicyState = serde_json::from_slice(bytes)
        .map_err(|e| format!("Rule file is corrupt; the original file is preserved: {e}"))?;
    state.validate()?;
    Ok(state)
}

fn journal_lines(bytes: &[u8]) -> Result<Vec<(&[u8], u64)>, String> {
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(
            "The last journal line is incomplete; the original file is preserved".to_string(),
        );
    }
    let mut records = Vec::new();
    for (index, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let record = decode_record(line, index + 1)?;
        records.push((line, record.timestamp_unix_ms));
    }
    Ok(records)
}

fn decode_record(line: &[u8], line_number: usize) -> Result<Record, String> {
    let record: Record = serde_json::from_slice(line).map_err(|e| {
        format!("Journal line {line_number} is corrupt; the original file is preserved: {e}")
    })?;
    if record.schema_version != 1
        || record.event.is_empty()
        || record.event.len() > 128
        || record.event.chars().any(char::is_control)
    {
        return Err(format!(
            "Journal line {line_number} has an unsupported format; the original file is preserved"
        ));
    }
    Ok(record)
}

fn rotate_journal(previous: &[u8], newest: &[u8], now: u64) -> Result<Vec<u8>, String> {
    if newest.len() > MAX_JOURNAL_BYTES {
        return Err("One journal record exceeds the 10 MiB limit".to_string());
    }
    let cutoff = now.saturating_sub(JOURNAL_RETENTION_MS);
    let records = journal_lines(previous)?;
    let kept: Vec<&[u8]> = records
        .into_iter()
        .filter_map(|(line, timestamp)| (timestamp >= cutoff).then_some(line))
        .collect();
    let mut size: usize = kept.iter().map(|line| line.len()).sum::<usize>() + newest.len();
    let mut start = 0;
    while size > MAX_JOURNAL_BYTES {
        size -= kept[start].len();
        start += 1;
    }
    let mut output = Vec::with_capacity(size);
    for line in &kept[start..] {
        output.extend_from_slice(line);
    }
    output.extend_from_slice(newest);
    Ok(output)
}

fn now_ms() -> Result<u64, String> {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| {
            "System time is before the Unix epoch; cannot record rule changes".to_string()
        })?
        .as_millis();
    u64::try_from(ms).map_err(|_| "System time exceeds the storage range".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{AppScope, RuleAction};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "bree-storage-{}-{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }

        fn store(&self) -> Store {
            Store::at(self.0.join("data"))
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            if self.0.join("data").is_dir() {
                let _ = fs::set_permissions(self.0.join("data"), fs::Permissions::from_mode(0o700));
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn add(index: usize) -> RuleChange {
        RuleChange::Add {
            action: RuleAction::Allow,
            scope: AppScope {
                bundle_id: format!("dev.bree.fixture{index}"),
                bundle_path: format!("/Applications/Fixture{index}.app"),
                executable_path: format!("/Applications/Fixture{index}.app/Contents/MacOS/Fixture"),
            },
        }
    }

    fn private_write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn record(timestamp: u64, event: &str) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&Record {
            schema_version: 1,
            timestamp_unix_ms: timestamp,
            event: event.to_string(),
            data: serde_json::json!({ "padding": "" }),
        })
        .unwrap();
        bytes.push(b'\n');
        bytes
    }

    fn sized_record(timestamp: u64, size: usize) -> Vec<u8> {
        let base = record(timestamp, "preview");
        let padding = "x".repeat(size - base.len());
        let mut bytes = serde_json::to_vec(&Record {
            schema_version: 1,
            timestamp_unix_ms: timestamp,
            event: "preview".to_string(),
            data: serde_json::json!({ "padding": padding }),
        })
        .unwrap();
        bytes.push(b'\n');
        assert_eq!(bytes.len(), size);
        bytes
    }

    #[test]
    fn reading_missing_store_has_no_disk_side_effects() {
        let root = TestRoot::new();
        let store = root.store();
        assert_eq!(store.load().unwrap(), PolicyState::default());
        assert!(store.records(20).unwrap().is_empty());
        assert!(store.records(0).unwrap().is_empty());
        let probe = store.probe();
        assert_eq!(probe.root_status, "missing");
        assert_eq!(probe.write_status, "not_probed");
        assert!(!store.root().exists());
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 0);
    }

    #[test]
    fn history_missing_journal_is_read_only_and_limit_keeps_newest_append() {
        let root = TestRoot::new();
        let store = root.store();
        let dir = store.required_root().unwrap();
        assert!(store.records(10).unwrap().is_empty());
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
        let now = now_ms().unwrap();
        store
            .append_record_at(
                &dir,
                "cleanup_started",
                serde_json::json!({ "run_id": "fixture", "count": 1 }),
                now,
            )
            .unwrap();
        store
            .append_record_at(
                &dir,
                "target_request_prepared",
                serde_json::json!(["fixture", 1]),
                now,
            )
            .unwrap();
        store
            .append_record_at(
                &dir,
                "target_request_outcome",
                serde_json::json!({ "requested": true, "exited": false }),
                now,
            )
            .unwrap();
        store
            .append_record_at(&dir, "cleanup_finished", serde_json::json!(null), now)
            .unwrap();
        store
            .append_record_at(
                &dir,
                "cancelled",
                serde_json::json!({ "requested": false }),
                now,
            )
            .unwrap();
        let previous = fs::read(store.root().join(JOURNAL)).unwrap();
        let limited = store.records(2).unwrap();
        assert_eq!(
            limited
                .iter()
                .map(|record| record.event.as_str())
                .collect::<Vec<_>>(),
            ["cancelled", "cleanup_finished"]
        );
        assert_eq!(limited[0].data, serde_json::json!({ "requested": false }));
        assert!(limited[1].data.is_null());
        assert_eq!(
            serde_json::to_value(limited[0].clone()).unwrap()["schema_version"],
            1
        );
        assert_eq!(store.records(20).unwrap().len(), 5);
        assert!(store.records(0).unwrap().is_empty());
        assert_eq!(fs::read(store.root().join(JOURNAL)).unwrap(), previous);
        assert!(!store.root().join("state.lock").exists());
        assert!(!store.root().join(STATE).exists());
    }

    #[test]
    fn history_filters_expired_records_and_keeps_exact_seven_day_boundary() {
        let root = TestRoot::new();
        let store = root.store();
        let dir = store.required_root().unwrap();
        let now = JOURNAL_RETENTION_MS + 1000;
        let mut original = record(999, "expired");
        original.extend(record(1000, "boundary"));
        original.extend(record(1001, "recent"));
        private_write(&store.root().join(JOURNAL), &original);
        let history = store.records_at(&dir, 20, now).unwrap();
        assert_eq!(
            history
                .iter()
                .map(|record| record.event.as_str())
                .collect::<Vec<_>>(),
            ["recent", "boundary"]
        );
        assert_eq!(store.records_at(&dir, 1, now).unwrap()[0].event, "recent");
        assert_eq!(fs::read(store.root().join(JOURNAL)).unwrap(), original);
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 1);
    }

    #[test]
    fn history_rejects_corruption_even_when_limit_would_hide_it() {
        let root = TestRoot::new();
        let store = root.store();
        store.required_root().unwrap();
        let mut corrupt_old_line = b"invalid old record\n".to_vec();
        corrupt_old_line.extend(record(now_ms().unwrap(), "cleanup_finished"));
        for corrupt in [
            corrupt_old_line,
            b"{}".to_vec(),
            b"\n".to_vec(),
            br#"{"schema_version":2,"timestamp_unix_ms":0,"event":"cleanup_started","data":null}
"#
            .to_vec(),
        ] {
            private_write(&store.root().join(JOURNAL), &corrupt);
            assert!(store.records(1).is_err());
            assert!(store.records(0).is_err());
            assert_eq!(fs::read(store.root().join(JOURNAL)).unwrap(), corrupt);
            assert_eq!(fs::read_dir(store.root()).unwrap().count(), 1);
        }
    }

    #[test]
    fn rule_add_remove_roundtrip_keeps_private_files() {
        let root = TestRoot::new();
        let store = root.store();
        let added = store.change(add(1)).unwrap();
        assert_eq!(added.revision, 1);
        assert_eq!(store.load().unwrap(), added);
        let removed = store
            .change(RuleChange::Remove {
                id: added.rules[0].id.clone(),
            })
            .unwrap();
        assert_eq!(removed.revision, 2);
        assert!(removed.rules.is_empty());
        assert_eq!(fs::metadata(store.root()).unwrap().mode() & 0o777, 0o700);
        for entry in fs::read_dir(store.root()).unwrap() {
            let meta = entry.unwrap().metadata().unwrap();
            assert_eq!(meta.mode() & 0o777, 0o600);
        }
        let log = fs::read(store.root().join(JOURNAL)).unwrap();
        let records = journal_lines(&log).unwrap();
        assert_eq!(records.len(), 2);
        assert!(!String::from_utf8(log).unwrap().contains("/Applications/"));
    }

    #[test]
    fn corrupt_empty_and_unknown_schema_are_preserved_not_reset() {
        for corrupt in [
            b"".as_slice(),
            b"{",
            br#"{"schema_version":2,"revision":0,"rules":[]}"#,
        ] {
            let root = TestRoot::new();
            let store = root.store();
            store.required_root().unwrap();
            private_write(&store.root().join(STATE), corrupt);
            assert!(store.load().is_err());
            assert!(store.change(add(1)).is_err());
            let mut called = false;
            assert!(
                store
                    .with_state_lock(|_| {
                        called = true;
                        Ok(())
                    })
                    .is_err()
            );
            assert!(!called);
            assert_eq!(fs::read(store.root().join(STATE)).unwrap(), corrupt);
            assert!(!store.root().join(JOURNAL).exists());
            assert_eq!(store.probe().state_status, "invalid");
        }
    }

    #[test]
    fn bad_required_log_prevents_state_commit_and_is_preserved() {
        let root = TestRoot::new();
        let store = root.store();
        store.change(add(1)).unwrap();
        let original = fs::read(store.root().join(STATE)).unwrap();
        for bad in [
            b"garbage\n".as_slice(),
            b"{}",
            b"\n",
            br#"{"schema_version":9,"timestamp_unix_ms":0,"event":"preview","data":null}
"#,
        ] {
            private_write(&store.root().join(JOURNAL), bad);
            assert!(store.change(add(2)).is_err());
            assert!(
                store
                    .append_record("preview", serde_json::json!({}))
                    .is_err()
            );
            assert_eq!(fs::read(store.root().join(JOURNAL)).unwrap(), bad);
            assert_eq!(fs::read(store.root().join(STATE)).unwrap(), original);
        }
    }

    #[test]
    fn retention_includes_exact_seven_day_boundary() {
        let now = JOURNAL_RETENTION_MS + 1000;
        let mut previous = record(999, "expired");
        previous.extend(record(1000, "boundary"));
        previous.extend(record(1001, "recent"));
        let output = rotate_journal(&previous, &record(now, "new"), now).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("expired"));
        assert!(output.contains("boundary"));
        assert!(output.contains("recent"));
        assert!(output.contains("new"));
    }

    #[test]
    fn size_rotation_keeps_whole_lines_and_exact_ten_mib() {
        let newest = record(20, "new");
        let exact_old = sized_record(20, MAX_JOURNAL_BYTES - newest.len());
        let exact = rotate_journal(&exact_old, &newest, 20).unwrap();
        assert_eq!(exact.len(), MAX_JOURNAL_BYTES);
        assert_eq!(journal_lines(&exact).unwrap().len(), 2);
        let over_old = sized_record(20, MAX_JOURNAL_BYTES - newest.len() + 1);
        let rotated = rotate_journal(&over_old, &newest, 20).unwrap();
        assert_eq!(rotated, newest);
        let exact_record = sized_record(20, MAX_JOURNAL_BYTES);
        assert_eq!(
            rotate_journal(&[], &exact_record, 20).unwrap(),
            exact_record
        );
        assert!(rotate_journal(&[], &sized_record(20, MAX_JOURNAL_BYTES + 1), 20).is_err());
    }

    #[test]
    fn utf8_record_byte_limit_and_oversized_file_leave_original() {
        let root = TestRoot::new();
        let store = root.store();
        store
            .append_record("preview", serde_json::json!({ "count": 0 }))
            .unwrap();
        let old = fs::read(store.root().join(JOURNAL)).unwrap();
        // UTF-8 bytes, not the smaller character count, determine the bound.
        let padding = "é".repeat(MAX_JOURNAL_BYTES / 2);
        assert!(
            store
                .append_record("preview", serde_json::json!({ "padding": padding }))
                .is_err()
        );
        assert_eq!(fs::read(store.root().join(JOURNAL)).unwrap(), old);
        private_write(&store.root().join(STATE), &vec![b' '; MAX_STATE_BYTES + 1]);
        assert!(store.load().is_err());
        assert_eq!(
            fs::metadata(store.root().join(STATE)).unwrap().len(),
            (MAX_STATE_BYTES + 1) as u64
        );
    }

    #[test]
    fn unsafe_permissions_links_and_nonregular_files_are_rejected() {
        let root = TestRoot::new();
        let store = root.store();
        store.required_root().unwrap();
        private_write(&root.0.join("outside"), b"original");
        symlink(root.0.join("outside"), store.root().join(STATE)).unwrap();
        assert!(store.load().is_err());
        assert!(store.change(add(1)).is_err());
        assert_eq!(fs::read(root.0.join("outside")).unwrap(), b"original");
        fs::remove_file(store.root().join(STATE)).unwrap();
        private_write(&store.root().join(STATE), b"{}");
        fs::set_permissions(store.root().join(STATE), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load().unwrap_err().contains("permissions"));
        fs::remove_file(store.root().join(STATE)).unwrap();
        fs::hard_link(root.0.join("outside"), store.root().join(STATE)).unwrap();
        assert!(store.load().unwrap_err().contains("hard link"));
        fs::remove_file(store.root().join(STATE)).unwrap();
        fs::create_dir(store.root().join(STATE)).unwrap();
        assert!(store.load().is_err());
        fs::remove_dir(store.root().join(STATE)).unwrap();
        fs::set_permissions(store.root(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.load().unwrap_err().contains("permissions"));
        fs::set_permissions(store.root(), fs::Permissions::from_mode(0o700)).unwrap();
        let linked = Store::at(root.0.join("linked"));
        symlink(store.root(), linked.root()).unwrap();
        assert!(linked.load().is_err());
        assert!(linked.execution_lock().is_err());
    }

    #[test]
    fn lock_and_journal_symlinks_are_never_followed() {
        for name in ["state.lock", "journal.lock", "execution.lock", JOURNAL] {
            let root = TestRoot::new();
            let store = root.store();
            store.required_root().unwrap();
            private_write(&root.0.join("outside"), b"untouched");
            symlink(root.0.join("outside"), store.root().join(name)).unwrap();
            let failed = match name {
                "state.lock" => store.change(add(1)).is_err(),
                "execution.lock" => store.execution_lock().is_err(),
                _ => store
                    .append_record("preview", serde_json::json!({}))
                    .is_err(),
            };
            assert!(failed);
            assert_eq!(fs::read(root.0.join("outside")).unwrap(), b"untouched");
        }
    }

    #[test]
    fn unwritable_directory_preserves_old_and_leaves_no_temp() {
        // Root intentionally bypasses permission checks; this failure requires an
        // ordinary user, which is the supported CLI execution model.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = TestRoot::new();
        let store = root.store();
        store.change(add(1)).unwrap();
        let old = fs::read(store.root().join(STATE)).unwrap();
        fs::set_permissions(store.root(), fs::Permissions::from_mode(0o500)).unwrap();
        assert!(store.change(add(2)).is_err());
        fs::set_permissions(store.root(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fs::read(store.root().join(STATE)).unwrap(), old);
        assert!(!fs::read_dir(store.root()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    fn child(test: &str, root: &Path, mode: &str) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--ignored", "--test-threads=1"])
            .env("BREE_STORAGE_TEST_ROOT", root)
            .env("BREE_STORAGE_TEST_MODE", mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap()
    }

    fn wait_child(child: &mut Child) -> Result<(), String> {
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let mut output = String::new();
                child
                    .stdout
                    .take()
                    .unwrap()
                    .read_to_string(&mut output)
                    .unwrap();
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("child status {status}: {output}"))
                };
            }
            if start.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("bounded storage fixture timed out".to_string());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_held(holder: &mut Child, root: &Path) {
        let start = Instant::now();
        while !root.join("held").exists() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(10));
        }
        if !root.join("held").exists() {
            let _ = holder.kill();
            let _ = holder.wait();
            panic!("lock fixture did not become ready");
        }
    }

    fn assert_bounded_busy(result: Result<(), String>, elapsed: Duration, name: &str) {
        let error = result.unwrap_err();
        assert!(error.contains(name) && error.contains("250 ms"), "{error}");
        assert!(elapsed >= Duration::from_millis(LOCK_WAIT_MS));
        assert!(
            elapsed < Duration::from_millis(500),
            "{name} took {elapsed:?}"
        );
        eprintln!(
            "{name} busy returned in {:.3} ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }

    #[test]
    fn state_lock_wait_is_bounded_preserves_files_then_recovers() {
        let root = TestRoot::new();
        let store = root.store();
        store.change(add(1)).unwrap();
        let old_state = fs::read(store.root().join(STATE)).unwrap();
        let old_journal = fs::read(store.root().join(JOURNAL)).unwrap();
        let mut holder = child("storage::tests::subprocess_fixture", &root.0, "hold_state");
        wait_held(&mut holder, &root.0);
        let start = Instant::now();
        let outcome = store.change(add(2)).map(|_| ());
        let elapsed = start.elapsed();
        let unchanged = fs::read(store.root().join(STATE)).unwrap() == old_state
            && fs::read(store.root().join(JOURNAL)).unwrap() == old_journal;
        private_write(&root.0.join("release"), b"release");
        wait_child(&mut holder).unwrap();
        assert_bounded_busy(outcome, elapsed, "state.lock");
        assert!(unchanged);
        assert_eq!(store.change(add(2)).unwrap().revision, 2);
    }

    #[test]
    fn with_state_lock_contention_is_bounded_and_never_calls_operation() {
        let root = TestRoot::new();
        let store = root.store();
        store.change(add(1)).unwrap();
        let old_state = fs::read(store.root().join(STATE)).unwrap();
        let old_journal = fs::read(store.root().join(JOURNAL)).unwrap();
        let mut holder = child("storage::tests::subprocess_fixture", &root.0, "hold_state");
        wait_held(&mut holder, &root.0);
        let mut called = false;
        let start = Instant::now();
        let outcome = store.with_state_lock(|_| {
            called = true;
            Ok(())
        });
        let elapsed = start.elapsed();
        private_write(&root.0.join("release"), b"release");
        wait_child(&mut holder).unwrap();
        assert_bounded_busy(outcome, elapsed, "state.lock");
        assert!(!called);
        assert_eq!(fs::read(store.root().join(STATE)).unwrap(), old_state);
        assert_eq!(fs::read(store.root().join(JOURNAL)).unwrap(), old_journal);
    }

    #[test]
    fn with_state_lock_holds_latest_policy_through_operation_then_releases() {
        let root = TestRoot::new();
        let store = root.store();
        store.change(add(1)).unwrap();
        let old_state = fs::read(store.root().join(STATE)).unwrap();
        let old_journal = fs::read(store.root().join(JOURNAL)).unwrap();
        let result = store
            .with_state_lock(|policy| {
                assert_eq!(policy.revision, 1);
                assert_eq!(policy.rules.len(), 1);
                let start = Instant::now();
                let nested_change = store.change(add(2)).map(|_| ());
                assert_bounded_busy(nested_change, start.elapsed(), "state.lock");
                assert_eq!(fs::read(store.root().join(STATE)).unwrap(), old_state);
                assert_eq!(fs::read(store.root().join(JOURNAL)).unwrap(), old_journal);
                Ok(policy.revision)
            })
            .unwrap();
        assert_eq!(result, 1);
        assert_eq!(store.change(add(2)).unwrap().revision, 2);
    }

    #[test]
    fn with_state_lock_operation_error_releases_without_implicit_write() {
        let root = TestRoot::new();
        let store = root.store();
        let error = store
            .with_state_lock::<()>(|policy| {
                assert_eq!(*policy, PolicyState::default());
                Err("fixture operation cancelled".to_string())
            })
            .unwrap_err();
        assert_eq!(error, "fixture operation cancelled");
        assert!(!store.root().join(STATE).exists());
        assert!(!store.root().join(JOURNAL).exists());
        assert_eq!(store.change(add(1)).unwrap().revision, 1);
    }

    #[test]
    fn journal_lock_wait_is_bounded_preserves_files_then_recovers() {
        let root = TestRoot::new();
        let store = root.store();
        store.change(add(1)).unwrap();
        let old_state = fs::read(store.root().join(STATE)).unwrap();
        let old_journal = fs::read(store.root().join(JOURNAL)).unwrap();
        let mut holder = child(
            "storage::tests::subprocess_fixture",
            &root.0,
            "hold_journal",
        );
        wait_held(&mut holder, &root.0);
        let start = Instant::now();
        let append_outcome = store.append_record("preview", serde_json::json!({ "count": 0 }));
        let append_elapsed = start.elapsed();
        let start = Instant::now();
        let change_outcome = store.change(add(2)).map(|_| ());
        let change_elapsed = start.elapsed();
        let unchanged = fs::read(store.root().join(STATE)).unwrap() == old_state
            && fs::read(store.root().join(JOURNAL)).unwrap() == old_journal;
        private_write(&root.0.join("release"), b"release");
        wait_child(&mut holder).unwrap();
        assert_bounded_busy(append_outcome, append_elapsed, "journal.lock");
        assert_bounded_busy(change_outcome, change_elapsed, "journal.lock");
        assert!(unchanged);
        store
            .append_record("preview", serde_json::json!({ "count": 0 }))
            .unwrap();
        assert_eq!(store.change(add(2)).unwrap().revision, 2);
    }

    #[test]
    fn two_process_execution_lock_is_exclusive_and_released() {
        let root = TestRoot::new();
        let store = root.store();
        let mut holder = child("storage::tests::subprocess_fixture", &root.0, "hold");
        wait_held(&mut holder, &root.0);
        assert!(store.execution_lock().unwrap_err().contains("Another Bree"));
        private_write(&root.0.join("release"), b"release");
        wait_child(&mut holder).unwrap();
        let guard = store.execution_lock().unwrap();
        let inode = fs::metadata(store.root().join("execution.lock"))
            .unwrap()
            .ino();
        drop(guard);
        let _again = store.execution_lock().unwrap();
        assert_eq!(
            fs::metadata(store.root().join("execution.lock"))
                .unwrap()
                .ino(),
            inode
        );
    }

    #[test]
    fn concurrent_process_updates_do_not_lose_rules() {
        let root = TestRoot::new();
        let mut a = child("storage::tests::subprocess_fixture", &root.0, "update_a");
        let mut b = child("storage::tests::subprocess_fixture", &root.0, "update_b");
        let store = root.store();
        let mut reader_errors = Vec::new();
        let start = Instant::now();
        while (a.try_wait().unwrap().is_none() || b.try_wait().unwrap().is_none())
            && start.elapsed() < Duration::from_secs(5)
        {
            if let Err(error) = store.load() {
                reader_errors.push(error);
            }
            if let Err(error) = store.records(20) {
                reader_errors.push(error);
            }
            thread::sleep(Duration::from_millis(1));
        }
        let result_a = wait_child(&mut a);
        let result_b = wait_child(&mut b);
        assert!(
            result_a.is_ok() && result_b.is_ok(),
            "child a: {result_a:?}; child b: {result_b:?}"
        );
        assert!(
            reader_errors.is_empty(),
            "atomic readers: {reader_errors:?}"
        );
        let state = root.store().load().unwrap();
        assert_eq!(state.revision, 12);
        assert_eq!(state.rules.len(), 12);
        let actual: std::collections::HashSet<_> = state
            .rules
            .iter()
            .map(|rule| rule.scope.bundle_id.clone())
            .collect();
        let expected: std::collections::HashSet<_> = (0..6)
            .chain(100..106)
            .map(|index| format!("dev.bree.fixture{index}"))
            .collect();
        assert_eq!(actual, expected);
        let log = fs::read(root.store().root().join(JOURNAL)).unwrap();
        assert_eq!(journal_lines(&log).unwrap().len(), 12);
        assert_eq!(store.records(20).unwrap().len(), 12);
    }

    #[test]
    #[ignore = "only called as a bounded subprocess by parent tests"]
    fn subprocess_fixture() {
        let root = PathBuf::from(std::env::var_os("BREE_STORAGE_TEST_ROOT").expect("fixture root"));
        let store = Store::at(root.join("data"));
        match std::env::var("BREE_STORAGE_TEST_MODE").unwrap().as_str() {
            mode @ ("hold" | "hold_state" | "hold_journal") => {
                let dir = store.required_root().unwrap();
                let name = match mode {
                    "hold_state" => "state.lock",
                    "hold_journal" => "journal.lock",
                    _ => "execution.lock",
                };
                let _guard = FileLock::acquire(&dir, name, mode == "hold").unwrap();
                private_write(&root.join("held"), b"held");
                let start = Instant::now();
                while !root.join("release").exists() {
                    assert!(
                        start.elapsed() < Duration::from_secs(8),
                        "release not received"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
            }
            mode @ ("update_a" | "update_b") => {
                let offset = if mode == "update_a" { 0 } else { 100 };
                let deadline = Instant::now() + Duration::from_secs(5);
                for index in 0..6 {
                    loop {
                        assert!(
                            Instant::now() < deadline,
                            "fixture update deadline exceeded"
                        );
                        match store.change(add(offset + index)) {
                            Ok(_) => break,
                            Err(error)
                                if ["state.lock", "journal.lock"].iter().any(|name| {
                                    error == format!("{name} is busy; stopped after waiting {LOCK_WAIT_MS} ms. Try again later")
                                }) =>
                            {
                                // This fixture is the caller. A precise lock-busy error
                                // makes no mutation, so it may explicitly issue a new
                                // bounded request. The product never replays changes.
                                let remaining = deadline.saturating_duration_since(Instant::now());
                                assert!(!remaining.is_zero(), "fixture update deadline exceeded: {error}");
                                thread::sleep(remaining.min(Duration::from_millis(10)));
                            }
                            Err(error) => panic!("unexpected fixture update error: {error}"),
                        }
                    }
                }
            }
            mode => panic!("unexpected fixture mode {mode}"),
        }
    }
}
