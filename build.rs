//! Build script for rust-agent-native-template.
//!
//! Integrates the Agent-Native linter and provides a clean, welcoming canvas for
//! user-defined pre-build logic (code generation, C FFI, assets, etc.).

#[path = "build_linter.rs"]
mod build_linter;

fn main() {
    // 1. Enforce Agent-Native architectural constraints (AGENTS.md Rules 1-4).
    // Thresholds can be customized in `.agent-lint.toml`.
    build_linter::run_linter();

    // 2. User-defined custom pre-build logic can be added below:
    // e.g., tonic_build::compile_protos(...), cc::Build::new()..., etc.
}
