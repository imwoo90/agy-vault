# agy-vault

Zero-Knowledge Antigravity Session Preservation & Auto-Restore Daemon built with the **Rust Agent-Native** architecture.

## Overview

Google Antigravity (`agy` CLI & IDE Language Server) enforces a hardcoded 500-conversation limit (`MaxConversations = 500`). When total sessions exceed 500, `agy` silently `unlink()`s the oldest `~/.gemini/antigravity-cli/conversations/<uuid>.db` and `brain/<uuid>/` directories while leaving the index entries in `conversation_summaries.db` and `annotations/<uuid>.pbtxt`. Attempting to open any conversation older than the latest 500 causes an infinite loading spinner.

`agy-vault` eliminates this limitation with **zero disk overhead** for active sessions:

1. **Zero-Copy `ext4` Hardlink Mirroring (`link(2)`)**:
   - Hardlinks `~/.gemini/antigravity-cli/conversations/<uuid>.db*`, `brain/<uuid>/**`, and `annotations/<uuid>.pbtxt` into `~/.gemini/agy-vault/`.
   - Active sessions share the exact same `ext4` inode (`nlink = 2`, 0 bytes of additional disk usage).
   - When `agy` garbage-collects an old session via `unlink()`, `nlink` decrements from `2 -> 1`, preserving 100% of the SQLite `.db` and `brain/transcript.jsonl` files inside `~/.gemini/agy-vault/`.
2. **Instant `< 1ms` Reverse Restoration (`inotify` + CLI Shim)**:
   - Watches `~/.gemini/antigravity-cli/annotations/` via Linux `inotify`. When the IDE or CLI accesses `annotations/<uuid>.pbtxt` for a pruned conversation, `agy-vault` immediately hardlinks `conversations/<uuid>.db*` and `brain/<uuid>/**` back into `~/.gemini/antigravity-cli/` and updates `.db` modification time to keep it inside the active LRU window.
   - Provides a transparent CLI wrapper (`agy-vault shim-exec` / `install-shim`) that pre-restores any `--resume <uuid>` target prior to invoking the real `agy` binary.

## CLI Usage

```bash
# Synchronize all active conversations, brain directories, and annotations into the vault
cargo run --release -- sync

# Inspect vault telemetry and zero-overhead hardlink status
cargo run --release -- status

# Restore a specific pruned conversation by full UUID or prefix
cargo run --release -- restore <uuid-or-prefix>

# Run the real-time inotify forward-mirroring & reverse-restoration daemon
cargo run --release -- daemon
```
