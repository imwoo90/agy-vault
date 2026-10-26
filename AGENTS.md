# Rust Agent-Native Guide & Standards (AGENTS.md)

This file defines the project-specific coding guidelines, architectural constraints, and compilation standards for `agy-vault`.

---

## 1. Architectural Baseline: Agent-Native Core
This project strictly adheres to the compiler-enforced **Agent-Native** architecture.

👉 **Mandatory Baseline Reading**: Consult [AGENT_NATIVE.md](file://AGENT_NATIVE.md) for the authoritative specification on:
- **Core Philosophy**: Living LLM-Wiki (`mod.rs` / `lib.rs` = `index.md`), compile-checked intra-doc links, and living doctests.
- **Compiler Hard Limits**: Rules 1-5 enforced via `build.rs` and `build_linter.rs` (module headers, logical code limits, inline test limits, function limits, shallow module prohibition).
- **Anti-Code-Golfing Principle**: Decompose into cohesive submodules; never compress variable names or whitespace.
- **Safety Lints**: `missing_docs = "deny"`, `clippy::unwrap_used = "deny"`, `clippy::expect_used = "deny"` in `Cargo.toml`.

---

## 2. Project Architecture & Domain Knowledge (`agy-vault`)

`agy-vault` prevents Google Antigravity (`agy` CLI & IDE Language Server) from destroying conversation history beyond 500 sessions (`MaxConversations = 500`) by combining a streaming x86_64 machine-code signature patcher (`500 -> 0` unlimited mode) with a real-time `ext4` hardlink vault (`~/.gemini/agy-vault/`).

### Module Catalog
- [`src/lib.rs`](file://src/lib.rs): Crate root and Living LLM-Wiki index re-exporting the vault engine, restoration, binary patcher, and daemon interfaces.
- [`src/vault_engine.rs`](file://src/vault_engine.rs): Core `ext4` inode hardlink mirroring (`link(2)`), SQLite `.db` / `.db-wal` / `.db-shm` consistency preservation, and recursive `brain/<uuid>/**` hardlinking.
- [`src/restore.rs`](file://src/restore.rs): Reverse session restoration (`restore_session`), UUID prefix resolution, and zero-overhead storage telemetry (`compute_status`).
- [`src/shim.rs`](file://src/shim.rs): CLI argument parser, vault index writer, `restore_all_from_vault`, and streaming 64 KB x86_64 binary signature scanner/patcher (`ensure_agy_binary_unbounded`).
- [`src/daemon.rs`](file://src/daemon.rs): Real-time Linux `inotify` watcher daemon monitoring `conversations/`, `brain/`, `annotations/`, and `bin/` (auto-patching `bin/agy` on updates and keeping both active and vault directories 100% synchronized).
- [`src/main.rs`](file://src/main.rs): Command-line interface providing `sync`, `status`, `restore`, `daemon`, and `patch-binary` subcommands.

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
cargo clippy --all-targets -- -D warnings
```
