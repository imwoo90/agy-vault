# agy-vault: Architecture & Binary Disassembly Analysis

## 1. Root Cause in Google Antigravity (`agy`)
Disassembly of `agy` (`google3/third_party/jetski`) reveals how the 500-conversation limit works:

1. **`CreateLanguageServerAndServe`**:
   ```asm
   84 d2                test   %dl, %dl
   ba f4 01 00 00       mov    $0x1f4, %edx       ; MaxConversations = 500 (0x01f4)
   41 b9 00 00 00 00    mov    $0x0, %r9d         ; 0 = unlimited (disable pruning)
   49 0f 45 d1          cmovne %r9, %rdx
   ```
2. **`trajectorystore.(*Manager).Init` & `PruneAll`**:
   ```asm
   48 85 ff             test   %rdi, %rdi         ; if maxConversations <= 0
   0f 8e 00 01 00 00    jle    <skip_prune>       ; skip PruneAll completely!
   e8 08 1f 00 00       call   <PruneAll>         ; otherwise unlink oldest .db and brain/<uuid>
   ```
3. **Why the Web IDE Sidebar Hangs on >500th Sessions**:
   - `PruneAll` deletes `conversations/<uuid>.db` and `brain/<uuid>/`, but never deletes rows from `conversation_summaries.db`.
   - When a user clicks an older session in the Web IDE sidebar, `(*CascadeManager).LoadTrajectory` calls `LoadUnsafe`, which checks `conversations/<uuid>.db` on disk and fails with `trajectory <uuid> not found in any store`, causing an infinite loading spinner.

---

## 2. Dual-Layer Defense Architecture

```
~/.gemini/antigravity-cli/                      ~/.gemini/agy-vault/
├── bin/agy (MaxConversations patched 500 -> 0)
├── conversations/ (all sessions preserved)     ├── conversations/ (ext4 hardlink mirror)
│   └── <uuid>.db ═══════(same inode)═════════► │   └── <uuid>.db (+ .db-wal, .db-shm)
├── brain/         (all sessions preserved)     ├── brain/         (ext4 hardlink mirror)
│   └── <uuid>/   ═══════(recursive link)═════► │   └── <uuid>/**
└── annotations/                                └── index.json     (vault telemetry)
```

1. **Layer 1 — Streaming Machine-Code Signature Patcher (`src/shim.rs`)**:
   - Scans `bin/agy` in 64 KB chunks (`< 1 MB` daemon RSS) for the unique 17-byte instruction signature `84 d2 ba f4 01 00 00 41 b9 00 00 00 00 49 0f 45 d1`.
   - Atomically patches `ba f4 01 00 00` (`500`) to `ba 00 00 00 00` (`0`) via temporary copy + `rename(2)` so running `agy` processes never hit `ETXTBSY`.
   - Watches `~/.gemini/antigravity-cli/bin/` via `inotify` (`IN_MOVED_TO | IN_CLOSE_WRITE`) to automatically re-patch `bin/agy` whenever Google Antigravity auto-updates.
2. **Layer 2 — Zero-Copy `ext4` Hardlink Vault (`src/vault_engine.rs`, `src/restore.rs`, `src/daemon.rs`)**:
   - Mirrors every `.db`, `.db-wal`, `.db-shm`, `brain/<uuid>/**`, and `annotations/<uuid>.pbtxt` into `~/.gemini/agy-vault/` via `link(2)` (`0` bytes extra disk space while shared).
   - Automatically restores any missing session files from `~/.gemini/agy-vault/` on startup, on `IN_DELETE`, or on `annotations/<uuid>.pbtxt` access in `< 1ms`.
