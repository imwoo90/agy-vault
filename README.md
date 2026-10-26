# agy-vault

Zero-Knowledge Antigravity Session Preservation & Auto-Patching Daemon built with the **Rust Agent-Native** architecture (`0` runtime dependencies, `< 1 MB` RSS footprint).

## Overview

Google Antigravity (`agy` CLI & Web/Desktop IDE Language Server) enforces a hardcoded 500-conversation garbage collection limit (`MaxConversations = 500` in `CreateLanguageServerAndServe`). Whenever `agy` starts and total sessions exceed 500, `trajectorystore.(*Manager).Init` calls `PruneAll(500)`, silently `unlink()`ing the oldest `~/.gemini/antigravity-cli/conversations/<uuid>.db` and `brain/<uuid>/` directories while leaving the index rows in `conversation_summaries.db`. Clicking any conversation older than the latest 500 in the Web IDE sidebar causes an infinite loading spinner.

`agy-vault` permanently eliminates this limitation with **zero wrapper scripts** and **zero disk overhead**:

1. **Streaming Machine-Code Signature Patcher (`MaxConversations: 500 -> 0`)**:
   - Inside `agy`, `trajectorystore.(*Manager).Init` and `PruneAll` natively treat `maxConversations <= 0` as **unlimited** (`test %rdi, %rdi; jle` skips `PruneAll` completely).
   - `agy-vault` scans `~/.gemini/antigravity-cli/bin/agy` using a 64 KB streaming chunk scanner (`< 1 MB` RSS) for the 17-byte x86_64 instruction signature (`84 d2 ba f4 01 00 00 41 b9 00 00 00 00 49 0f 45 d1`) and atomically patches `mov $0x1f4, %edx` (`500`) to `mov $0x0, %edx` (`0`) via kernel copy + `rename(2)`.
   - Watches `~/.gemini/antigravity-cli/bin/` via Linux `inotify` (`IN_MOVED_TO | IN_CLOSE_WRITE`) so whenever `agy` auto-updates to a new version, the new binary is automatically re-patched at its updated offset without user intervention.
2. **Zero-Copy `ext4` Hardlink Vault (`~/.gemini/agy-vault/`)**:
   - Hardlinks `~/.gemini/antigravity-cli/conversations/<uuid>.db*`, `brain/<uuid>/**`, and `annotations/<uuid>.pbtxt` into `~/.gemini/agy-vault/`.
   - Both directories share the exact same `ext4` inode (`nlink = 2`, **0 bytes** of additional disk usage).
   - Even if an unpatched binary ever unlinks a session during an update window, `agy-vault` immediately restores the hardlinks from `~/.gemini/agy-vault/` in `< 1ms`.

## CLI Usage

```bash
# Build and install release binary
cargo build --release
cp target/release/agy-vault ~/.local/bin/agy-vault

# Scan and patch ~/.gemini/antigravity-cli/bin/agy (MaxConversations: 500 -> 0) and sync all sessions
agy-vault patch-binary

# Synchronize all active conversations, brain directories, and annotations with the vault
agy-vault sync

# Inspect vault telemetry and zero-overhead hardlink status
agy-vault status

# Restore a specific conversation by full UUID or prefix
agy-vault restore <uuid-or-prefix>

# Run the real-time inotify watcher & auto-patcher daemon
agy-vault daemon --poll-ms 1000
```
