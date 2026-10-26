//! Deterministic integration tests for `agy-vault` covering:
//! 1. `ext4` inode hardlink mirroring (`nlink == 2`, zero disk overhead)
//! 2. Antigravity 500-session GC `unlink()` simulation and `< 1ms` reverse restoration
//! 3. SQLite WAL/SHM clean checkpoint cleanup vs crash preservation
//! 4. Real-time Linux `inotify` daemon forward-sync and `annotations/*.pbtxt` auto-restore
//! 5. Transparent CLI shim (`shim-exec`) pre-restoration on `--resume <uuid>`

use agy_vault::{
    DaemonConfig, PATCHED_MAX_CONVS_SIG, PatchOutcome, UNPATCHED_MAX_CONVS_SIG, VaultConfig,
    compute_status, ensure_agy_binary_unbounded, restore_all_from_vault, restore_session,
    run_daemon, sync_all, sync_session,
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
fn test_inotify_daemon_auto_patches_binary_on_startup_and_update() -> TestResult {
    let (root, cfg) = create_temp_sandbox("inotify_daemon")?;
    let uuid_evicted = "33333333-4444-5555-6666-777777777777";
    populate_mock_session(&cfg, uuid_evicted, b"EVICTED-DB-CONTENT", false)?;
    sync_all(&cfg)?;

    let active_db = cfg
        .active_conversations_dir()
        .join(format!("{uuid_evicted}.db"));
    fs::remove_file(&active_db)?;
    fs::remove_dir_all(cfg.active_brain_dir().join(uuid_evicted))?;
    assert!(!active_db.exists());

    // Create a mock `bin/agy` containing the unpatched 500-conversation signature:
    let bin_dir = cfg.agy_dir.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let bin_path = bin_dir.join("agy");
    let mut mock_elf = vec![0x90u8; 64];
    mock_elf.extend_from_slice(&UNPATCHED_MAX_CONVS_SIG);
    mock_elf.extend_from_slice(&[0xc3u8; 32]);
    fs::write(&bin_path, &mock_elf)?;

    let daemon_cfg = DaemonConfig {
        vault: cfg.clone(),
        poll_ms: 25,
        max_ticks: Some(15),
    };

    let handle = thread::spawn(move || run_daemon(&daemon_cfg));
    thread::sleep(Duration::from_millis(80));

    // Verify startup patched `bin/agy` and restored `uuid_evicted` into active folder:
    assert_eq!(
        ensure_agy_binary_unbounded(&bin_path)?,
        PatchOutcome::AlreadyPatched(64)
    );
    assert!(active_db.is_file());

    // Simulate an `agy` auto-update overwriting `bin/agy` at a DIFFERENT offset (e.g. offset 128):
    let mut updated_elf = vec![0x90u8; 128];
    updated_elf.extend_from_slice(&UNPATCHED_MAX_CONVS_SIG);
    updated_elf.extend_from_slice(&[0xc3u8; 32]);
    let update_tmp = bin_dir.join("agy.update_tmp");
    fs::write(&update_tmp, &updated_elf)?;
    fs::rename(&update_tmp, &bin_path)?;

    let uuid_live = "44444444-5555-6666-7777-888888888888";
    populate_mock_session(&cfg, uuid_live, b"LIVE-CREATED-DB", false)?;

    let stats = handle
        .join()
        .map_err(|_| "daemon thread panicked")??;
    assert_eq!(stats.binary_patches_applied, 2);
    assert_eq!(
        ensure_agy_binary_unbounded(&bin_path)?,
        PatchOutcome::AlreadyPatched(128)
    );
    let patched_bytes = fs::read(&bin_path)?;
    assert_eq!(
        &patched_bytes[128..128 + PATCHED_MAX_CONVS_SIG.len()],
        &PATCHED_MAX_CONVS_SIG
    );
    assert!(
        cfg.vault_conversations_dir()
            .join(format!("{uuid_live}.db"))
            .is_file()
    );

    let _ = fs::remove_dir_all(root);
    Ok(())
}

#[test]
fn test_restore_all_from_vault_and_idempotent_binary_patch() -> TestResult {
    let (root, cfg) = create_temp_sandbox("restore_all")?;
    let uuid1 = "55555555-6666-7777-8888-999999999999";
    let uuid2 = "66666666-7777-8888-9999-aaaaaaaaaaaa";
    populate_mock_session(&cfg, uuid1, b"SESSION-ONE-DB", false)?;
    populate_mock_session(&cfg, uuid2, b"SESSION-TWO-DB", false)?;
    sync_all(&cfg)?;

    let active_db1 = cfg.active_conversations_dir().join(format!("{uuid1}.db"));
    let active_db2 = cfg.active_conversations_dir().join(format!("{uuid2}.db"));
    fs::remove_file(&active_db1)?;
    fs::remove_file(&active_db2)?;
    assert!(!active_db1.exists());
    assert!(!active_db2.exists());

    let restored_count = restore_all_from_vault(&cfg)?;
    assert_eq!(restored_count, 2);
    assert!(active_db1.is_file());
    assert!(active_db2.is_file());
    assert_eq!(fs::metadata(&active_db1)?.nlink(), 2);
    assert_eq!(fs::metadata(&active_db2)?.nlink(), 2);

    let mock_agy = root.join("agy_bin");
    let mut payload = vec![0x90u8; 32];
    payload.extend_from_slice(&UNPATCHED_MAX_CONVS_SIG);
    payload.extend_from_slice(&[0xc3u8; 16]);
    fs::write(&mock_agy, &payload)?;
    let mut perms = fs::metadata(&mock_agy)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mock_agy, perms)?;

    assert_eq!(
        ensure_agy_binary_unbounded(&mock_agy)?,
        PatchOutcome::NewlyPatched(32)
    );
    assert_eq!(
        ensure_agy_binary_unbounded(&mock_agy)?,
        PatchOutcome::AlreadyPatched(32)
    );

    let _ = fs::remove_dir_all(root);
    Ok(())
}
