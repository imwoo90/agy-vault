//! # Reverse Session Restoration & Inode Telemetry Engine
//!
//! ## Overview
//! Resolves full or prefix session UUIDs inside `<vault_dir>`, restores garbage-collected SQLite
//! `.db*`, `brain/<uuid>/**`, and `annotations/<uuid>.pbtxt` files back into `<agy_dir>` via
//! `ext4` hardlinks in `< 1ms`, and computes zero-overhead inode sharing telemetry (`nlink >= 2`
//! vs `nlink == 1`).
//!
//! ## Search Tags
//! #restore, #telemetry, #uuid-resolution, #inode-sharing, #lru-promotion

use crate::vault_engine::{
    SyncReport, VaultConfig, ensure_hardlink, is_uuid_like, sync_directory_recursive,
};
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Instant;

const AT_FDCWD: i32 = -100;

unsafe extern "C" {
    fn utimensat(dirfd: i32, pathname: *const i8, times: *const libc_timespec, flags: i32) -> i32;
}

#[repr(C)]
struct libc_timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Promotes `path`'s modification and access timestamps (`mtime`/`atime`) to current wall-clock time
/// (`UTIME_NOW`) so a swapped-in session ranks #1 in Antigravity's 500-session LRU working set.
fn touch_file_mtime_now(path: &Path) -> Result<(), String> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("invalid NUL byte in path {}", path.display()))?;
    // SAFETY: Passing NULL `times` to `utimensat` with `AT_FDCWD` sets both atime and mtime to current time.
    let rc = unsafe { utimensat(AT_FDCWD, c_path.as_ptr(), std::ptr::null(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("utimensat failed for {}", path.display()))
    }
}

/// Quantitative report emitted by [`restore_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// Canonical session UUID restored from the vault.
    pub uuid: String,
    /// Whether `<uuid>.db` was restored into the active conversations directory.
    pub db_restored: bool,
    /// Number of companion WAL/SHM files restored (`0..=2`).
    pub wal_files_restored: usize,
    /// Number of files inside `brain/<uuid>/**` restored into the active brain directory.
    pub brain_files_restored: usize,
    /// Whether `annotations/<uuid>.pbtxt` was restored.
    pub annotation_restored: bool,
    /// Wall-clock restoration latency in microseconds.
    pub elapsed_micros: u128,
}

/// Snapshot of vault coverage, inode sharing, and storage overhead emitted by [`compute_status`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultStatus {
    /// Count of `.db` files currently present in `<agy_dir>/conversations`.
    pub active_conversations: usize,
    /// Count of `.db` files preserved in `<vault_dir>/conversations`.
    pub vault_conversations: usize,
    /// Count of `.db` files preserved exclusively in `<vault_dir>` after `agy` GC eviction.
    pub vault_only_conversations: usize,
    /// Count of active `.db` files sharing the exact same `ext4` inode with `<vault_dir>`.
    pub shared_inode_conversations: usize,
    /// Count of session directories in `<agy_dir>/brain`.
    pub active_brain_dirs: usize,
    /// Count of session directories in `<vault_dir>/brain`.
    pub vault_brain_dirs: usize,
    /// Count of session directories preserved exclusively in `<vault_dir>/brain`.
    pub vault_only_brain_dirs: usize,
    /// Count of `.pbtxt` files preserved in `<vault_dir>/annotations`.
    pub vault_annotations: usize,
    /// Total regular files stored across `<vault_dir>`.
    pub total_vault_files: usize,
    /// Files in `<vault_dir>` with `nlink >= 2` (zero additional disk space).
    pub shared_inode_files: usize,
    /// Files in `<vault_dir>` with `nlink == 1` (retained after active copy was unlinked).
    pub vault_only_files: usize,
    /// Total logical bytes across all files in `<vault_dir>`.
    pub logical_vault_bytes: u64,
    /// Logical bytes consumed by vault-only files (`nlink == 1`).
    pub vault_only_bytes: u64,
}

/// Resolves an exact UUID or unambiguous UUID prefix inside `<vault_dir>`.
pub fn resolve_vault_uuid(config: &VaultConfig, query: &str) -> Result<String, String> {
    let q = query.trim().trim_end_matches(".db").trim_end_matches(".pbtxt");
    if q.is_empty() {
        return Err("UUID query cannot be empty".to_string());
    }
    let mut matches = BTreeSet::new();
    if let Ok(rd) = fs::read_dir(config.vault_conversations_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("db")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && stem.starts_with(q)
            {
                matches.insert(stem.to_string());
            }
        }
    }
    if let Ok(rd) = fs::read_dir(config.vault_brain_dir()) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Some(name) = e.file_name().to_str()
                && name.starts_with(q)
            {
                matches.insert(name.to_string());
            }
        }
    }
    match matches.len() {
        0 => Err(format!("session '{q}' not found in vault")),
        1 => matches.into_iter().next().ok_or_else(|| "empty match".to_string()),
        n => Err(format!("ambiguous UUID prefix '{q}' matched {n} sessions")),
    }
}

/// Restores SQLite `.db` and non-empty `.db-wal` files for `uuid` from vault to active
/// and promotes the `.db` inode modification timestamp (`mtime`) to `now` for LRU retention.
/// Never restores `.db-shm` so SQLite cleanly rebuilds its `mmap` WAL-index without `SIGBUS`.
fn restore_sqlite_files(config: &VaultConfig, uuid: &str) -> Result<(bool, usize), String> {
    let mut db_restored = false;
    let v_db = config.vault_conversations_dir().join(format!("{uuid}.db"));
    let a_db = config.active_conversations_dir().join(format!("{uuid}.db"));
    if v_db.is_file() {
        ensure_hardlink(&v_db, &a_db)?;
        let _ = touch_file_mtime_now(&a_db);
        db_restored = true;
    }
    let mut wal_restored = 0usize;
    let v_wal = config.vault_conversations_dir().join(format!("{uuid}.db-wal"));
    let a_wal = config.active_conversations_dir().join(format!("{uuid}.db-wal"));
    if let Ok(meta) = fs::metadata(&v_wal)
        && meta.is_file()
        && meta.len() > 0
    {
        ensure_hardlink(&v_wal, &a_wal)?;
        let _ = touch_file_mtime_now(&a_wal);
        wal_restored += 1;
    }
    Ok((db_restored, wal_restored))
}

/// Restores a pruned conversation `<uuid>` (SQLite `.db*`, `brain/<uuid>/**`, and `.pbtxt`)
/// from `<vault_dir>` back into `<agy_dir>` via `ext4` hardlinks in `< 1ms`.
pub fn restore_session(config: &VaultConfig, uuid_query: &str) -> Result<RestoreReport, String> {
    let start = Instant::now();
    let uuid = resolve_vault_uuid(config, uuid_query)?;
    for dir in [
        config.active_conversations_dir(),
        config.active_brain_dir(),
        config.active_annotations_dir(),
    ] {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let (db_restored, wal_files_restored) = restore_sqlite_files(config, &uuid)?;
    let v_brain = config.vault_brain_dir().join(&uuid);
    let a_brain = config.active_brain_dir().join(&uuid);
    let mut dummy = SyncReport::default();
    let brain_files_restored = if v_brain.is_dir() {
        sync_directory_recursive(&v_brain, &a_brain, &mut dummy)?
    } else {
        0
    };
    let v_ann = config.vault_annotations_dir().join(format!("{uuid}.pbtxt"));
    let a_ann = config.active_annotations_dir().join(format!("{uuid}.pbtxt"));
    let mut annotation_restored = false;
    if v_ann.is_file() && !a_ann.is_file() {
        ensure_hardlink(&v_ann, &a_ann)?;
        annotation_restored = true;
    }
    Ok(RestoreReport {
        uuid,
        db_restored,
        wal_files_restored,
        brain_files_restored,
        annotation_restored,
        elapsed_micros: start.elapsed().as_micros(),
    })
}

/// Counts active and vaulted conversation `.db` files and verifies shared `ext4` inodes.
fn count_conversation_dbs(config: &VaultConfig, st: &mut VaultStatus) {
    if let Ok(rd) = fs::read_dir(config.active_conversations_dir()) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|s| s.to_str()) == Some("db") {
                st.active_conversations += 1;
            }
        }
    }
    if let Ok(rd) = fs::read_dir(config.vault_conversations_dir()) {
        for e in rd.flatten() {
            let vp = e.path();
            if vp.extension().and_then(|s| s.to_str()) == Some("db") {
                st.vault_conversations += 1;
                if let Some(name) = vp.file_name() {
                    let ap = config.active_conversations_dir().join(name);
                    if let (Ok(vm), Ok(am)) = (fs::metadata(&vp), fs::metadata(&ap)) {
                        if vm.dev() == am.dev() && vm.ino() == am.ino() {
                            st.shared_inode_conversations += 1;
                        }
                    } else {
                        st.vault_only_conversations += 1;
                    }
                }
            }
        }
    }
}

/// Computes comprehensive vault status, inode sharing counts, and storage overhead metrics.
pub fn compute_status(config: &VaultConfig) -> Result<VaultStatus, String> {
    let mut st = VaultStatus::default();
    count_conversation_dbs(config, &mut st);
    if let Ok(rd) = fs::read_dir(config.active_brain_dir()) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && e.file_name().to_str().is_some_and(is_uuid_like)
            {
                st.active_brain_dirs += 1;
            }
        }
    }
    if let Ok(rd) = fs::read_dir(config.vault_brain_dir()) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Some(name) = e.file_name().to_str()
                && is_uuid_like(name)
            {
                st.vault_brain_dirs += 1;
                if !config.active_brain_dir().join(name).is_dir() {
                    st.vault_only_brain_dirs += 1;
                }
            }
        }
    }
    if let Ok(rd) = fs::read_dir(config.vault_annotations_dir()) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|s| s.to_str()) == Some("pbtxt") {
                st.vault_annotations += 1;
            }
        }
    }
    accumulate_vault_file_metrics(&config.vault_dir, &mut st)?;
    Ok(st)
}

/// Walks `<vault_dir>` recursively to tally `nlink >= 2` (shared) vs `nlink == 1` (vault-only) files.
fn accumulate_vault_file_metrics(dir: &Path, status: &mut VaultStatus) -> Result<(), String> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some("index.json") {
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if meta.is_dir() {
            accumulate_vault_file_metrics(&path, status)?;
        } else if meta.is_file() {
            status.total_vault_files += 1;
            status.logical_vault_bytes += meta.len();
            if meta.nlink() >= 2 {
                status.shared_inode_files += 1;
            } else {
                status.vault_only_files += 1;
                status.vault_only_bytes += meta.len();
            }
        }
    }
    Ok(())
}
