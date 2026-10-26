//! # agy-vault CLI Entrypoint
//!
//! ## Overview
//! Command-line interface for `agy-vault`, providing deterministic subcommands to synchronize
//! active Antigravity conversations into the `ext4` hardlink vault (`sync`), inspect zero-overhead
//! inode telemetry (`status`), restore garbage-collected sessions (`restore`), run the real-time
//! `inotify` watcher (`daemon`), and execute or install the transparent `agy` CLI shim
//! (`shim-exec`, `install-shim`).
//!
//! ## Search Tags
//! #cli, #entrypoint, #antigravity, #vault, #subcommands

use agy_vault::{
    CliOptions, DaemonConfig, PatchOutcome, SyncReport, VaultStatus, build_vault_config,
    compute_status, ensure_agy_binary_unbounded, parse_cli_args, restore_all_from_vault,
    restore_session, run_daemon, sync_all,
};
use std::env;
use std::process;

/// Prints CLI usage instructions.
fn print_help() {
    println!("agy-vault: Zero-Knowledge Antigravity Session Preservation & Auto-Patching Daemon");
    println!();
    println!("USAGE:");
    println!("  agy-vault <SUBCOMMAND> [OPTIONS]");
    println!();
    println!("SUBCOMMANDS:");
    println!("  sync                  Mirror active conversations, brain dirs, and annotations via ext4 hardlinks");
    println!("  status [--json]       Display active vs vaulted session counts and zero-overhead inode metrics");
    println!("  restore <uuid>        Restore a garbage-collected session from the vault in <1ms");
    println!("  daemon                Run real-time inotify forward-sync and auto-patcher watcher");
    println!("  patch-binary          Scan and patch bin/agy MaxConversations (500 -> 0) and restore all sessions");
}

/// Formats and prints the output of `agy-vault sync`.
fn render_sync_output(rep: &SyncReport, status: &VaultStatus, json_output: bool) {
    if json_output {
        println!(
            "{{\"active_db_synced\":{},\"brain_dirs_synced\":{},\"annotations_synced\":{},\"hardlinks_created\":{},\"inodes_updated\":{},\"already_linked\":{},\"stale_wal_cleaned\":{},\"vault_conversations\":{},\"vault_only_conversations\":{},\"elapsed_ms\":{}}}",
            rep.active_db_synced,
            rep.brain_dirs_synced,
            rep.annotations_synced,
            rep.hardlinks_created,
            rep.inodes_updated,
            rep.already_linked,
            rep.stale_wal_cleaned,
            status.vault_conversations,
            status.vault_only_conversations,
            rep.elapsed_ms
        );
    } else {
        println!("=== agy-vault sync complete ({} ms) ===", rep.elapsed_ms);
        println!("  Active conversations synced : {}", rep.active_db_synced);
        println!("  Active brain dirs synced    : {}", rep.brain_dirs_synced);
        println!("  Annotations synced          : {}", rep.annotations_synced);
        println!("  New hardlinks created       : {}", rep.hardlinks_created);
        println!("  Modified inodes relinked    : {}", rep.inodes_updated);
        println!("  Existing hardlinks verified : {}", rep.already_linked);
        println!("  Stale WAL/SHM cleaned       : {}", rep.stale_wal_cleaned);
        println!("  Total vaulted conversations : {}", status.vault_conversations);
        println!(
            "  GC-rescued (vault-only) DBs : {}",
            status.vault_only_conversations
        );
    }
}

/// Formats and prints the output of `agy-vault status`.
fn render_status_output(st: &VaultStatus, json_output: bool) {
    if json_output {
        println!(
            "{{\"active_conversations\":{},\"vault_conversations\":{},\"vault_only_conversations\":{},\"shared_inode_conversations\":{},\"active_brain_dirs\":{},\"vault_brain_dirs\":{},\"vault_only_brain_dirs\":{},\"vault_annotations\":{},\"total_vault_files\":{},\"shared_inode_files\":{},\"vault_only_files\":{},\"logical_vault_bytes\":{},\"vault_only_bytes\":{}}}",
            st.active_conversations,
            st.vault_conversations,
            st.vault_only_conversations,
            st.shared_inode_conversations,
            st.active_brain_dirs,
            st.vault_brain_dirs,
            st.vault_only_brain_dirs,
            st.vault_annotations,
            st.total_vault_files,
            st.shared_inode_files,
            st.vault_only_files,
            st.logical_vault_bytes,
            st.vault_only_bytes
        );
        return;
    }
    let mb = 1024.0 * 1024.0;
    println!("=== agy-vault status ===");
    println!("  Active conversations (.db)  : {}", st.active_conversations);
    println!("  Vaulted conversations (.db) : {}", st.vault_conversations);
    println!("  Shared-inode active (.db)   : {} (0-byte overhead)", st.shared_inode_conversations);
    println!("  GC-rescued vault-only (.db) : {}", st.vault_only_conversations);
    println!("  Active / Vaulted brain dirs : {} / {} (GC-rescued: {})", st.active_brain_dirs, st.vault_brain_dirs, st.vault_only_brain_dirs);
    println!("  Vaulted annotations (.pbtxt): {}", st.vault_annotations);
    println!("  Vault files (shared/total)  : {} / {}", st.shared_inode_files, st.total_vault_files);
    println!("  Logical vault size          : {:.2} MB (extra disk: {:.2} MB)", st.logical_vault_bytes as f64 / mb, st.vault_only_bytes as f64 / mb);
}

/// Executes the `patch-binary` subcommand.
fn run_patch_binary(opts: &CliOptions, cfg: &agy_vault::VaultConfig) -> Result<i32, String> {
    let real_agy = opts
        .real_agy
        .clone()
        .unwrap_or_else(|| cfg.agy_dir.join("bin").join("agy"));
    let outcome = ensure_agy_binary_unbounded(&real_agy)?;
    let restored = restore_all_from_vault(cfg)?;
    let _ = sync_all(cfg)?;
    match outcome {
        PatchOutcome::NewlyPatched(off) => println!(
            "Patched {} at offset 0x{off:x} (MaxConversations: 500 -> 0), restored {restored} sessions",
            real_agy.display()
        ),
        PatchOutcome::AlreadyPatched(off) => println!(
            "Verified {} already patched at offset 0x{off:x} (MaxConversations = 0), restored {restored} sessions",
            real_agy.display()
        ),
        PatchOutcome::SignatureNotFound => println!(
            "Signature not found in {}, restored {restored} sessions",
            real_agy.display()
        ),
    }
    Ok(0)
}

/// Executes the `daemon` subcommand.
fn run_daemon_cmd(opts: &CliOptions, cfg: agy_vault::VaultConfig) -> Result<i32, String> {
    let dcfg = DaemonConfig {
        vault: cfg,
        poll_ms: opts.poll_ms,
        max_ticks: opts.max_ticks,
    };
    let stats = run_daemon(&dcfg)?;
    println!(
        "[agy-vault] daemon exited: ticks={}, events={}, syncs={}, auto_restores={}, patches={}",
        stats.ticks_completed,
        stats.inotify_events_processed,
        stats.forward_syncs_triggered,
        stats.auto_restores_triggered,
        stats.binary_patches_applied
    );
    Ok(0)
}

/// Executes the parsed CLI subcommand and returns the process exit code.
fn execute_cli(opts: CliOptions) -> Result<i32, String> {
    if opts.subcommand == "help" {
        print_help();
        return Ok(0);
    }
    let cfg = build_vault_config(&opts)?;

    match opts.subcommand.as_str() {
        "sync" => {
            let _ = restore_all_from_vault(&cfg)?;
            let rep = sync_all(&cfg)?;
            let status = compute_status(&cfg)?;
            render_sync_output(&rep, &status, opts.json_output);
            Ok(0)
        }
        "status" => {
            let st = compute_status(&cfg)?;
            render_status_output(&st, opts.json_output);
            Ok(0)
        }
        "restore" => {
            let query = opts
                .target_uuid
                .as_deref()
                .ok_or("restore requires a <uuid-or-prefix> argument")?;
            let rep = restore_session(&cfg, query)?;
            println!(
                "=== Restored session {} in {} us (db={}, wal={}, brain={}, ann={}) ===",
                rep.uuid,
                rep.elapsed_micros,
                rep.db_restored,
                rep.wal_files_restored,
                rep.brain_files_restored,
                rep.annotation_restored
            );
            Ok(0)
        }
        "daemon" => run_daemon_cmd(&opts, cfg),
        "patch-binary" => run_patch_binary(&opts, &cfg),
        other => Err(format!("unknown subcommand '{other}' (run with --help)")),
    }
}

fn main() {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    let opts = match parse_cli_args(&raw_args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[agy-vault] error: {e}");
            process::exit(2);
        }
    };

    match execute_cli(opts) {
        Ok(code) => process::exit(code),
        Err(e) => {
            eprintln!("[agy-vault] error: {e}");
            process::exit(1);
        }
    }
}
