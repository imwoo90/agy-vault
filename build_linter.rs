#![allow(dead_code)]
//! Agent-Native Architectural Linter Engine
//!
//! Enforces compile-time architectural constraints (AGENTS.md Rules 1-4) during
//! `cargo check`, `cargo build`, and `cargo test`. Ensures code remains compact,
//! cohesive, and living-wiki documented for high-efficiency LLM context windows.

use std::fs;
use std::ops::RangeInclusive;
use std::path::Path;
use syn::spanned::Spanned;
use syn::visit::Visit;

/// Configuration thresholds for the Agent-Native Linter.
/// Can be customized per project via `.agent-lint.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinterConfig {
    /// Minimum characters required in module-level `//!` doc header (Rule 1).
    pub min_module_doc_chars: usize,
    /// Enforces structured Living Wiki schema (## Overview, ## Search Tags, ## Submodules) (Rule 1).
    pub enforce_doc_schema: bool,
    /// Maximum logical production code characters per file (Rule 2).
    pub max_logical_code_chars: usize,
    /// Maximum inline test characters in `src/` files before requiring extraction to `tests/` (Rule 2b).
    pub max_inline_test_chars: usize,
    /// Maximum documentation characters per file (Rule 3).
    pub max_doc_chars: usize,
    /// Maximum physical characters per individual logic function (Rule 4).
    pub max_function_chars: usize,
    /// Maximum physical characters for declarative UI components (`#[component]` / `rsx!`) (Rule 4b).
    pub max_ui_component_chars: usize,
}

impl Default for LinterConfig {
    fn default() -> Self {
        Self {
            min_module_doc_chars: 100,
            enforce_doc_schema: true,
            max_logical_code_chars: 10_000,
            max_inline_test_chars: 5_000,
            max_doc_chars: 4_000,
            max_function_chars: 2_000,
            max_ui_component_chars: 3_500,
        }
    }
}

/// Loads configuration from `.agent-lint.toml` if present in the given root directory,
/// falling back to default Agent-Native limits for any unspecified settings.
pub fn load_config(root_dir: &Path) -> LinterConfig {
    let mut config = LinterConfig::default();
    let config_path = root_dir.join(".agent-lint.toml");

    let Ok(content) = fs::read_to_string(&config_path) else {
        return config;
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
            continue;
        }

        if let Some((key, val)) = trimmed.split_once('=') {
            let key = key.trim();
            let val = val.split('#').next().unwrap_or("").trim().trim_matches('"');
            if key == "enforce_doc_schema" {
                if let Ok(b) = val.parse::<bool>() {
                    config.enforce_doc_schema = b;
                }
            } else if let Ok(num) = val.parse::<usize>() {
                match key {
                    "min_module_doc_chars" => config.min_module_doc_chars = num,
                    "max_logical_code_chars" => config.max_logical_code_chars = num,
                    "max_inline_test_chars" => config.max_inline_test_chars = num,
                    "max_doc_chars" => config.max_doc_chars = num,
                    "max_function_chars" => config.max_function_chars = num,
                    "max_ui_component_chars" => config.max_ui_component_chars = num,
                    _ => {}
                }
            }
        }
    }

    config
}

/// Executes the Agent-Native linter against the `src/` directory.
/// Intended to be called from `build.rs` during pre-build compilation.
pub fn run_linter() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=.agent-lint.toml");
    println!("cargo:rerun-if-changed=build_linter.rs");

    let config = load_config(Path::new("."));
    let src_dir = Path::new("src");
    if let Err(e) = check_dir(src_dir, &config) {
        eprintln!("\n=== [Agent-Native Linter] Build Constraint Violation ===");
        eprintln!("{}\n", e);
        std::process::exit(1);
    }
}

/// Recursively traverses a directory and checks all Rust source files.
pub fn check_dir(dir: &Path, config: &LinterConfig) -> Result<(), String> {
    if !dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            check_dir(&path, config)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            check_file(&path, config)?;
        }
    }
    Ok(())
}

/// Checks an individual file against the linter configuration.
pub fn check_file(path: &Path, config: &LinterConfig) -> Result<(), String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    check_source_with_config(path, &content, config)
}

/// Checks source code string using default Agent-Native limits.
pub fn check_source(path: &Path, content: &str) -> Result<(), String> {
    check_source_with_config(path, content, &LinterConfig::default())
}

/// Checks source code string against specified linter thresholds.
pub fn check_source_with_config(
    path: &Path,
    content: &str,
    config: &LinterConfig,
) -> Result<(), String> {
    // 0. Parse file AST using syn.
    // If syntax parsing fails due to a code error or typo, gracefully pass through to rustc
    // so the compiler outputs rich, span-accurate diagnostic messages instead of masking them.
    let syn_file = match syn::parse_file(content) {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };

    // Check if the file is an integration test or benchmark file
    let is_test_file = is_path_test_file(path);

    // Pass 1: Extract test scopes (e.g. #[cfg(test)]) to preserve TDD incentives
    let mut scope_collector = TestScopeCollector {
        test_line_ranges: Vec::new(),
    };
    scope_collector.visit_file(&syn_file);

    // Pass 2: Calculate character metrics (Rules 2, 2b & 3) with string/comment-aware scanning
    let lines: Vec<&str> = content.lines().collect();
    let (prod_logical_code_chars, doc_chars, inline_test_chars, non_english_comment_line) =
        scan_code_and_comments(&lines, is_test_file, &scope_collector.test_line_ranges);

    // 0b. Enforce English-Only Documentation & Comments (Rule 1)
    if let Some(bad_line) = non_english_comment_line {
        return Err(format!(
            "Rule: AGENTS.md Rule 1 (English-Only Documentation & Comments)\nLocation: {:?}:{}\nRequirement: All documentation (`//!`, `///`) and inline comments (`//`, `/* */`) must be written strictly in concise English to maximize LLM tokenizer efficiency and lexical consistency.\nAction: Rewrite the non-English comment at line {} in English.",
            path, bad_line, bad_line
        ));
    }

    // 1. Check production logical code limit (Rule 2)
    if prod_logical_code_chars > config.max_logical_code_chars {
        let over = prod_logical_code_chars - config.max_logical_code_chars;
        return Err(format!(
            "Rule: AGENTS.md Rule 2 (Production Logical Code Limit)\nLocation: {:?}\nLimit: Maximum {} characters (excluding tests & comments)\nActual: {} characters (+{} over limit)\nAction: Refactor by splitting responsibilities into cohesive submodules. To adjust project ceilings, edit `.agent-lint.toml`.",
            path, config.max_logical_code_chars, prod_logical_code_chars, over
        ));
    }

    // 1b. Check inline unit test limit in src/ (Rule 2b)
    if !is_test_file && inline_test_chars > config.max_inline_test_chars {
        let over = inline_test_chars - config.max_inline_test_chars;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("module");
        return Err(format!(
            "Rule: AGENTS.md Rule 2b (Inline Unit Test Limit)\nLocation: {:?}\nLimit: Maximum {} characters for inline tests in src/\nActual: {} characters (+{} over limit)\nAction: Move integration tests to the root `tests/` directory (e.g. tests/{}_test.rs), or extract private unit tests into a submodule file (e.g. src/{}/tests.rs or src/{}_tests.rs) to preserve encapsulation and keep production files compact for LLM context windows.",
            path, config.max_inline_test_chars, inline_test_chars, over, stem, stem, stem
        ));
    }

    // 2. Check documentation character limit (Rule 3)
    if doc_chars > config.max_doc_chars {
        let over = doc_chars - config.max_doc_chars;
        return Err(format!(
            "Rule: AGENTS.md Rule 3 (File Documentation Limit)\nLocation: {:?}\nLimit: Maximum {} characters\nActual: {} characters (+{} over limit)\nAction: Keep documentation concise to maintain high signal-to-noise ratio for LLM context.",
            path, config.max_doc_chars, doc_chars, over
        ));
    }

    // 3. Enforce File-Level Living Wiki Header (//! at least min_module_doc_chars) for production code (Rule 1)
    if !is_test_file {
        let mut module_doc_lines = Vec::new();
        let mut module_doc_len = 0;
        for attr in &syn_file.attrs {
            if matches!(attr.style, syn::AttrStyle::Inner(_))
                && attr.path().is_ident("doc")
                && let syn::Meta::NameValue(syn::MetaNameValue {
                    value:
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }),
                    ..
                }) = &attr.meta
            {
                let val = s.value();
                // Measure actual Unicode characters, not UTF-8 bytes (prevents CJK penalty)
                module_doc_len += val.trim().chars().count();
                module_doc_lines.push(val);
            }
        }

        if module_doc_len < config.min_module_doc_chars {
            let under = config.min_module_doc_chars - module_doc_len;
            return Err(format!(
                "Rule: AGENTS.md Rule 1 (File-Level Living Wiki Header)\nLocation: {:?}:1\nLimit: Minimum {} characters in module doc (//!)\nActual: {} characters (-{} under limit)\nAction: Every production source file must serve as a living Wiki entry detailing its purpose, responsibilities, and architecture.",
                path, config.min_module_doc_chars, module_doc_len, under
            ));
        }

        if config.enforce_doc_schema {
            validate_doc_schema(path, &module_doc_lines, &syn_file)?;
        }
    }

    // 4. Pass 3: AST Inspection for Function Physical Size (Rule 4 & Rule 4b)
    if !is_test_file {
        let mut visitor = FunctionSizeChecker {
            path,
            source_lines: &lines,
            is_test_file,
            max_function_chars: config.max_function_chars,
            max_ui_component_chars: config.max_ui_component_chars,
            errors: Vec::new(),
        };
        visitor.visit_file(&syn_file);

        if let Some(err) = visitor.errors.into_iter().next() {
            return Err(err);
        }
    }

    Ok(())
}

/// Checks whether a path corresponds strictly to a test or benchmark file.
/// Avoids false positives on production files such as `src/contest.rs` or `src/attestation.rs`.
pub fn is_path_test_file(path: &Path) -> bool {
    let in_test_dir = path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s == "tests" || s == "benches"
    });

    let is_test_filename = path.file_name().is_some_and(|f| {
        let name = f.to_string_lossy();
        name == "test.rs" || name == "tests.rs" || name.ends_with("_test.rs") || name.ends_with("_tests.rs")
    });

    in_test_dir || is_test_filename
}

/// Collects line ranges for items annotated with `#[cfg(test)]` or `#[test]`.
struct TestScopeCollector {
    test_line_ranges: Vec<RangeInclusive<usize>>,
}

impl<'ast> Visit<'ast> for TestScopeCollector {
    fn visit_item(&mut self, node: &'ast syn::Item) {
        let is_test = match node {
            syn::Item::Mod(m) => is_cfg_test(&m.attrs),
            syn::Item::Fn(f) => is_cfg_test(&f.attrs),
            syn::Item::Struct(s) => is_cfg_test(&s.attrs),
            syn::Item::Enum(e) => is_cfg_test(&e.attrs),
            syn::Item::Const(c) => is_cfg_test(&c.attrs),
            syn::Item::Static(s) => is_cfg_test(&s.attrs),
            syn::Item::Trait(t) => is_cfg_test(&t.attrs),
            syn::Item::Impl(i) => is_cfg_test(&i.attrs),
            _ => false,
        };

        if is_test {
            let span = node.span();
            let start_line = span.start().line.max(1);
            let end_line = span.end().line;
            if start_line <= end_line {
                self.test_line_ranges.push(start_line..=end_line);
            }
        }

        syn::visit::visit_item(self, node);
    }
}

/// Checks if attributes indicate a test configuration, avoiding token spacing artifacts
/// and accurately differentiating `test` predicates from `not(test)` or feature flags.
fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr
            .path()
            .segments
            .last()
            .is_some_and(|seg| seg.ident == "test" || seg.ident == "rstest")
        {
            return true;
        }
        if attr.path().is_ident("cfg")
            && let syn::Meta::List(list) = &attr.meta
        {
            let compact = list.tokens.to_string().replace(' ', "");
            // Direct #[cfg(test)]
            if compact == "test"
                || compact.starts_with("test,")
                || compact.ends_with(",test")
                || compact.contains(",test,")
            {
                return true;
            }
            // Explicit not(test) is never test code
            if compact.contains("not(test")
                || compact.contains("not(all(test")
                || compact.contains("not(any(test")
            {
                return false;
            }
            // Strip string literals (e.g. feature="test-utils") to avoid false positives on features
            let mut cleaned = String::new();
            let mut in_str = false;
            for c in compact.chars() {
                if c == '"' {
                    in_str = !in_str;
                } else if !in_str {
                    cleaned.push(c);
                }
            }
            if cleaned.starts_with("all(") || cleaned.starts_with("any(") {
                return cleaned
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|w| w == "test");
            }
        }
        false
    })
}

/// Inspects functions across modules, impl blocks, and traits for physical character limits
/// and disallowed complexity escape hatches (`#[allow(clippy::too_many_lines)]`, `#[allow(clippy::too_many_arguments)]`).
struct FunctionSizeChecker<'a> {
    path: &'a Path,
    source_lines: &'a [&'a str],
    is_test_file: bool,
    max_function_chars: usize,
    max_ui_component_chars: usize,
    errors: Vec<String>,
}

fn find_disallowed_escape_hatch(attrs: &[syn::Attribute]) -> Option<(usize, &'static str)> {
    for attr in attrs {
        if attr.path().is_ident("allow")
            && let syn::Meta::List(list) = &attr.meta
        {
            let tokens = list.tokens.to_string().replace(' ', "");
            if tokens.contains("too_many_arguments") {
                return Some((attr.span().start().line.max(1), "clippy::too_many_arguments"));
            }
            if tokens.contains("too_many_lines") {
                return Some((attr.span().start().line.max(1), "clippy::too_many_lines"));
            }
        }
    }
    None
}

impl<'a> FunctionSizeChecker<'a> {
    fn check_escape_hatches(&mut self, item_label: &str, attrs: &[syn::Attribute]) {
        if self.is_test_file {
            return;
        }
        if let Some((line, lint_name)) = find_disallowed_escape_hatch(attrs) {
            self.errors.push(format!(
                "Rule: AGENTS.md Rule 4 (Disallowed Escape Hatch Attribute `{}`)\nLocation: {:?}:{}\nItem: {}\nAction: Do not bypass structural complexity limits with `#[allow({})]`. Group related arguments into a cohesive context struct or decompose the function.",
                lint_name, self.path, line, item_label, lint_name
            ));
        }
    }

    fn check_fn_size(
        &mut self,
        fn_name: &str,
        attrs: &[syn::Attribute],
        sig_span: proc_macro2::Span,
        body_span: proc_macro2::Span,
    ) {
        if self.is_test_file {
            return;
        }

        self.check_escape_hatches(&format!("Function `{}`", fn_name), attrs);

        let start = attrs
            .first()
            .map(|a| a.span().start())
            .unwrap_or_else(|| sig_span.start());
        let end = body_span.end();

        if start.line == 0 || end.line == 0 || start.line > self.source_lines.len() {
            return;
        }

        let fn_chars = calculate_span_chars(self.source_lines, start, end);
        let has_component_attr = attrs.iter().any(|attr| {
            attr.path()
                .segments
                .last()
                .is_some_and(|seg| seg.ident == "component")
        });
        let end_line_idx = end.line.min(self.source_lines.len());
        let has_ui_macro = self.source_lines[(start.line - 1)..end_line_idx]
            .iter()
            .any(|line| {
                line.contains("rsx!") || line.contains("html!") || line.contains("view!")
            });
        let is_ui_component = has_component_attr || has_ui_macro;
        let limit = if is_ui_component {
            self.max_ui_component_chars
        } else {
            self.max_function_chars
        };

        if fn_chars > limit {
            let over = fn_chars - limit;
            let rule_label = if is_ui_component {
                "Rule 4b (Declarative UI Component Physical Size Limit)"
            } else {
                "Rule 4 (Function Physical Size Limit)"
            };
            self.errors.push(format!(
                "Rule: AGENTS.md {}\nLocation: {:?}:{}\nItem: Function `{}`\nLimit: Maximum {} characters\nActual: {} characters (+{} over limit)\nAction: Refactor into smaller helper functions or sub-components. To adjust ceilings, edit `.agent-lint.toml`.",
                rule_label, self.path, start.line, fn_name, limit, fn_chars, over
            ));
        }
    }
}

impl<'ast, 'a> Visit<'ast> for FunctionSizeChecker<'a> {
    fn visit_file(&mut self, node: &'ast syn::File) {
        self.check_escape_hatches("File", &node.attrs);
        syn::visit::visit_file(self, node);
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.check_escape_hatches(&format!("Module `{}`", node.ident), &node.attrs);
        syn::visit::visit_item_mod(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let fn_name = node.sig.ident.to_string();
        self.check_fn_size(&fn_name, &node.attrs, node.sig.span(), node.block.span());
        syn::visit::visit_item_fn(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        self.check_escape_hatches("Impl Block", &node.attrs);
        syn::visit::visit_item_impl(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let fn_name = node.sig.ident.to_string();
        self.check_fn_size(&fn_name, &node.attrs, node.sig.span(), node.block.span());
        syn::visit::visit_impl_item_fn(self, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        let fn_name = node.sig.ident.to_string();
        if let Some(block) = &node.default {
            self.check_fn_size(&fn_name, &node.attrs, node.sig.span(), block.span());
        } else {
            self.check_escape_hatches(&format!("Trait Method `{}`", fn_name), &node.attrs);
        }
        syn::visit::visit_trait_item_fn(self, node);
    }
}

/// Checks if a comment string contains non-English CJK/Hangul scripts.
fn contains_non_english_script(text: &str) -> bool {
    text.chars().any(|c| {
        ('\u{1100}'..='\u{11FF}').contains(&c) // Hangul Jamo
            || ('\u{3040}'..='\u{30FF}').contains(&c) // Hiragana & Katakana
            || ('\u{3130}'..='\u{318F}').contains(&c) // Hangul Compatibility Jamo
            || ('\u{AC00}'..='\u{D7AF}').contains(&c) // Hangul Syllables
            || ('\u{4E00}'..='\u{9FFF}').contains(&c) // CJK Unified Ideographs
    })
}

/// Scans source lines and calculates code vs doc character counts, while detecting non-English comments.
fn scan_code_and_comments(
    lines: &[&str],
    is_test_file: bool,
    test_ranges: &[RangeInclusive<usize>],
) -> (usize, usize, usize, Option<usize>) {
    let mut prod_logical_code_chars = 0;
    let mut doc_chars = 0;
    let mut inline_test_chars = 0;
    let mut first_non_english_comment_line = None;
    let mut block_comment_depth = 0;
    let mut in_raw_string = false;
    let mut in_normal_string = false;

    for (line_idx, line) in lines.iter().enumerate() {
        let line_num = line_idx + 1; // 1-indexed
        let trimmed = line.trim();
        let line_char_count = line.chars().count() + 1;

        if trimmed.is_empty() {
            continue;
        }

        let is_in_test_scope = is_test_file || test_ranges.iter().any(|r| r.contains(&line_num));

        if block_comment_depth > 0 {
            if first_non_english_comment_line.is_none() && contains_non_english_script(trimmed) {
                first_non_english_comment_line = Some(line_num);
            }
            if !is_in_test_scope {
                doc_chars += line_char_count;
            } else if !is_test_file {
                inline_test_chars += line_char_count;
            }
            let opens = trimmed.matches("/*").count();
            let closes = trimmed.matches("*/").count();
            block_comment_depth = (block_comment_depth + opens).saturating_sub(closes);
            continue;
        }

        // Check raw string transitions: r#" or r##"
        if in_raw_string {
            if trimmed.contains("\"#") || trimmed.contains("\"##") {
                in_raw_string = false;
            }
            if !is_in_test_scope {
                prod_logical_code_chars += line_char_count;
            } else if !is_test_file {
                inline_test_chars += line_char_count;
            }
            continue;
        }

        // If line starts while inside a multiline normal string, process string content
        if in_normal_string {
            let (slash_pos, new_in_string) = find_inline_comment(trimmed, in_normal_string);
            in_normal_string = new_in_string;
            if let Some(pos) = slash_pos {
                let code_part = trimmed[..pos].trim();
                let comment_part = &trimmed[pos..];
                if first_non_english_comment_line.is_none()
                    && contains_non_english_script(comment_part)
                {
                    first_non_english_comment_line = Some(line_num);
                }
                if !is_in_test_scope {
                    prod_logical_code_chars += code_part.chars().count() + 1;
                    doc_chars += comment_part.chars().count();
                } else if !is_test_file {
                    inline_test_chars += line_char_count;
                }
            } else if !is_in_test_scope {
                prod_logical_code_chars += line_char_count;
            } else if !is_test_file {
                inline_test_chars += line_char_count;
            }
            continue;
        }

        // Line comments MUST be evaluated BEFORE raw string markers (when outside strings)
        if trimmed.starts_with("//") {
            if first_non_english_comment_line.is_none() && contains_non_english_script(trimmed) {
                first_non_english_comment_line = Some(line_num);
            }
            if !is_in_test_scope {
                doc_chars += line_char_count;
            } else if !is_test_file {
                inline_test_chars += line_char_count;
            }
            continue;
        }

        // Check standard block comment start (supporting nested block comments)
        if trimmed.starts_with("/*") {
            if first_non_english_comment_line.is_none() && contains_non_english_script(trimmed) {
                first_non_english_comment_line = Some(line_num);
            }
            let opens = trimmed.matches("/*").count();
            let closes = trimmed.matches("*/").count();
            if opens > closes {
                block_comment_depth = opens - closes;
            }
            if !is_in_test_scope {
                doc_chars += line_char_count;
            } else if !is_test_file {
                inline_test_chars += line_char_count;
            }
            continue;
        }

        // Check raw string start in actual code
        if trimmed.contains("r#\"") || trimmed.contains("r##\"") {
            let open_pos = match (trimmed.find("r##\""), trimmed.find("r#\"")) {
                (Some(p2), Some(p1)) => Some(p2.min(p1)),
                (Some(p2), None) => Some(p2),
                (None, Some(p1)) => Some(p1),
                (None, None) => None,
            };
            let close_pos = match (trimmed.rfind("\"##"), trimmed.rfind("\"#")) {
                (Some(p2), Some(p1)) => Some(p2.max(p1)),
                (Some(p2), None) => Some(p2),
                (None, Some(p1)) => Some(p1),
                (None, None) => None,
            };
            let is_single_line = match (open_pos, close_pos) {
                (Some(o), Some(c)) => c > o,
                _ => false,
            };
            if !is_single_line {
                in_raw_string = true;
            }
            if !is_in_test_scope {
                prod_logical_code_chars += line_char_count;
            } else if !is_test_file {
                inline_test_chars += line_char_count;
            }
            continue;
        }

        // Normal line: if line contains inline comment `//` outside of quotes, split counts
        let (slash_pos, new_in_string) = find_inline_comment(trimmed, false);
        in_normal_string = new_in_string;
        if let Some(slash_pos) = slash_pos {
            let code_part = trimmed[..slash_pos].trim();
            let comment_part = &trimmed[slash_pos..];
            if first_non_english_comment_line.is_none() && contains_non_english_script(comment_part)
            {
                first_non_english_comment_line = Some(line_num);
            }
            if !is_in_test_scope {
                prod_logical_code_chars += code_part.chars().count() + 1;
                doc_chars += comment_part.chars().count();
            } else if !is_test_file {
                inline_test_chars += line_char_count;
            }
        } else if !is_in_test_scope {
            prod_logical_code_chars += line_char_count;
        } else if !is_test_file {
            inline_test_chars += line_char_count;
        }
    }

    (
        prod_logical_code_chars,
        doc_chars,
        inline_test_chars,
        first_non_english_comment_line,
    )
}

/// Helper to parse character literal byte length starting with `'`.
/// Returns Some(len) if it is a valid char literal (e.g. `'c'`, `'\n'`, `'\\''`).
/// Returns None if it is a lifetime (e.g. `'a`, `'static`, `'_`).
fn parse_char_literal_len(rem: &str) -> Option<usize> {
    if !rem.starts_with('\'') {
        return None;
    }
    let mut chars = rem[1..].char_indices();
    let (_, first) = chars.next()?;
    if first == '\\' {
        let (_, esc) = chars.next()?;
        if esc == 'x' {
            chars.next()?; // hex 1
            chars.next()?; // hex 2
        } else if esc == 'u' {
            let (_, open) = chars.next()?;
            if open != '{' {
                return None;
            }
            loop {
                let (_, c) = chars.next()?;
                if c == '}' {
                    break;
                }
            }
        }
        // Now expect the closing quote
        let (idx, close) = chars.next()?;
        if close == '\'' {
            return Some(1 + idx + close.len_utf8());
        }
        None
    } else if first != '\'' {
        let (idx, second) = chars.next()?;
        if second == '\'' {
            Some(1 + idx + second.len_utf8())
        } else {
            None
        }
    } else {
        None
    }
}

/// Finds the start position of an inline `//` comment that is not inside quotes,
/// and tracks multiline string state across lines.
fn find_inline_comment(line: &str, mut in_string: bool) -> (Option<usize>, bool) {
    let mut chars = line.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if in_string {
            if ch == '\\' {
                chars.next(); // Skip escaped character
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
        } else if ch == '\'' {
            if let Some(len) = parse_char_literal_len(&line[idx..]) {
                let target_idx = idx + len;
                while let Some(&(next_idx, _)) = chars.peek() {
                    if next_idx < target_idx {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
        } else if ch == '/'
            && let Some(&(_, next_ch)) = chars.peek()
            && next_ch == '/'
        {
            return (Some(idx), in_string);
        }
    }
    (None, in_string)
}

fn calculate_span_chars(
    lines: &[&str],
    start: proc_macro2::LineColumn,
    end: proc_macro2::LineColumn,
) -> usize {
    let start_line = start.line.max(1);
    let end_line = end.line.min(lines.len());

    if start_line > end_line {
        return 0;
    }

    let mut total_chars = 0;
    for (line_idx, line) in lines.iter().enumerate().take(end_line).skip(start_line - 1) {
        let is_first = line_idx == start_line - 1;
        let is_last = line_idx == end_line - 1;

        let start_col = if is_first { start.column } else { 0 };
        let end_col = if is_last { Some(end.column) } else { None };

        total_chars += char_range(line, start_col, end_col);
        if !is_last {
            total_chars += 1; // newline character
        }
    }

    total_chars
}

fn char_range(line: &str, start_byte: usize, end_byte: Option<usize>) -> usize {
    let start_idx = line
        .char_indices()
        .map(|(idx, _)| idx)
        .find(|&idx| idx >= start_byte)
        .unwrap_or(line.len());
    let end_idx = match end_byte {
        Some(eb) => line
            .char_indices()
            .map(|(idx, _)| idx)
            .find(|&idx| idx >= eb)
            .unwrap_or(line.len()),
        None => line.len(),
    };
    if start_idx <= end_idx {
        line[start_idx..end_idx].chars().count()
    } else {
        0
    }
}

/// Validates that module-level doc comments (`//!`) adhere to the Living Wiki Schema (Rule 1).
pub fn validate_doc_schema(
    path: &Path,
    doc_lines: &[String],
    syn_file: &syn::File,
) -> Result<(), String> {
    let mut overview_lines = Vec::new();
    let mut tags_lines = Vec::new();
    let mut submodules_lines = Vec::new();

    let mut has_overview_header = false;
    let mut has_tags_header = false;
    let mut has_submodules_header = false;

    enum Section {
        None,
        Overview,
        Tags,
        Submodules,
        Other,
    }

    let mut current_section = Section::None;

    for line in doc_lines {
        let t = line.trim();
        if t.starts_with("## ") || t.starts_with("# ") {
            if t.starts_with("## Overview") || t.starts_with("# Overview") {
                has_overview_header = true;
                current_section = Section::Overview;
            } else if t.starts_with("## Search Tags")
                || t.starts_with("# Search Tags")
                || t.starts_with("## Tags")
                || t.starts_with("# Tags")
            {
                has_tags_header = true;
                current_section = Section::Tags;
            } else if t.starts_with("## Submodules")
                || t.starts_with("# Submodules")
                || t.starts_with("## Modules")
                || t.starts_with("# Modules")
                || t.starts_with("## Module Components")
            {
                has_submodules_header = true;
                current_section = Section::Submodules;
            } else {
                current_section = Section::Other;
            }
            continue;
        }

        match current_section {
            Section::Overview => overview_lines.push(t),
            Section::Tags => tags_lines.push(t),
            Section::Submodules => submodules_lines.push(t),
            Section::None | Section::Other => {}
        }
    }

    // 1. Enforce ## Overview with non-empty content
    let has_overview_content = overview_lines.iter().any(|l| !l.is_empty());
    if !has_overview_header || !has_overview_content {
        return Err(format!(
            "Rule: AGENTS.md Rule 1 (Living Wiki Schema: Missing Overview)\nLocation: {:?}:1\nRequirement: Module doc (`//!`) must contain a `## Overview` section describing its single responsibility.\nAction: Add `## Overview` followed by a concise 1-2 sentence description at the top of the file.",
            path
        ));
    }

    // 2. Enforce ## Search Tags with at least one `#tag` strictly inside the Search Tags section
    let has_tag_keyword = tags_lines.iter().any(|l| {
        l.split_whitespace().any(|word| {
            let clean = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '#' && c != '-' && c != '_');
            clean.starts_with('#') && !clean.starts_with("##") && clean.len() > 1
        })
    });

    if !has_tags_header || !has_tag_keyword {
        return Err(format!(
            "Rule: AGENTS.md Rule 1 (Living Wiki Schema: Missing Search Tags)\nLocation: {:?}:1\nRequirement: Module doc (`//!`) must contain a `## Search Tags` section with at least one hashtag (e.g. `#keyword`).\nAction: Add `## Search Tags` followed by deterministic hashtags (e.g. `#tag1, #tag2`) for fast grep indexing.",
            path
        ));
    }

    // 3. Enforce ## Submodules catalog if external submodules are declared
    let mut external_submodules = Vec::new();
    for item in &syn_file.items {
        if let syn::Item::Mod(m) = item
            && !is_cfg_test(&m.attrs)
            && m.content.is_none()
        {
            external_submodules.push(m.ident.to_string());
        }
    }

    if !external_submodules.is_empty() {
        if !has_submodules_header {
            return Err(format!(
                "Rule: AGENTS.md Rule 1 (Living Wiki Schema: Missing Submodules Catalog)\nLocation: {:?}:1\nRequirement: Module declares child submodules via `mod <name>;` but is missing a `## Submodules` catalog.\nAction: Add a `## Submodules` section with 1-line single-responsibility summaries for each child submodule.",
                path
            ));
        }

        let submodules_text = submodules_lines.join("\n");
        let parent_dir = path.parent();
        for submod in &external_submodules {
            let bracketed1 = format!("[`{}`]", submod);
            let bracketed2 = format!("[{}]", submod);
            let list_entry1 = format!("- {}:", submod);
            let list_entry2 = format!("- `{}`:", submod);
            if !submodules_text.contains(&bracketed1)
                && !submodules_text.contains(&bracketed2)
                && !submodules_text.contains(&list_entry1)
                && !submodules_text.contains(&list_entry2)
            {
                return Err(format!(
                    "Rule: AGENTS.md Rule 1 (Living Wiki Schema: Submodule Catalog Drift)\nLocation: {:?}:1\nRequirement: Submodule `{}` is declared via `mod {};` but not listed in the `## Submodules` catalog.\nAction: Add `- [`{}`] (`{}.rs` or `{}/mod.rs`): 1-line single-responsibility summary` under `## Submodules`.",
                    path, submod, submod, submod, submod, submod
                ));
            }

            // Enforce explicit physical path disambiguation (`submod.rs` vs `submod/mod.rs`) when files exist on disk
            if let Some(dir) = parent_dir {
                let dir_mod_path = dir.join(submod).join("mod.rs");
                let leaf_rs_path = dir.join(format!("{}.rs", submod));
                let expected_dir_token = format!("{}/mod.rs", submod);
                let expected_leaf_token = format!("{}.rs", submod);

                if dir_mod_path.is_file() && !submodules_text.contains(&expected_dir_token) {
                    return Err(format!(
                        "Rule: AGENTS.md Rule 1 (Living Wiki Schema: Ambiguous Submodule Path)\nLocation: {:?}:1\nRequirement: Submodule `{}` resolves to directory module `{}` on disk, so its `## Submodules` catalog entry must explicitly include `({})` to prevent agent 404 navigation errors.\nAction: Update the catalog entry to `- [`{}`] (`{}`): 1-line single-responsibility summary`.",
                        path, submod, expected_dir_token, expected_dir_token, submod, expected_dir_token
                    ));
                } else if leaf_rs_path.is_file() && !submodules_text.contains(&expected_leaf_token) {
                    return Err(format!(
                        "Rule: AGENTS.md Rule 1 (Living Wiki Schema: Ambiguous Submodule Path)\nLocation: {:?}:1\nRequirement: Submodule `{}` resolves to leaf file `{}` on disk, so its `## Submodules` catalog entry must explicitly include `({})` to prevent agent 404 navigation errors.\nAction: Update the catalog entry to `- [`{}`] (`{}`): 1-line single-responsibility summary`.",
                        path, submod, expected_leaf_token, expected_leaf_token, submod, expected_leaf_token
                    ));
                }
            }
        }
    }

    Ok(())
}


