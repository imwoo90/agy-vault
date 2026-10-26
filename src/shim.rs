//! # Transparent `agy` CLI Shim & Argument Parser
//!
//! ## Overview
//! Provides transparent command-line wrapping for the Google Antigravity CLI (`agy`). Inspects
//! user CLI arguments for conversation UUIDs (`--resume <uuid>`, `-r <uuid>`, or bare UUIDs),
//! automatically restores any garbage-collected session from `<vault_dir>` prior to invoking the
//! real `agy` binary, and parses `agy-vault` CLI options.
//!
//! ## Search Tags
//! #cli-shim, #arg-parser, #resume, #wrapper, #passthrough

use crate::restore::{compute_status, heal_evicted_sessions, resolve_vault_uuid, restore_session};
use crate::vault_engine::{SyncReport, VaultConfig, ensure_hardlink, is_uuid_like, sync_all};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Hardlinks any historical `.pbtxt` annotations whose `.db` and `brain/` were already pruned.
pub(crate) fn sync_orphan_annotations(
    config: &VaultConfig,
    seen_uuids: &BTreeSet<String>,
    total: &mut SyncReport,
) -> Result<(), String> {
    if !config.active_annotations_dir().is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(config.active_annotations_dir()).map_err(|e| e.to_string())? {
        let p = entry.map_err(|e| e.to_string())?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("pbtxt")
            && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            && !seen_uuids.contains(stem)
        {
            let dst = config.vault_annotations_dir().join(format!("{stem}.pbtxt"));
            total.record_action(ensure_hardlink(&p, &dst)?);
            total.annotations_synced += 1;
        }
    }
    Ok(())
}

/// Persists a lightweight JSON summary of the latest vault state to `<vault_dir>/index.json`.
pub(crate) fn write_vault_index(config: &VaultConfig, last: &SyncReport) -> Result<(), String> {
    let st = compute_status(config)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let json = format!(
        "{{\"updated_at_unix\":{ts},\"active_conversations\":{},\"vault_conversations\":{},\"vault_only_conversations\":{},\"shared_inode_conversations\":{},\"vault_brain_dirs\":{},\"vault_annotations\":{},\"last_sync_hardlinks_created\":{},\"last_sync_elapsed_ms\":{}}}\n",
        st.active_conversations,
        st.vault_conversations,
        st.vault_only_conversations,
        st.shared_inode_conversations,
        st.vault_brain_dirs,
        st.vault_annotations,
        last.hardlinks_created,
        last.elapsed_ms
    );
    fs::write(config.vault_index_path(), json).map_err(|e| e.to_string())
}

/// Parsed CLI options and subcommands for `agy-vault`.
#[derive(Debug, Clone)]
pub struct CliOptions {
    /// Active subcommand (`sync`, `status`, `restore`, `daemon`, `shim-exec`, `install-shim`, `help`).
    pub subcommand: String,
    /// Optional override for `~/.gemini/antigravity-cli`.
    pub agy_dir: Option<PathBuf>,
    /// Optional override for `~/.gemini/agy-vault`.
    pub vault_dir: Option<PathBuf>,
    /// Optional override for the real `agy` binary path.
    pub real_agy: Option<PathBuf>,
    /// Optional override for the installed shim script path.
    pub shim_path: Option<PathBuf>,
    /// Target conversation UUID or prefix for `restore`.
    pub target_uuid: Option<String>,
    /// Poll timeout in milliseconds for `daemon`.
    pub poll_ms: i32,
    /// Optional maximum poll ticks for `daemon`.
    pub max_ticks: Option<usize>,
    /// Whether to emit machine-readable JSON output.
    pub json_output: bool,
    /// Passthrough arguments forwarded to the real `agy` binary in `shim-exec`.
    pub passthrough_args: Vec<String>,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            subcommand: "status".to_string(),
            agy_dir: None,
            vault_dir: None,
            real_agy: None,
            shim_path: None,
            target_uuid: None,
            poll_ms: 1000,
            max_ticks: None,
            json_output: false,
            passthrough_args: Vec::new(),
        }
    }
}

/// Parses a single flag taking a value and advances `idx`.
fn parse_valued_flag(
    flag: &str,
    raw_args: &[String],
    idx: &mut usize,
    opts: &mut CliOptions,
) -> Result<bool, String> {
    match flag {
        "--agy-dir" => {
            *idx += 1;
            opts.agy_dir = Some(PathBuf::from(
                raw_args.get(*idx).ok_or("--agy-dir requires a path")?,
            ));
            *idx += 1;
            Ok(true)
        }
        "--vault-dir" => {
            *idx += 1;
            opts.vault_dir = Some(PathBuf::from(
                raw_args.get(*idx).ok_or("--vault-dir requires a path")?,
            ));
            *idx += 1;
            Ok(true)
        }
        "--real-agy" => {
            *idx += 1;
            opts.real_agy = Some(PathBuf::from(
                raw_args.get(*idx).ok_or("--real-agy requires a path")?,
            ));
            *idx += 1;
            Ok(true)
        }
        "--shim-path" => {
            *idx += 1;
            opts.shim_path = Some(PathBuf::from(
                raw_args.get(*idx).ok_or("--shim-path requires a path")?,
            ));
            *idx += 1;
            Ok(true)
        }
        "--poll-ms" => {
            *idx += 1;
            let val = raw_args.get(*idx).ok_or("--poll-ms requires an integer")?;
            opts.poll_ms = val
                .parse::<i32>()
                .map_err(|_| "invalid integer for --poll-ms")?;
            *idx += 1;
            Ok(true)
        }
        "--max-ticks" => {
            *idx += 1;
            let val = raw_args
                .get(*idx)
                .ok_or("--max-ticks requires an integer")?;
            opts.max_ticks = Some(
                val.parse::<usize>()
                    .map_err(|_| "invalid integer for --max-ticks")?,
            );
            *idx += 1;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Parses command-line arguments into [`CliOptions`].
pub fn parse_cli_args(raw_args: &[String]) -> Result<CliOptions, String> {
    let mut opts = CliOptions::default();
    if raw_args.is_empty() {
        return Ok(opts);
    }

    let mut idx = 0usize;
    if !raw_args[0].starts_with('-') {
        opts.subcommand = raw_args[0].clone();
        idx = 1;
    }

    while idx < raw_args.len() {
        let arg = &raw_args[idx];
        if arg == "--" {
            opts.passthrough_args.extend_from_slice(&raw_args[idx + 1..]);
            break;
        } else if arg == "--json" {
            opts.json_output = true;
            idx += 1;
        } else if arg == "--help" || arg == "-h" {
            opts.subcommand = "help".to_string();
            idx += 1;
        } else if parse_valued_flag(arg, raw_args, &mut idx, &mut opts)? {
            continue;
        } else if opts.target_uuid.is_none() && !arg.starts_with('-') {
            opts.target_uuid = Some(arg.clone());
            idx += 1;
        } else {
            return Err(format!("unrecognized argument: {arg}"));
        }
    }

    Ok(opts)
}

/// Resolves [`VaultConfig`] combining CLI overrides and environment defaults.
pub fn build_vault_config(opts: &CliOptions) -> Result<VaultConfig, String> {
    let mut cfg = VaultConfig::from_env()?;
    if let Some(ref agy) = opts.agy_dir {
        cfg.agy_dir = agy.clone();
    }
    if let Some(ref vault) = opts.vault_dir {
        cfg.vault_dir = vault.clone();
    }
    Ok(cfg)
}

/// Extracts candidate conversation UUIDs or `--resume` / `-r` query prefixes from CLI arguments.
///
/// # Examples
/// ```
/// use agy_vault::extract_uuids_from_args;
///
/// let args = vec![
///     "--resume".to_string(),
///     "cc892e65-120e-48f3-b0cc-4ac54395db11".to_string(),
/// ];
/// let found = extract_uuids_from_args(&args);
/// assert_eq!(found, vec!["cc892e65-120e-48f3-b0cc-4ac54395db11".to_string()]);
/// ```
pub fn extract_uuids_from_args(args: &[String]) -> Vec<String> {
    let mut set = BTreeSet::new();
    let mut iter = args.iter().peekable();

    while let Some(arg) = iter.next() {
        if arg == "--resume" || arg == "-r" || arg == "--conversation-id" {
            if let Some(next_val) = iter.peek()
                && !next_val.starts_with('-')
            {
                set.insert((*next_val).clone());
            }
        } else if let Some(val) = arg
            .strip_prefix("--resume=")
            .or_else(|| arg.strip_prefix("--conversation-id="))
        {
            if !val.is_empty() {
                set.insert(val.to_string());
            }
        } else if is_uuid_like(arg) {
            set.insert(arg.clone());
        }
    }

    set.into_iter().collect()
}

/// Executes the transparent CLI shim: pre-restores any referenced pruned session from the vault,
/// and then executes the real `agy` binary with all user arguments.
pub fn run_shim_exec(
    config: &VaultConfig,
    real_agy_bin: &Path,
    passthrough_args: &[String],
) -> Result<i32, String> {
    let _ = heal_evicted_sessions(config);
    let candidates = extract_uuids_from_args(passthrough_args);
    for candidate in &candidates {
        if let Ok(uuid) = resolve_vault_uuid(config, candidate) {
            let active_db = config.active_conversations_dir().join(format!("{uuid}.db"));
            if !active_db.is_file() {
                let _ = restore_session(config, &uuid);
            }
        }
    }

    if std::env::var("AGY_VAULT_SHIM_EXECVE").as_deref() == Ok("1") {
        use std::os::unix::process::CommandExt;
        let err = Command::new(real_agy_bin).args(passthrough_args).exec();
        return Err(format!("failed to exec {}: {}", real_agy_bin.display(), err));
    }

    let status = Command::new(real_agy_bin)
        .args(passthrough_args)
        .status()
        .map_err(|e| format!("failed to execute {}: {}", real_agy_bin.display(), e))?;

    let _ = sync_all(config);
    let _ = heal_evicted_sessions(config);
    Ok(status.code().unwrap_or(1))
}

/// Installs an executable POSIX shell shim script at `shim_path` (e.g. `~/.local/bin/agy`)
/// that wraps `real_agy_bin` through `agy-vault shim-exec`.
pub fn install_shim_script(
    shim_path: &Path,
    vault_bin: &Path,
    real_agy_bin: &Path,
) -> Result<PathBuf, String> {
    if shim_path == real_agy_bin {
        return Err("shim_path and real_agy_bin cannot be the same path".to_string());
    }
    if let Some(parent) = shim_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Always unlink existing file or symlink first so we never follow a symlink to real_agy_bin!
    if fs::symlink_metadata(shim_path).is_ok() {
        fs::remove_file(shim_path).map_err(|e| e.to_string())?;
    }
    let script = format!(
        "#!/bin/sh\n# Auto-generated by agy-vault: Transparent Zero-Knowledge Antigravity Session Shim\nAGY_VAULT_SHIM_EXECVE=1 exec \"{}\" shim-exec --real-agy \"{}\" -- \"$@\"\n",
        vault_bin.display(),
        real_agy_bin.display()
    );
    fs::write(shim_path, script).map_err(|e| e.to_string())?;
    let mut perms = fs::metadata(shim_path)
        .map_err(|e| e.to_string())?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(shim_path, perms).map_err(|e| e.to_string())?;
    Ok(shim_path.to_path_buf())
}
