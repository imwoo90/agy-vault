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
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Instant, SystemTime};

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
    let trimmed = query
        .trim()
        .trim_end_matches(".db")
        .trim_end_matches(".pbtxt");
    if trimmed.is_empty() {
        return Err("UUID query cannot be empty".to_string());
    }

    let mut matches = BTreeSet::new();
    if config.vault_conversations_dir().is_dir() {
        for entry in fs::read_dir(config.vault_conversations_dir()).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("db")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && stem.starts_with(trimmed)
            {
                matches.insert(stem.to_string());
            }
        }
    }
    if config.vault_brain_dir().is_dir() {
        for entry in fs::read_dir(config.vault_brain_dir()).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Some(name) = entry.file_name().to_str()
                && name.starts_with(trimmed)
            {
                matches.insert(name.to_string());
            }
        }
    }

    match matches.len() {
        0 => Err(format!("session '{}' not found in vault", trimmed)),
        1 => matches
            .into_iter()
            .next()
            .ok_or_else(|| "unexpected empty match set".to_string()),
        n => Err(format!(
            "ambiguous UUID prefix '{}' matched {} sessions in vault",
            trimmed, n
        )),
    }
}

/// Restores SQLite `.db`, `.db-wal`, and `.db-shm` files for `uuid` from vault to active.
fn restore_sqlite_files(config: &VaultConfig, uuid: &str) -> Result<(bool, usize), String> {
    let mut db_restored = false;
    let vault_db = config.vault_conversations_dir().join(format!("{uuid}.db"));
    let active_db = config.active_conversations_dir().join(format!("{uuid}.db"));
    if vault_db.is_file() {
        ensure_hardlink(&vault_db, &active_db)?;
        if let Ok(file) = File::options().write(true).open(&active_db) {
            let _ = file.set_modified(SystemTime::now());
        }
        db_restored = true;
    }

    let mut wal_files_restored = 0usize;
    for suffix in ["db-wal", "db-shm"] {
        let vault_comp = config
            .vault_conversations_dir()
            .join(format!("{uuid}.{suffix}"));
        let active_comp = config
            .active_conversations_dir()
            .join(format!("{uuid}.{suffix}"));
        if vault_comp.is_file() {
            ensure_hardlink(&vault_comp, &active_comp)?;
            wal_files_restored += 1;
        }
    }
    Ok((db_restored, wal_files_restored))
}

/// Restores a pruned conversation `<uuid>` (SQLite `.db*`, `brain/<uuid>/**`, and `.pbtxt`)
/// from `<vault_dir>` back into `<agy_dir>` via `ext4` hardlinks in `< 1ms`.
pub fn restore_session(config: &VaultConfig, uuid_query: &str) -> Result<RestoreReport, String> {
    let start = Instant::now();
    let uuid = resolve_vault_uuid(config, uuid_query)?;

    fs::create_dir_all(config.active_conversations_dir()).map_err(|e| e.to_string())?;
    fs::create_dir_all(config.active_brain_dir()).map_err(|e| e.to_string())?;
    fs::create_dir_all(config.active_annotations_dir()).map_err(|e| e.to_string())?;

    let (db_restored, wal_files_restored) = restore_sqlite_files(config, &uuid)?;

    let vault_brain = config.vault_brain_dir().join(&uuid);
    let active_brain = config.active_brain_dir().join(&uuid);
    let mut dummy_report = SyncReport::default();
    let brain_files_restored = if vault_brain.is_dir() {
        sync_directory_recursive(&vault_brain, &active_brain, &mut dummy_report)?
    } else {
        0
    };

    let vault_ann = config.vault_annotations_dir().join(format!("{uuid}.pbtxt"));
    let active_ann = config
        .active_annotations_dir()
        .join(format!("{uuid}.pbtxt"));
    let mut annotation_restored = false;
    if vault_ann.is_file() && !active_ann.is_file() {
        ensure_hardlink(&vault_ann, &active_ann)?;
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
fn count_conversation_dbs(config: &VaultConfig, status: &mut VaultStatus) -> Result<(), String> {
    if config.active_conversations_dir().is_dir() {
        for entry in fs::read_dir(config.active_conversations_dir()).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.path().extension().and_then(|s| s.to_str()) == Some("db") {
                status.active_conversations += 1;
            }
        }
    }

    if config.vault_conversations_dir().is_dir() {
        for entry in fs::read_dir(config.vault_conversations_dir()).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let vault_path = entry.path();
            if vault_path.extension().and_then(|s| s.to_str()) == Some("db") {
                status.vault_conversations += 1;
                if let Some(name) = vault_path.file_name() {
                    let active_path = config.active_conversations_dir().join(name);
                    if let (Ok(v_meta), Ok(a_meta)) =
                        (fs::metadata(&vault_path), fs::metadata(&active_path))
                    {
                        if v_meta.dev() == a_meta.dev() && v_meta.ino() == a_meta.ino() {
                            status.shared_inode_conversations += 1;
                        }
                    } else {
                        status.vault_only_conversations += 1;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Computes comprehensive vault status, inode sharing counts, and storage overhead metrics.
pub fn compute_status(config: &VaultConfig) -> Result<VaultStatus, String> {
    let mut status = VaultStatus::default();
    count_conversation_dbs(config, &mut status)?;

    if config.active_brain_dir().is_dir() {
        for entry in fs::read_dir(config.active_brain_dir()).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Some(name) = entry.file_name().to_str()
                && is_uuid_like(name)
            {
                status.active_brain_dirs += 1;
            }
        }
    }

    if config.vault_brain_dir().is_dir() {
        for entry in fs::read_dir(config.vault_brain_dir()).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Some(name) = entry.file_name().to_str()
                && is_uuid_like(name)
            {
                status.vault_brain_dirs += 1;
                if !config.active_brain_dir().join(name).is_dir() {
                    status.vault_only_brain_dirs += 1;
                }
            }
        }
    }

    if config.vault_annotations_dir().is_dir() {
        for entry in fs::read_dir(config.vault_annotations_dir()).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.path().extension().and_then(|s| s.to_str()) == Some("pbtxt") {
                status.vault_annotations += 1;
            }
        }
    }

    accumulate_vault_file_metrics(&config.vault_dir, &mut status)?;
    Ok(status)
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
