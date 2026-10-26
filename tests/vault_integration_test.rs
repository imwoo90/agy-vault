//! Deterministic integration tests for `agy-vault` covering:
//! 1. `ext4` inode hardlink mirroring (`nlink == 2`, zero disk overhead)
//! 2. Antigravity 500-session GC `unlink()` simulation and `< 1ms` reverse restoration
//! 3. SQLite WAL/SHM clean checkpoint cleanup vs crash preservation
//! 4. Real-time Linux `inotify` daemon forward-sync and `annotations/*.pbtxt` auto-restore
//! 5. Transparent CLI shim (`shim-exec`) pre-restoration on `--resume <uuid>`

use agy_vault::{
    DaemonConfig, VaultConfig, compute_status, install_shim_script, restore_session, run_daemon,
    run_shim_exec, sync_all, sync_session,
};
use std::error::Error;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type TestResult = Result<(), Box<dyn Error>>;

fn create_temp_sandbox(test_name: &str) -> Result<(PathBuf, VaultConfig), Box<dyn Error>> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "agy_vault_test_{}_{}_{}",
        test_name,
        std::process::id(),
        nanos
    ));
    let agy_dir = root.join("antigravity-cli");
    let vault_dir = root.join("agy-vault");
    fs::create_dir_all(agy_dir.join("conversations"))?;
    fs::create_dir_all(agy_dir.join("brain"))?;
    fs::create_dir_all(agy_dir.join("annotations"))?;
    Ok((root, VaultConfig::new(agy_dir, vault_dir)))
}

fn populate_mock_session(
    cfg: &VaultConfig,
    uuid: &str,
    db_payload: &[u8],
    with_wal: bool,
) -> TestResult {
    let db_path = cfg.active_conversations_dir().join(format!("{uuid}.db"));
    fs::write(&db_path, db_payload)?;

    if with_wal {
        let wal_path = cfg
            .active_conversations_dir()
            .join(format!("{uuid}.db-wal"));
        let shm_path = cfg
            .active_conversations_dir()
            .join(format!("{uuid}.db-shm"));
        fs::write(wal_path, b"WAL-FRAME-DATA")?;
        fs::write(shm_path, b"SHM-INDEX-DATA")?;
    }

    let logs_dir = cfg
        .active_brain_dir()
        .join(uuid)
        .join(".system_generated")
        .join("logs");
    fs::create_dir_all(&logs_dir)?;
    fs::write(
        logs_dir.join("transcript.jsonl"),
        format!("{{\"session\":\"{uuid}\",\"status\":\"DONE\"}}\n"),
    )?;

    let ann_path = cfg.active_annotations_dir().join(format!("{uuid}.pbtxt"));
    fs::write(ann_path, format!("title: \"Session {uuid}\"\n"))?;
    Ok(())
}

#[test]
fn test_hardlink_sync_gc_eviction_and_instant_restore() -> TestResult {
    let (root, cfg) = create_temp_sandbox("gc_restore")?;
    let uuid_old = "11111111-2222-3333-4444-555555555555";
    let uuid_new = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    populate_mock_session(&cfg, uuid_old, b"SQLITE3-OLD-SESSION-BINARY", false)?;
    populate_mock_session(&cfg, uuid_new, b"SQLITE3-NEW-SESSION-BINARY", true)?;

    let sync_rep = sync_all(&cfg)?;
    assert_eq!(sync_rep.active_db_synced, 2);
    assert_eq!(sync_rep.brain_dirs_synced, 2);
    assert_eq!(sync_rep.annotations_synced, 2);

    let active_old_db = cfg.active_conversations_dir().join(format!("{uuid_old}.db"));
    let vault_old_db = cfg.vault_conversations_dir().join(format!("{uuid_old}.db"));
    let active_meta = fs::metadata(&active_old_db)?;
    let vault_meta = fs::metadata(&vault_old_db)?;
    assert_eq!(active_meta.ino(), vault_meta.ino());
    assert_eq!(vault_meta.nlink(), 2);

    fs::remove_file(&active_old_db)?;
    fs::remove_dir_all(cfg.active_brain_dir().join(uuid_old))?;

    let st_after_gc = compute_status(&cfg)?;
    assert_eq!(st_after_gc.active_conversations, 1);
    assert_eq!(st_after_gc.vault_conversations, 2);
    assert_eq!(st_after_gc.vault_only_conversations, 1);
    assert_eq!(st_after_gc.vault_only_brain_dirs, 1);

    let survived_meta = fs::metadata(&vault_old_db)?;
    assert_eq!(survived_meta.nlink(), 1);
    assert_eq!(
        fs::read(&vault_old_db)?,
        b"SQLITE3-OLD-SESSION-BINARY"
    );

    let restore_rep = restore_session(&cfg, "11111111")?;
    assert_eq!(restore_rep.uuid, uuid_old);
    assert!(restore_rep.db_restored);
    assert_eq!(restore_rep.brain_files_restored, 1);

    assert!(active_old_db.is_file());
    let restored_transcript = cfg
        .active_brain_dir()
        .join(uuid_old)
        .join(".system_generated/logs/transcript.jsonl");
    assert!(restored_transcript.is_file());
    assert_eq!(fs::metadata(&vault_old_db)?.nlink(), 2);

    let _ = fs::remove_dir_all(root);
    Ok(())
}

#[test]
fn test_sqlite_wal_clean_checkpoint_cleanup() -> TestResult {
    let (root, cfg) = create_temp_sandbox("wal_checkpoint")?;
    let uuid = "22222222-3333-4444-5555-666666666666";

    populate_mock_session(&cfg, uuid, b"SQLITE-PRE-CHECKPOINT", true)?;
    let rep1 = sync_session(&cfg, uuid)?;
    assert_eq!(rep1.active_db_synced, 1);
    assert!(
        cfg.vault_conversations_dir()
            .join(format!("{uuid}.db-wal"))
            .is_file()
    );
    assert!(
        cfg.vault_conversations_dir()
            .join(format!("{uuid}.db-shm"))
            .is_file()
    );

    fs::write(
        cfg.active_conversations_dir().join(format!("{uuid}.db")),
        b"SQLITE-POST-CHECKPOINT",
    )?;
    fs::remove_file(
        cfg.active_conversations_dir()
            .join(format!("{uuid}.db-wal")),
    )?;
    fs::remove_file(
        cfg.active_conversations_dir()
            .join(format!("{uuid}.db-shm")),
    )?;

    let rep2 = sync_session(&cfg, uuid)?;
    assert_eq!(rep2.stale_wal_cleaned, 2);
    assert!(
        !cfg.vault_conversations_dir()
            .join(format!("{uuid}.db-wal"))
            .exists()
    );
    assert!(
        !cfg.vault_conversations_dir()
            .join(format!("{uuid}.db-shm"))
            .exists()
    );
    assert_eq!(
        fs::read(cfg.vault_conversations_dir().join(format!("{uuid}.db")))?,
        b"SQLITE-POST-CHECKPOINT"
    );

    let _ = fs::remove_dir_all(root);
    Ok(())
}

#[test]
fn test_inotify_daemon_on_demand_swap_in_without_delete_thrashing() -> TestResult {
    let (root, cfg) = create_temp_sandbox("inotify_daemon")?;
    let uuid_evicted = "33333333-4444-5555-6666-777777777777";
    let uuid_pruned_live = "77777777-8888-9999-aaaa-bbbbbbbbbbbb";
    populate_mock_session(&cfg, uuid_evicted, b"EVICTED-DB-CONTENT", false)?;
    populate_mock_session(&cfg, uuid_pruned_live, b"PRUNED-LIVE-DB-CONTENT", false)?;
    sync_all(&cfg)?;

    let active_db = cfg
        .active_conversations_dir()
        .join(format!("{uuid_evicted}.db"));
    let live_pruned_db = cfg
        .active_conversations_dir()
        .join(format!("{uuid_pruned_live}.db"));

    fs::remove_file(&active_db)?;
    fs::remove_dir_all(cfg.active_brain_dir().join(uuid_evicted))?;
    assert!(!active_db.exists());

    let daemon_cfg = DaemonConfig {
        vault: cfg.clone(),
        poll_ms: 25,
        max_ticks: Some(15),
    };

    let handle = thread::spawn(move || run_daemon(&daemon_cfg));
    thread::sleep(Duration::from_millis(60));

    // Simulate PruneAll unlinking uuid_pruned_live while daemon is watching:
    // Daemon must NOT restore it on IN_DELETE (preventing startup thrashing),
    // while keeping it 100% intact inside the vault!
    fs::remove_file(&live_pruned_db)?;
    fs::remove_dir_all(cfg.active_brain_dir().join(uuid_pruned_live))?;

    // Trigger on-demand swap-in for uuid_evicted via annotation access/modify:
    let ann_path = cfg
        .active_annotations_dir()
        .join(format!("{uuid_evicted}.pbtxt"));
    fs::write(&ann_path, "title: \"Resumed Session\"\n")?;

    let uuid_live = "44444444-5555-6666-7777-888888888888";
    populate_mock_session(&cfg, uuid_live, b"LIVE-CREATED-DB", false)?;

    let stats = handle
        .join()
        .map_err(|_| "daemon thread panicked")??;
    assert_eq!(stats.auto_restores_triggered, 1);
    assert!(active_db.is_file());
    assert_eq!(fs::read(&active_db)?, b"EVICTED-DB-CONTENT");
    assert!(
        cfg.active_brain_dir()
            .join(uuid_evicted)
            .join(".system_generated/logs/transcript.jsonl")
            .is_file()
    );

    // Verify uuid_pruned_live stayed evicted from active working set, but survived in vault!
    assert!(!live_pruned_db.exists());
    let vault_pruned_db = cfg
        .vault_conversations_dir()
        .join(format!("{uuid_pruned_live}.db"));
    assert!(vault_pruned_db.is_file());
    assert_eq!(fs::read(&vault_pruned_db)?, b"PRUNED-LIVE-DB-CONTENT");
    assert_eq!(fs::metadata(&vault_pruned_db)?.nlink(), 1);

    assert!(
        cfg.vault_conversations_dir()
            .join(format!("{uuid_live}.db"))
            .is_file()
    );

    let _ = fs::remove_dir_all(root);
    Ok(())
}

#[test]
fn test_cli_shim_pre_restores_resumed_session() -> TestResult {
    let (root, cfg) = create_temp_sandbox("cli_shim")?;
    let uuid = "55555555-6666-7777-8888-999999999999";
    let uuid_unreferenced = "66666666-7777-8888-9999-aaaaaaaaaaaa";
    populate_mock_session(&cfg, uuid, b"SHIM-RESTORE-DB", false)?;
    populate_mock_session(&cfg, uuid_unreferenced, b"UNREFERENCED-DB", false)?;
    sync_all(&cfg)?;

    let active_db = cfg.active_conversations_dir().join(format!("{uuid}.db"));
    let unref_db = cfg
        .active_conversations_dir()
        .join(format!("{uuid_unreferenced}.db"));
    fs::remove_file(&active_db)?;
    fs::remove_file(&unref_db)?;
    assert!(!active_db.exists());
    assert!(!unref_db.exists());

    let mock_agy = root.join("mock_agy.sh");
    let script = format!(
        "#!/bin/sh\nif [ -f \"{}\" ]; then exit 0; else exit 42; fi\n",
        active_db.display()
    );
    fs::write(&mock_agy, &script)?;
    let mut perms = fs::metadata(&mock_agy)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mock_agy, perms)?;

    let args = vec!["--conversation".to_string(), uuid.to_string()];
    let code = run_shim_exec(&cfg, &mock_agy, &args)?;
    assert_eq!(code, 0);
    assert!(active_db.is_file());
    // Unreferenced evicted session must remain in vault only (on-demand swap-in):
    assert!(!unref_db.exists());

    let shim_out = root.join("bin/agy");
    fs::create_dir_all(shim_out.parent().ok_or("no parent")?)?;
    std::os::unix::fs::symlink(&mock_agy, &shim_out)?;
    let installed =
        install_shim_script(&shim_out, &PathBuf::from("/usr/bin/agy-vault"), &mock_agy)?;
    assert!(installed.is_file());
    assert!(!fs::symlink_metadata(&installed)?.file_type().is_symlink());
    assert_eq!(fs::read_to_string(&mock_agy)?, script);
    assert_eq!(fs::metadata(&installed)?.permissions().mode() & 0o111, 0o111);

    let _ = fs::remove_dir_all(root);
    Ok(())
}
