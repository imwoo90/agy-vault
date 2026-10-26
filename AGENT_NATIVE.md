# Agent-Native Architecture Standards & Guidelines (AGENT_NATIVE.md)

This specification defines the universal architectural standards, compile-time constraints, and development workflows for **Agent-Native** Rust codebases.

Built upon the **"Code as Documentation" (Living LLM-Wiki)** philosophy, this framework turns the Rust compiler into an active collaborator and strict arbiter, ensuring codebases remain compact, modular, and self-documenting with zero documentation drift.

---

## 1. Core Philosophy: Code as Documentation (LLM-Wiki)

To prevent documentation rot and maximize the signal-to-noise ratio (SNR) in LLM context windows, the Rust source code itself acts as the living knowledge base for architecture, interfaces, and design decisions.

1. **`mod.rs` = `index.md` (Module Architecture Catalog)**:
   * Every directory's `mod.rs` acts as the `index.md` (table of contents and architectural map) for that domain.
   * `mod.rs` headers MUST provide:
     - `## Overview`: High-level domain responsibility in 1-2 concise sentences.
      - `## Submodules`: Table of contents linking each submodule along with its explicit relative physical path (`- [`[`submodule`]`] (\`submodule.rs\`): ...` for leaf files, or `- [`[`submodule`]`] (\`submodule/mod.rs\`): ...` for directory modules) and a **1-line single-responsibility summary**. Explicit path disambiguation (`foo.rs` vs `foo/mod.rs`) eliminates 404 file-not-found round-trips and allows AI agents to navigate to the exact file in $O(\log N)$ steps without opening every child file.
      - `## Search Tags`: Comma-separated search keywords (`#tag1, #tag2`) for fast deterministic grep.
2. **Compiler-Verified Documentation**:
   * Never write documentation that cannot be compiled. Structs, traits, and public functions must use standard doc comments (`///`) with executable code examples (`doctests`).
   * When running `cargo test`, the compiler executes all doctests, guaranteeing documentation never becomes obsolete.
3. **Compile-Checked Intra-Doc Links (Type-Safe Graph)**:
   * Reference other types, modules, or functions using Rust's native intra-doc link syntax (e.g. `[`[`Calculator`](crate::example::Calculator)`]`).
   * Rustdoc validates every link at build time (`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`), eliminating broken references and hallucinations.
   * *Anti-Pattern (No Manual Collaboration Graphs)*: Do NOT manually write natural-language "Collaboration Graphs" in leaf files. Rust's native type system, `use` declarations, and compiler-verified intra-doc links already define the dependency graph with zero drift.

---

## 2. LLM-Agent Constraints (Enforced at Compile-Time via `build.rs` & `build_linter.rs`)

To prevent monoliths and keep files optimized for LLM context windows, strict AST constraints are enforced during `cargo check`, `cargo build`, and `cargo test`:

1. **Rule 1: File-Level Living Wiki Header (Min 100 Chars, English-Only & Schema Enforcement)**:
   * Every non-test production `.rs` file must begin with a file-level doc comment (`//!`) of **at least 100 characters**.
   * **English-Only Documentation & Comments**: All documentation (`//!`, `///`) and inline comments (`//`, `/* */`) MUST be written strictly in concise **English**. English technical prose achieves 2.5x–3x higher LLM tokenizer efficiency than CJK/Hangul scripts and preserves 100% lexical consistency with Rust identifiers and `#search-tags`. Non-English scripts (Hangul/CJK) in comments are rejected at compile time.
   * **Compile-Time Schema Enforcement (`build_linter.rs`)**:
     - `## Overview`: Mandatory for all production files. Details component responsibility in 1-2 concise sentences.
     - `## Search Tags`: Mandatory for all production files. Must include deterministic hashtags (e.g. `#keyword`).
     - `## Submodules`: Mandatory for any module declaring child submodules via `mod <name>;`. Every declared submodule must be documented with its explicit relative path (`(<name>.rs)` or `(<name>/mod.rs)`) and a 1-line summary; omitting a submodule or mismatching its on-disk path (`<name>.rs` vs `<name>/mod.rs`) triggers a compilation error (zero catalog or path drift).
   * Schema enforcement is enabled by default (`enforce_doc_schema = true`), configurable via `.agent-lint.toml`.
2. **Rule 2: Production Logical Code Limit (Max 10,000 Characters)**:
   * The character count of active production code (excluding comments, doc comments, blank lines, and `#[cfg(test)]` modules) must not exceed **10,000 characters** (~200–300 lines of SLOC).
   * Exceeding this limit indicates bloated responsibility; split into cohesive submodules.
   * *Strict Enforcement*: Limits cannot be bypassed via code attributes (`#[allow(...)]`). To adjust project-wide ceilings, configure `.agent-lint.toml`.
3. **Rule 2b: Inline Unit Test Limit in `src/` (Max 5,000 Characters)**:
   * Inline unit tests (`#[cfg(test)]`) within a `src/` file must not exceed **5,000 characters**.
   * When unit tests grow beyond 5,000 characters:
     - **Integration tests** (public API): move to the root `tests/` directory (e.g. `tests/<module>_test.rs`).
     - **Unit tests** (requiring access to private/crate items): extract into a dedicated submodule file (e.g. `src/<module>/tests.rs` or `src/<module>_tests.rs` with `#[cfg(test)] mod tests;`).
     This preserves encapsulation while keeping production files compact for LLM context windows.
4. **Rule 3: File Documentation Limit (Max 4,000 Characters)**:
   * Total character count of documentation comments (`//`, `///`, `//!`, `/* */`) must not exceed **4,000 characters** (~50–80 lines).
   * Keeps descriptions concise and high-signal, preventing LLM prompt bloat.
5. **Rule 4: Function Physical Size & Parameter Discipline (Max 2,000 Chars / UI Components Max 3,500 Chars)**:
   * A single imperative logic function (production or test, including signature, body, comments, and braces) must not exceed **2,000 characters** (~40–50 physical lines).
   * **Rule 4b (Declarative UI Component Allowance — Max 3,500 Characters)**: Functions annotated with `#[component]` or containing declarative UI markup macros (`rsx!`, `html!`, `view!`) have an elevated ceiling of **3,500 characters** (`max_ui_component_chars = 3500` in `.agent-lint.toml`). Because declarative markup has low cyclomatic complexity (`CC ≈ 1`) while utility CSS strings (e.g. Tailwind `class: "..."`) consume 40–60% of the character budget, this prevents artificial micro-fragmentation of cohesive UI views while still blocking monolithic God Components.
   * *Strict Enforcement (No Escape Hatches)*: Functions and `impl` blocks cannot bypass structural complexity limits via `#[allow(clippy::too_many_lines)]` or `#[allow(clippy::too_many_arguments)]`. When decomposing functions, encapsulate related arguments into a cohesive context or accumulator struct rather than passing 8+ primitive parameters.
6. **Rule 5: Anti-Code-Golfing & Anti-Shallow-Module Principles**:
   * *Anti-Code-Golfing*: Never compress variable names (e.g. `transaction_context` -> `tc`), eliminate idiomatic whitespace/newlines, or abuse macros to artificially circumvent character limits.
   * *Anti-Shallow-Module (Ousterhout's Deep Modules)*: Do not over-fragment code into trivial pass-through leaf files (e.g. a file containing only `pub use` re-exports or a 5-line wrapper where the `//!` header is larger than the code). Keep re-exports in `mod.rs` and group cohesive types or small helpers into a shared domain module.

---

## 3. Standard Quality & Safety Lints (Enforced via `Cargo.toml [lints]`)

Leveraging Rust 1.74+ package-level lint declarations, standard compiler and Clippy lints are elevated to hard compilation errors:

1. **`missing_docs = "deny"` (Rust Standard Lint)**:
   * Enforces the Living LLM-Wiki contract: every public struct, enum, function, trait, and type alias must have outer doc comments (`///`).
2. **`clippy::unwrap_used = "deny"` & `clippy::expect_used = "deny"` (Clippy Standard Lints)**:
   * Prohibits `.unwrap()` and `.expect()` in production code, enforcing typed error handling (`Result<T, E>` and `?`).
   * *Test Exception*: In tests (`tests/` or `#[cfg(test)]`), you may annotate `#![allow(clippy::unwrap_used, clippy::expect_used)]` or return `Result<(), Box<dyn std::error::Error>>` to streamline assertions.

---

## 4. Development Workflow (TDD & Verification Baseline)

When implementing features or bug fixes, always adhere to the verification cycle:

1. **Write Failing Tests First**: Write unit tests or doctests describing expected behavior before implementation.
2. **Implement Code & Docs**: Write clean, idiomatic code alongside concise, accurate doc comments.
3. **Verify Baseline Compliance**:
   ```bash
   # 1. Check compiler linter rules (Agent-Native constraints)
   cargo check

   # 2. Run unit tests, integration tests, and doctests
   cargo test --all-targets

   # 3. Verify doc link integrity and ensure zero warnings
   RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
   ```

---

## 5. AI Agent Navigation Guide

For AI coding agents interacting with an Agent-Native repository:

1. **Entry Points**: Always start with `AGENTS.md` and the crate root (`src/main.rs` or `src/lib.rs`) to grasp top-level domain architecture and verification commands.
2. **Zero-404 Module Indexing (`mod.rs = index.md`)**: Treat `mod.rs` in any directory as the module's architecture map. Read its `## Submodules` catalog to see the exact relative path (`(foo.rs)` vs `(foo/mod.rs)`) and 1-line responsibility of each child. Open only the relevant target files in parallel and skip unrelated sibling modules.
3. **Deterministic Traversal**: Follow compile-checked intra-doc links (`[Type]`) and `#tag` search tags to traverse dependencies deterministically without blind full-codebase searching.
4. **Pre-commit Verification**: Never consider a task complete without passing the verification commands specified in the project's `AGENTS.md`.
