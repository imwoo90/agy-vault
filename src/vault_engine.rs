//! # Core Hardlink Vault Synchronization Engine
//!
//! ## Overview
//! Implements zero-copy `ext4` hardlink mirroring (`link(2)`) between Google Antigravity's active
//! state directory (`~/.gemini/antigravity-cli/`) and the persistent vault directory
//! (`~/.gemini/agy-vault/`). Preserves SQLite `.db`, `.db-wal`, and `.db-shm` files alongside
//! recursive `brain/<uuid>/**` artifacts and `annotations/<uuid>.pbtxt` metadata.
//!
//! ## Search Tags
//! #hardlink, #inode, #sqlite-wal, #sync, #vault-engine

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Configuration paths for the Antigravity active directory and persistent hardlink vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultConfig {
    /// Root path of Google Antigravity CLI state (`~/.gemini/antigravity-cli`).
    pub agy_dir: PathBuf,
    /// Root path of the persistent hardlink vault (`~/.gemini/agy-vault`).
    pub vault_dir: PathBuf,
}

impl VaultConfig {
    /// Creates a new [`VaultConfig`] from explicit active and vault directory paths.
    ///
    /// # Examples
    /// ```
    /// use std::path::PathBuf;
    /// use agy_vault::VaultConfig;
    ///
    /// let cfg = VaultConfig::new("/tmp/agy", "/tmp/vault");
    /// assert_eq!(cfg.active_conversations_dir(), PathBuf::from("/tmp/agy/conversations"));
    /// assert_eq!(cfg.vault_conversations_dir(), PathBuf::from("/tmp/vault/conversations"));
    /// ```
    pub fn new(agy_dir: impl Into<PathBuf>, vault_dir: impl Into<PathBuf>) -> Self {
        Self {
            agy_dir: agy_dir.into(),
            vault_dir: vault_dir.into(),
        }
    }

    /// Resolves default [`VaultConfig`] from environment variables (`AGY_DIR`, `AGY_VAULT_DIR`, `HOME`).
    pub fn from_env() -> Result<Self, String> {
        let home = PathBuf::from(env::var("HOME").map_err(|_| "HOME not set".to_string())?);
        let agy_dir = env::var("AGY_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".gemini/antigravity-cli"));
        let vault_dir = env::var("AGY_VAULT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".gemini/agy-vault"));
        Ok(Self { agy_dir, vault_dir })
    }

    /// Path to active SQLite conversation databases (`<agy_dir>/conversations`).
    pub fn active_conversations_dir(&self) -> PathBuf {
        self.agy_dir.join("conversations")
    }
    /// Path to active brain directory (`<agy_dir>/brain`).
    pub fn active_brain_dir(&self) -> PathBuf {
        self.agy_dir.join("brain")
    }
    /// Path to active annotations directory (`<agy_dir>/annotations`).
    pub fn active_annotations_dir(&self) -> PathBuf {
        self.agy_dir.join("annotations")
    }
    /// Path to vaulted SQLite conversation databases (`<vault_dir>/conversations`).
    pub fn vault_conversations_dir(&self) -> PathBuf {
        self.vault_dir.join("conversations")
    }
    /// Path to vaulted brain directory (`<vault_dir>/brain`).
    pub fn vault_brain_dir(&self) -> PathBuf {
        self.vault_dir.join("brain")
    }
    /// Path to vaulted annotations directory (`<vault_dir>/annotations`).
    pub fn vault_annotations_dir(&self) -> PathBuf {
        self.vault_dir.join("annotations")
    }
    /// Path to the JSON metadata index inside the vault (`<vault_dir>/index.json`).
    pub fn vault_index_path(&self) -> PathBuf {
        self.vault_dir.join("index.json")
    }

    /// Creates the vault directory hierarchy if it does not already exist.
    pub fn ensure_vault_dirs(&self) -> Result<(), String> {
        for dir in [
            self.vault_conversations_dir(),
            self.vault_brain_dir(),
            self.vault_annotations_dir(),
        ] {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Outcome of ensuring an `ext4` hardlink from `src` to `dst`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkAction {
    /// Destination did not exist and was newly hardlinked to source.
    CreatedNew,
    /// Destination existed with a different inode and was atomically relinked to source.
    UpdatedInode,
    /// Source and destination already share the exact same device and inode.
    AlreadyLinked,
}

/// Quantitative report emitted by [`sync_all`] and [`sync_session`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Number of active `.db` conversation sessions processed.
    pub active_db_synced: usize,
    /// Number of active `brain/<uuid>` directories processed.
    pub brain_dirs_synced: usize,
    /// Number of `annotations/<uuid>.pbtxt` files processed.
    pub annotations_synced: usize,
    /// Number of new hardlinks created in the vault.
    pub hardlinks_created: usize,
    /// Number of existing vault files relinked due to source inode replacement.
    pub inodes_updated: usize,
    /// Number of files skipped because source and destination already shared the same inode.
    pub already_linked: usize,
    /// Number of stale `.db-wal` / `.db-shm` files removed after clean SQLite checkpoint.
    pub stale_wal_cleaned: usize,
    /// Wall-clock duration of the sync operation in milliseconds.
    pub elapsed_ms: u128,
}

impl SyncReport {
    /// Accumulates a single [`LinkAction`] into the report counters.
    pub fn record_action(&mut self, action: LinkAction) {
        match action {
            LinkAction::CreatedNew => self.hardlinks_created += 1,
            LinkAction::UpdatedInode => self.inodes_updated += 1,
            LinkAction::AlreadyLinked => self.already_linked += 1,
        }
    }

    /// Merges another [`SyncReport`] into `self`.
    pub fn merge(&mut self, other: &SyncReport) {
        self.active_db_synced += other.active_db_synced;
        self.brain_dirs_synced += other.brain_dirs_synced;
        self.annotations_synced += other.annotations_synced;
        self.hardlinks_created += other.hardlinks_created;
        self.inodes_updated += other.inodes_updated;
        self.already_linked += other.already_linked;
        self.stale_wal_cleaned += other.stale_wal_cleaned;
    }
}

/// Checks whether a string matches the standard 36-character UUID format (`8-4-4-4-12` hex).
///
/// # Examples
/// ```
/// use agy_vault::is_uuid_like;
///
/// assert!(is_uuid_like("cc892e65-120e-48f3-b0cc-4ac54395db11"));
/// assert!(!is_uuid_like("not-a-uuid"));
/// ```
pub fn is_uuid_like(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// Ensures `dst` is an `ext4` hardlink pointing to the exact same inode as `src`.
pub fn ensure_hardlink(src: &Path, dst: &Path) -> Result<LinkAction, String> {
    let src_meta = fs::metadata(src).map_err(|e| e.to_string())?;
    if let Ok(dst_meta) = fs::symlink_metadata(dst) {
        if src_meta.dev() == dst_meta.dev() && src_meta.ino() == dst_meta.ino() {
            return Ok(LinkAction::AlreadyLinked);
        }
        let parent = dst.parent().ok_or("no parent dir")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let fname = dst.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");
        let tmp = parent.join(format!(".{fname}.agy_vault_tmp"));
        let _ = fs::remove_file(&tmp);
        if fs::hard_link(src, &tmp).is_err() {
            fs::copy(src, &tmp).map_err(|e| e.to_string())?;
        }
        fs::rename(&tmp, dst).map_err(|e| e.to_string())?;
        return Ok(LinkAction::UpdatedInode);
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst).map_err(|e| e.to_string())?;
    }
    Ok(LinkAction::CreatedNew)
}

/// Recursively mirrors all regular files under `src_dir` into `dst_dir` using [`ensure_hardlink`].
pub(crate) fn sync_directory_recursive(
    src_dir: &Path,
    dst_dir: &Path,
    report: &mut SyncReport,
) -> Result<usize, String> {
    if !src_dir.is_dir() {
        return Ok(0);
    }
    fs::create_dir_all(dst_dir).map_err(|e| e.to_string())?;
    let mut linked_files = 0usize;

    for entry in fs::read_dir(src_dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ftype = entry.file_type().map_err(|e| e.to_string())?;
        let src_path = entry.path();
        let dst_path = dst_dir.join(entry.file_name());

        if ftype.is_dir() {
            linked_files += sync_directory_recursive(&src_path, &dst_path, report)?;
        } else if ftype.is_file() {
            let action = ensure_hardlink(&src_path, &dst_path)?;
            report.record_action(action);
            linked_files += 1;
        }
    }
    Ok(linked_files)
}

/// Synchronizes a single conversation `<uuid>` (SQLite `.db*`, `brain/<uuid>/**`, and `.pbtxt`)
/// from the active Antigravity directory into the vault.
pub fn sync_session(cfg: &VaultConfig, uuid: &str) -> Result<SyncReport, String> {
    let start = Instant::now();
    cfg.ensure_vault_dirs()?;
    let mut rep = SyncReport::default();
    let (a_dir, v_dir) = (cfg.active_conversations_dir(), cfg.vault_conversations_dir());
    let (a_db, v_db) = (a_dir.join(format!("{uuid}.db")), v_dir.join(format!("{uuid}.db")));

    if a_db.is_file() {
        rep.record_action(ensure_hardlink(&a_db, &v_db)?);
        rep.active_db_synced += 1;

        let (a_wal, v_wal) = (a_dir.join(format!("{uuid}.db-wal")), v_dir.join(format!("{uuid}.db-wal")));
        if fs::metadata(&a_wal).is_ok_and(|m| m.is_file() && m.len() > 0) {
            rep.record_action(ensure_hardlink(&a_wal, &v_wal)?);
        } else if v_wal.exists() && fs::remove_file(&v_wal).is_ok() {
            rep.stale_wal_cleaned += 1;
        }
        let _ = fs::remove_file(v_dir.join(format!("{uuid}.db-shm")));
    }

    let (a_br, v_br) = (cfg.active_brain_dir().join(uuid), cfg.vault_brain_dir().join(uuid));
    if a_br.is_dir() {
        sync_directory_recursive(&a_br, &v_br, &mut rep)?;
        rep.brain_dirs_synced += 1;
    }

    let a_ann = cfg.active_annotations_dir().join(format!("{uuid}.pbtxt"));
    let v_ann = cfg.vault_annotations_dir().join(format!("{uuid}.pbtxt"));
    if a_ann.is_file() {
        rep.record_action(ensure_hardlink(&a_ann, &v_ann)?);
        rep.annotations_synced += 1;
    }

    rep.elapsed_ms = start.elapsed().as_millis();
    Ok(rep)
}

/// Synchronizes all active conversations, brain directories, and annotations into the vault.
pub fn sync_all(config: &VaultConfig) -> Result<SyncReport, String> {
    let start = Instant::now();
    config.ensure_vault_dirs()?;
    let mut total = SyncReport::default();
    let mut uuids = BTreeSet::new();

    if config.active_conversations_dir().is_dir() {
        for entry in fs::read_dir(config.active_conversations_dir()).map_err(|e| e.to_string())? {
            let p = entry.map_err(|e| e.to_string())?.path();
            if p.extension().and_then(|s| s.to_str()) == Some("db")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && is_uuid_like(stem)
            {
                uuids.insert(stem.to_string());
            }
        }
    }
    if config.active_brain_dir().is_dir() {
        for entry in fs::read_dir(config.active_brain_dir()).map_err(|e| e.to_string())? {
            let e = entry.map_err(|e| e.to_string())?;
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Some(name) = e.file_name().to_str()
                && is_uuid_like(name)
            {
                uuids.insert(name.to_string());
            }
        }
    }
    for uuid in &uuids {
        total.merge(&sync_session(config, uuid)?);
    }
    crate::shim::sync_orphan_annotations(config, &uuids, &mut total)?;
    total.elapsed_ms = start.elapsed().as_millis();
    crate::shim::write_vault_index(config, &total)?;
    Ok(total)
}
