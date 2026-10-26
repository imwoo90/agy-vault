//! # agy-vault: Zero-Knowledge Antigravity Session Preservation Library
//!
//! ## Overview
//! Provides deterministic `ext4` inode hardlink mirroring (`link(2)`), SQLite WAL/SHM consistency
//! management, real-time Linux `inotify` directory monitoring, and `< 1ms` reverse session
//! restoration to defeat Google Antigravity's hardcoded 500-conversation garbage collection limit
//! (`MaxConversations = 500`) with zero disk overhead for active sessions.
//!
//! ## Submodules
//! - [`vault_engine`] (`vault_engine.rs`): Core `ext4` hardlink synchronization and SQLite WAL/SHM consistency engine.
//! - [`restore`] (`restore.rs`): Reverse session restoration (`< 1ms`), UUID prefix resolution, and inode sharing telemetry.
//! - [`daemon`] (`daemon.rs`): Real-time Linux `inotify` watcher daemon for instant forward-sync and auto-restore.
//! - [`shim`] (`shim.rs`): Transparent `agy` CLI shim executor and command-line argument parser.
//!
//! ## Search Tags
//! #antigravity, #vault, #hardlink, #inotify, #sqlite-wal, #session-restore, #agent-native

pub mod daemon;
pub mod restore;
pub mod shim;
pub mod vault_engine;

pub use daemon::{DaemonConfig, DaemonStats, run_daemon};
pub use restore::{
    RestoreReport, VaultStatus, compute_status, resolve_vault_uuid, restore_session,
};
pub use shim::{
    CliOptions, PATCHED_MAX_CONVS_SIG, PatchOutcome, UNPATCHED_MAX_CONVS_SIG, build_vault_config,
    ensure_agy_binary_unbounded, parse_cli_args, restore_all_from_vault,
};
pub use vault_engine::{
    LinkAction, SyncReport, VaultConfig, ensure_hardlink, is_uuid_like, sync_all, sync_session,
};
