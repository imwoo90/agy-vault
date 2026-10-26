# agy-vault: Zero-Knowledge Antigravity Session Preservation & Auto-Restore Daemon

## 1. Background & Problem Statement
Google Antigravity (`agy` CLI & IDE Language Server, built on `google3/third_party/jetski`) enforces a hardcoded 500-conversation garbage collection limit (`MaxConversations = 500`):
- Whenever total sessions exceed 500, `agy` silently `unlink()`s the oldest `conversations/<uuid>.db` (and `.db-wal` / `.db-shm`) and `brain/<uuid>/` directories.
- However, `agy` **never deletes** the index entries in `conversation_summaries.db` or `annotations/<uuid>.pbtxt` (e.g., 1,736+ historical records remain visible in the UI sidebar).
- Clicking any conversation older than the most recent 500 in the Antigravity IDE or CLI `/resume` results in an **infinite loading spinner** or `HTTP 500` because the underlying `.db` and `brain/` files were unlinked.

---

## 2. Core Architecture (`ext4` Hardlink Vault + Reverse Restoration)

```
~/.gemini/antigravity-cli/                  ~/.gemini/agy-vault/
├── conversations/ (capped at 500 by agy)   ├── conversations/ (unlimited archive)
│   └── <uuid>.db ══════(same inode)══════► │   └── <uuid>.db (+ .db-wal, .db-shm)
├── brain/         (capped at 500 by agy)   ├── brain/         (unlimited archive)
│   └── <uuid>/   ══════(recursive link)══► │   └── <uuid>/**
└── annotations/   (1,736+ kept by agy)     └── index.json     (vault metadata)
```

### Key Mechanisms
1. **Zero-Copy `ext4` Hardlink Mirroring (`link(2)`)**:
   - Every file in `~/.gemini/antigravity-cli/conversations/<uuid>.db*` and `~/.gemini/antigravity-cli/brain/<uuid>/**` is hardlinked into `~/.gemini/agy-vault/`.
   - **0 Bytes Overhead**: While a session is among the latest 500, both paths share the exact same ext4 inode (`nlink = 2`).
   - **GC Immunity**: When `agy` runs its 500-session cleanup and calls `unlink()`, `nlink` decrements from `2 -> 1`. The file data remains 100% intact on disk inside `~/.gemini/agy-vault/` without a single byte copied.
2. **Transparent CLI Shim (`~/.local/bin/agy`)**:
   - Wraps the real `~/.gemini/antigravity-cli/bin/agy` binary.
   - **Pre-exec Hook**: If `--resume <uuid>` is passed and `<uuid>.db` was pruned from `conversations/`, immediately hardlinks `<uuid>.db*` and `brain/<uuid>/**` back from `~/.gemini/agy-vault/` before `execve()`.
   - **Post-exec / Background Sweep**: Hardlinks any newly created or updated session files into `~/.gemini/agy-vault/`.
3. **Real-Time `inotify` Daemon (`agy-vault daemon`)**:
   - Watches `~/.gemini/antigravity-cli/conversations/`, `brain/`, and `annotations/`.
   - **Forward Sync**: Immediately hardlinks newly created `.db`, `.db-wal`, `.db-shm`, and `brain/<uuid>/**` files into `~/.gemini/agy-vault/`.
   - **Reverse Restore on IDE Click (`IN_ACCESS` / `IN_OPEN` / `IN_MODIFY` on `annotations/<uuid>.pbtxt`)**: When the Antigravity Web/Desktop IDE attempts to load a pruned session (touching or reading its `annotations/<uuid>.pbtxt`), the daemon checks if `conversations/<uuid>.db` is missing and restores the hardlink from `~/.gemini/agy-vault/` in `< 1ms` so the IDE loads the old conversation seamlessly without infinite loading.

---

## 3. Implementation Phases (Ready to Execute on "진행해")

- **Phase 1 — Rust Workspace & Agent-Native Linter Setup (`projects/agy-vault`)**:
  - Initialize Cargo project following `rust-agent-native-template` (`AGENT_NATIVE.md`, `AGENTS.md`, `GEMINI.md`, `build.rs` linter, strict `clippy` & `rustdoc` lints).
- **Phase 2 — Core Hardlink Vault Engine (`src/vault_engine.rs`)**:
  - Implement atomic hardlink mirroring for `conversations/<uuid>.db*` (including WAL/SHM consistency) and recursive directory hardlinking for `brain/<uuid>/**`.
  - Implement `restore_session(uuid)` and `vault_status()` (`active_count`, `vaulted_only_count`, `total_protected_count`).
- **Phase 3 — `inotify` Watcher Daemon & Transparent `agy` Shim (`src/daemon.rs`, `src/shim.rs`)**:
  - Implement real-time `inotify` forward-mirroring and `annotations/<uuid>.pbtxt` reverse-restoration trigger.
  - Implement subcommands: `agy-vault sync`, `agy-vault status`, `agy-vault restore <uuid>`, `agy-vault daemon`, and `agy-vault shim-exec -- <args...>`.
- **Phase 4 — Deterministic Verification & Systemd User Service (`tests/`)**:
  - Unit & integration tests simulating `agy`'s 500-session `unlink()` GC and verifying 100% survival + instant `< 1ms` reverse restoration of `.db` and `brain/transcript.jsonl`.
  - Dry-run & initial non-destructive hardlink sync on host (`500` active sessions protected immediately).
