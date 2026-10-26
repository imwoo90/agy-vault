//! # CLI Options & Binary Signature Patcher
//!
//! ## Overview
//! Parses `agy-vault` command-line flags, maintains vault index metadata, and implements the
//! version-independent x86_64 machine-code signature scanner (`ensure_agy_binary_unbounded`) that
//! patches Google Antigravity's `MaxConversations = 500` (`0x01f4`) constant to `0` (`unlimited`)
//! across `agy` binary updates without requiring wrapper scripts.
//!
//! ## Search Tags
//! #binary-patcher, #signature-scanner, #cli-parser, #max-conversations, #unbounded

use crate::restore::{compute_status, restore_session};
use crate::vault_engine::{SyncReport, VaultConfig, ensure_hardlink, is_uuid_like};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unpatched x86_64 instruction signature in `CreateLanguageServerAndServe`:
/// `test %dl,%dl; mov $0x1f4,%edx; mov $0x0,%r9d; cmovne %r9,%rdx` (`MaxConversations = 500`).
pub const UNPATCHED_MAX_CONVS_SIG: [u8; 17] = [
    0x84, 0xd2, 0xba, 0xf4, 0x01, 0x00, 0x00, 0x41, 0xb9, 0x00, 0x00, 0x00, 0x00, 0x49, 0x0f,
    0x45, 0xd1,
];

/// Patched x86_64 instruction signature setting `MaxConversations = 0` (unlimited, skips `PruneAll`).
pub const PATCHED_MAX_CONVS_SIG: [u8; 17] = [
    0x84, 0xd2, 0xba, 0x00, 0x00, 0x00, 0x00, 0x41, 0xb9, 0x00, 0x00, 0x00, 0x00, 0x49, 0x0f,
    0x45, 0xd1,
];

/// Outcome of scanning and patching the `agy` binary's `MaxConversations` limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOutcome {
    /// Found the unpatched `500` (`0x01f4`) signature at byte offset and atomically patched it to `0`.
    NewlyPatched(usize),
    /// Binary already contains the patched `0` (`unlimited`) signature at byte offset.
    AlreadyPatched(usize),
    /// Binary did not exist or did not contain the signature.
    SignatureNotFound,
}

/// Scans `bin_path` in 64 KB streaming chunks for a 17-byte signature without heap-allocating the file.
fn find_signature_offset(bin_path: &Path, sig: &[u8; 17]) -> Result<Option<usize>, String> {
    let mut file = fs::File::open(bin_path).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 65536];
    let mut carry = [0u8; 16];
    let mut carry_len = 0usize;
    let mut file_off = 0usize;

    loop {
        let n = file.read(&mut buf[carry_len..]).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        buf[..carry_len].copy_from_slice(&carry[..carry_len]);
        let total = carry_len + n;
        if let Some(pos) = buf[..total].windows(sig.len()).position(|w| w == sig) {
            return Ok(Some(file_off - carry_len + pos));
        }
        file_off += n;
        if total >= 16 {
            carry.copy_from_slice(&buf[total - 16..total]);
            carry_len = 16;
        } else {
            carry[..total].copy_from_slice(&buf[..total]);
            carry_len = total;
        }
    }
}

/// Scans `bin_path` for the `MaxConversations = 500` x86_64 instruction signature and atomically
/// patches it to `0` (`unlimited`) via kernel `copy` + `seek` + `rename(2)` with `< 64 KB` RAM usage.
pub fn ensure_agy_binary_unbounded(bin_path: &Path) -> Result<PatchOutcome, String> {
    if !bin_path.is_file() {
        return Ok(PatchOutcome::SignatureNotFound);
    }
    if let Some(off) = find_signature_offset(bin_path, &UNPATCHED_MAX_CONVS_SIG)? {
        let parent = bin_path.parent().ok_or("no parent dir for agy binary")?;
        let tmp = parent.join(".agy.vault_patch_tmp");
        let _ = fs::remove_file(&tmp);
        fs::copy(bin_path, &tmp).map_err(|e| e.to_string())?;
        let mut f = OpenOptions::new()
            .write(true)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        f.seek(SeekFrom::Start(off as u64))
            .map_err(|e| e.to_string())?;
        f.write_all(&PATCHED_MAX_CONVS_SIG)
            .map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        drop(f);
        let mut perms = fs::metadata(bin_path)
            .map_err(|e| e.to_string())?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp, perms).map_err(|e| e.to_string())?;
        fs::rename(&tmp, bin_path).map_err(|e| e.to_string())?;
        return Ok(PatchOutcome::NewlyPatched(off));
    }
    if let Some(off) = find_signature_offset(bin_path, &PATCHED_MAX_CONVS_SIG)? {
        return Ok(PatchOutcome::AlreadyPatched(off));
    }
    Ok(PatchOutcome::SignatureNotFound)
}

/// Restores all vaulted sessions that are currently missing from the active directory.
pub fn restore_all_from_vault(config: &VaultConfig) -> Result<usize, String> {
    let mut restored = 0usize;
    let mut vault_uuids = BTreeSet::new();
    if let Ok(rd) = fs::read_dir(config.vault_conversations_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("db")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && is_uuid_like(stem)
            {
                vault_uuids.insert(stem.to_string());
            }
        }
    }
    if let Ok(rd) = fs::read_dir(config.vault_brain_dir()) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Some(name) = e.file_name().to_str()
                && is_uuid_like(name)
            {
                vault_uuids.insert(name.to_string());
            }
        }
    }
    for uuid in vault_uuids {
        let a_db = config.active_conversations_dir().join(format!("{uuid}.db"));
        let v_db = config.vault_conversations_dir().join(format!("{uuid}.db"));
        let a_br = config.active_brain_dir().join(&uuid);
        let v_br = config.vault_brain_dir().join(&uuid);
        if ((v_db.is_file() && !a_db.is_file()) || (v_br.is_dir() && !a_br.is_dir()))
            && restore_session(config, &uuid).is_ok()
        {
            restored += 1;
        }
    }
    Ok(restored)
}

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
    /// Active subcommand (`sync`, `status`, `restore`, `daemon`, `patch-binary`, `help`).
    pub subcommand: String,
    /// Optional override for `~/.gemini/antigravity-cli`.
    pub agy_dir: Option<PathBuf>,
    /// Optional override for `~/.gemini/agy-vault`.
    pub vault_dir: Option<PathBuf>,
    /// Optional override for the real `agy` binary path.
    pub real_agy: Option<PathBuf>,
    /// Target conversation UUID or prefix for `restore`.
    pub target_uuid: Option<String>,
    /// Poll timeout in milliseconds for `daemon`.
    pub poll_ms: i32,
    /// Optional maximum poll ticks for `daemon`.
    pub max_ticks: Option<usize>,
    /// Whether to emit machine-readable JSON output.
    pub json_output: bool,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            subcommand: "status".to_string(),
            agy_dir: None,
            vault_dir: None,
            real_agy: None,
            target_uuid: None,
            poll_ms: 1000,
            max_ticks: None,
            json_output: false,
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
        if arg == "--json" {
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
