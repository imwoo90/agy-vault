# Rust Agent-Native Guide & Standards (AGENTS.md)

This file defines the project-specific coding guidelines, architectural constraints, and compilation standards for `agy-vault`.

---

## 1. Architectural Baseline: Agent-Native Core
This project strictly adheres to the compiler-enforced **Agent-Native** architecture.

👉 **Mandatory Baseline Reading**: Consult [AGENT_NATIVE.md](file://AGENT_NATIVE.md) for the authoritative specification on:
- **Core Philosophy**: Living LLM-Wiki (`mod.rs` / `lib.rs` = `index.md`), compile-checked intra-doc links, and living doctests.
- **Compiler Hard Limits**: Rules 1-4 enforced via `build.rs` and `build_linter.rs` (module headers, logical code limits, inline test limits, function limits).
- **Anti-Code-Golfing Principle**: Decompose into cohesive submodules; never compress variable names or whitespace.
- **Safety Lints**: `missing_docs = "deny"`, `clippy::unwrap_used = "deny"`, `clippy::expect_used = "deny"` in `Cargo.toml`.

---

## 2. Project Architecture & Domain Knowledge (`agy-vault`)

`agy-vault` prevents Google Antigravity (`agy` CLI & IDE Language Server) from permanently destroying conversation history when its hardcoded 500-session garbage collector (`MaxConversations = 500`) runs `unlink()` on `~/.gemini/antigravity-cli/conversations/<uuid>.db` and `brain/<uuid>/`.

### Module Catalog
- [`src/lib.rs`](file://src/lib.rs): Crate root and Living LLM-Wiki index re-exporting the vault engine, restoration, and daemon interfaces.
- [`src/vault_engine.rs`](file://src/vault_engine.rs): Core `ext4` inode hardlink mirroring (`link(2)`), SQLite `.db` / `.db-wal` / `.db-shm` consistency preservation, and recursive `brain/<uuid>/**` hardlinking.
- [`src/restore.rs`](file://src/restore.rs): Reverse session restoration (`restore_session`), UUID prefix resolution, and zero-overhead storage telemetry (`compute_status`).
- [`src/daemon.rs`](file://src/daemon.rs): Real-time Linux `inotify` watcher daemon that forward-links newly created session files and triggers `< 1ms` reverse restoration when `annotations/<uuid>.pbtxt` is accessed for a pruned session, plus transparent CLI shim execution (`shim-exec`).
- [`src/main.rs`](file://src/main.rs): Command-line interface providing `sync`, `status`, `restore`, `daemon`, `shim-exec`, and `install-shim` subcommands.

---

## 3. Verification Protocol
Always verify code changes before completing any task:

```bash
# 1. Check compiler linter rules (Agent-Native constraints)
cargo check

# 2. Run unit tests, integration tests, and doctests
cargo test --all-targets

# 3. Verify doc link integrity and ensure zero warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# 4. Run Clippy across all targets
cargo clippy --all-targets
```
