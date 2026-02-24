//! Detects unbounded caches in the Lighthouse codebase.
//!
//! Two-pass analysis across scanned directories:
//! 1. Collect struct fields with collection types (HashMap/BTreeMap/HashSet/BTreeSet)
//! 2. Detect pruning methods in impl blocks (by name and AST analysis)
//!
//! Fields without any detected pruning cause CI failure unless allowlisted.

use glob::glob;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::{env, fs, process};
use syn::visit::Visit;
use syn::{Expr, ExprMethodCall, File, ImplItem, ItemImpl, ItemStruct, Member, Type};

/// Names of types that represent potentially unbounded collections.
const COLLECTION_TYPES: &[&str] = &["HashMap", "BTreeMap", "HashSet", "BTreeSet"];

/// Method names on collections that indicate pruning/bounding behavior.
const COLLECTION_PRUNING_METHODS: &[&str] = &[
    "remove",
    "retain",
    "clear",
    "drain",
    "pop",
    "pop_first",
    "pop_last",
    "split_off",
];

/// Substrings in method names that indicate the method performs pruning/bounding.
const PRUNING_NAME_PATTERNS: &[&str] = &[
    "prune", "shrink", "evict", "truncat", "gc", "purge", "cleanup", "expire",
];

/// Struct name suffixes that indicate non-cache types.
const NON_CACHE_SUFFIXES: &[&str] = &[
    "Config", "Request", "Response", "Params", "Args", "Options", "Event", "Message",
];

/// Directories to scan for cache-bearing structs (long-lived services).
const SCAN_DIRS: &[&str] = &["beacon_node", "validator_client", "slasher"];

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// A struct field that uses a collection type.
#[derive(Debug, Clone)]
struct FieldRecord {
    file_path: String,
    struct_name: String,
    field_name: String,
    collection_type: &'static str,
}

impl FieldRecord {
    fn entry_key(&self, repo_root: &Path) -> String {
        let relative_path = make_relative(&self.file_path, repo_root);
        format!("{}::{}::{}", relative_path, self.struct_name, self.field_name)
    }
}

/// A method that performs pruning on collection fields.
#[derive(Debug, Clone)]
struct PruningMethod {
    #[allow(dead_code)]
    method_name: String,
    /// Field names this method prunes. Empty means "all fields" (name-based detection).
    fields_pruned: HashSet<String>,
}

impl PruningMethod {
    fn covers_field(&self, field_name: &str) -> bool {
        self.fields_pruned.is_empty() || self.fields_pruned.contains(field_name)
    }
}

/// An entry in the allowlist file.
#[derive(Debug, Deserialize)]
struct AllowedEntry {
    entry: String,
    #[allow(dead_code)]
    reason: String,
}

/// Top-level allowlist configuration.
#[derive(Debug, Deserialize)]
struct Allowlist {
    #[serde(default)]
    allowed: Vec<AllowedEntry>,
}

// ---------------------------------------------------------------------------
// Global analysis state
// ---------------------------------------------------------------------------

/// Accumulated results from passes 1 and 2.
#[derive(Debug, Default)]
struct AnalysisState {
    /// Pass 1: struct_name -> list of collection fields.
    struct_fields: HashMap<String, Vec<FieldRecord>>,
    /// Pass 1: struct_name -> set of collection field names (for Pass 2 AST matching).
    collection_field_names: HashMap<String, HashSet<String>>,
    /// Pass 2: struct_name -> pruning methods.
    pruning_methods: HashMap<String, Vec<PruningMethod>>,
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn make_relative(path: &str, repo_root: &Path) -> String {
    let path = Path::new(path);
    match path.strip_prefix(repo_root) {
        Ok(relative) => relative.to_string_lossy().to_string(),
        Err(_) => path.to_string_lossy().to_string(),
    }
}

/// Check if a type is a collection type, returning the type name if so.
fn collection_type_name(ty: &Type) -> Option<&'static str> {
    if let Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            let name = segment.ident.to_string();
            for &coll in COLLECTION_TYPES {
                if name == coll {
                    return Some(coll);
                }
            }
        }
    }
    None
}

/// Check if any attribute is `#[derive(..., Deserialize, ...)]`.
fn has_derive_deserialize(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("derive") {
            if let Ok(nested) = attr.parse_args_with(
                syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
            ) {
                return nested
                    .iter()
                    .any(|path| path.segments.last().is_some_and(|s| s.ident == "Deserialize"));
            }
        }
        false
    })
}

/// Check if struct name ends with a non-cache suffix.
fn has_non_cache_suffix(name: &str) -> bool {
    NON_CACHE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// Check if all fields are bare generic type parameters (wrapper/container type).
fn all_fields_are_generic_params(node: &ItemStruct) -> bool {
    let type_params: HashSet<String> = node
        .generics
        .type_params()
        .map(|tp| tp.ident.to_string())
        .collect();
    if type_params.is_empty() || node.fields.is_empty() {
        return false;
    }
    node.fields.iter().all(|field| {
        if let Type::Path(tp) = &field.ty {
            tp.path.segments.len() == 1
                && tp.path.leading_colon.is_none()
                && type_params.contains(&tp.path.segments[0].ident.to_string())
        } else {
            false
        }
    })
}

/// Extract the field name from a `self.<field>` expression, peeling through
/// references and parentheses.
fn extract_self_field(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Field(expr_field) => {
            if is_self_expr(&expr_field.base) {
                if let Member::Named(ident) = &expr_field.member {
                    return Some(ident.to_string());
                }
            }
            None
        }
        Expr::Reference(expr_ref) => extract_self_field(&expr_ref.expr),
        Expr::Paren(expr_paren) => extract_self_field(&expr_paren.expr),
        _ => None,
    }
}

/// Check if an expression is `self`.
fn is_self_expr(expr: &Expr) -> bool {
    matches!(expr, Expr::Path(expr_path) if expr_path.path.is_ident("self"))
}

/// Check if a path should be skipped (test files, binaries).
fn should_skip_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    path_str.contains("/tests/")
        || path_str.contains("/test_utils")
        || path_str.contains("_test.rs")
        || path_str.contains("/src/bin/")
}

/// Get the struct name from an impl block's self_ty.
fn impl_struct_name(node: &ItemImpl) -> Option<String> {
    if let Type::Path(type_path) = &*node.self_ty {
        type_path.path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

/// Parse a Rust source file into a syn AST.
fn parse_rust_file(path: &Path) -> Option<File> {
    let content = fs::read_to_string(path).ok()?;
    syn::parse_file(&content).ok()
}

// ---------------------------------------------------------------------------
// Pass 1 Visitor: Collect struct fields with collection types
// ---------------------------------------------------------------------------

struct Pass1Visitor<'a> {
    file_path: String,
    state: &'a mut AnalysisState,
}

impl<'ast, 'a> Visit<'ast> for Pass1Visitor<'a> {
    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        let struct_name = node.ident.to_string();

        if has_derive_deserialize(&node.attrs) {
            return;
        }
        if has_non_cache_suffix(&struct_name) {
            return;
        }
        if all_fields_are_generic_params(node) {
            return;
        }

        for field in &node.fields {
            if let Some(coll_type) = collection_type_name(&field.ty) {
                let field_name = field
                    .ident
                    .as_ref()
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "unnamed".to_string());

                self.state
                    .struct_fields
                    .entry(struct_name.clone())
                    .or_default()
                    .push(FieldRecord {
                        file_path: self.file_path.clone(),
                        struct_name: struct_name.clone(),
                        field_name: field_name.clone(),
                        collection_type: coll_type,
                    });

                self.state
                    .collection_field_names
                    .entry(struct_name.clone())
                    .or_default()
                    .insert(field_name);
            }
        }

        syn::visit::visit_item_struct(self, node);
    }
}

// ---------------------------------------------------------------------------
// Pass 2 Visitor: Collect pruning methods in impl blocks
// ---------------------------------------------------------------------------

struct Pass2Visitor<'a> {
    state: &'a mut AnalysisState,
}

impl<'ast, 'a> Visit<'ast> for Pass2Visitor<'a> {
    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        if let Some(struct_name) = impl_struct_name(node) {
            if let Some(field_names) = self.state.collection_field_names.get(&struct_name).cloned()
            {
                for item in &node.items {
                    if let ImplItem::Fn(method) = item {
                        let method_name = method.sig.ident.to_string();
                        let method_lower = method_name.to_lowercase();

                        // Check by name
                        let is_pruning_name = PRUNING_NAME_PATTERNS
                            .iter()
                            .any(|pat| method_lower.contains(pat));

                        if is_pruning_name {
                            self.state
                                .pruning_methods
                                .entry(struct_name.clone())
                                .or_default()
                                .push(PruningMethod {
                                    method_name: method_name.clone(),
                                    fields_pruned: HashSet::new(), // empty = all fields
                                });
                            continue;
                        }

                        // Check by AST — walk method body
                        let mut body_visitor = MethodBodyPruningVisitor {
                            collection_fields: &field_names,
                            fields_pruned: HashSet::new(),
                        };
                        body_visitor.visit_block(&method.block);

                        if !body_visitor.fields_pruned.is_empty() {
                            self.state
                                .pruning_methods
                                .entry(struct_name.clone())
                                .or_default()
                                .push(PruningMethod {
                                    method_name: method_name.clone(),
                                    fields_pruned: body_visitor.fields_pruned,
                                });
                        }
                    }
                }
            }
        }

        syn::visit::visit_item_impl(self, node);
    }
}

// ---------------------------------------------------------------------------
// Pass 2 nested visitor: Detect pruning calls in method bodies
// ---------------------------------------------------------------------------

struct MethodBodyPruningVisitor<'a> {
    collection_fields: &'a HashSet<String>,
    fields_pruned: HashSet<String>,
}

impl<'ast, 'a> Visit<'ast> for MethodBodyPruningVisitor<'a> {
    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        let method_name = node.method.to_string();
        if COLLECTION_PRUNING_METHODS.contains(&method_name.as_str()) {
            if let Some(field_name) = extract_self_field(&node.receiver) {
                if self.collection_fields.contains(&field_name) {
                    self.fields_pruned.insert(field_name);
                }
            }
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

// ---------------------------------------------------------------------------
// File scanning
// ---------------------------------------------------------------------------

/// Scan files in scan_dirs for structs and pruning methods.
fn scan_structs_and_pruning(repo_root: &Path, state: &mut AnalysisState) {
    // Parse all files first so Pass 2 can see structs from any file.
    let mut parsed_files: Vec<(String, File)> = Vec::new();

    for dir in SCAN_DIRS {
        let pattern = format!("{}/**/*.rs", repo_root.join(dir).display());
        let paths: Vec<PathBuf> = glob(&pattern)
            .expect("Failed to read glob pattern")
            .filter_map(Result::ok)
            .collect();

        for path in paths {
            if should_skip_path(&path) {
                continue;
            }
            if let Some(syntax) = parse_rust_file(&path) {
                parsed_files.push((path.to_string_lossy().to_string(), syntax));
            }
        }
    }

    // Pass 1: Collect struct fields from all files.
    for (file_path, syntax) in &parsed_files {
        let mut visitor = Pass1Visitor {
            file_path: file_path.clone(),
            state,
        };
        visitor.visit_file(syntax);
    }

    // Pass 2: Collect pruning methods (now all structs are known).
    for (_file_path, syntax) in &parsed_files {
        let mut visitor = Pass2Visitor { state };
        visitor.visit_file(syntax);
    }
}

/// Find fields with no detected pruning.
fn find_unbounded_fields(state: &AnalysisState) -> Vec<FieldRecord> {
    let mut violations = Vec::new();

    for (struct_name, fields) in &state.struct_fields {
        let pruning = state.pruning_methods.get(struct_name);

        for field in fields {
            let has_covering_method = pruning
                .map(|methods| methods.iter().any(|m| m.covers_field(&field.field_name)))
                .unwrap_or(false);

            if !has_covering_method {
                violations.push(field.clone());
            }
        }
    }

    // Sort for deterministic output.
    violations.sort_by(|a, b| {
        let key = |f: &FieldRecord| {
            format!("{}::{}::{}", f.file_path, f.struct_name, f.field_name)
        };
        key(a).cmp(&key(b))
    });

    violations
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let repo_root = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        env::current_dir().expect("Failed to get current directory")
    };

    let repo_root = fs::canonicalize(&repo_root).unwrap_or_else(|_| {
        eprintln!(
            "Warning: could not canonicalize repo root {:?}, using as-is",
            repo_root
        );
        repo_root
    });

    let allowlist_path = repo_root.join(".github/custom/unbounded-cache-allowlist.toml");

    let allowlist: Allowlist = if allowlist_path.exists() {
        let content = fs::read_to_string(&allowlist_path).expect("Failed to read allowlist file");
        toml::from_str(&content).expect("Failed to parse allowlist TOML")
    } else {
        Allowlist {
            allowed: Vec::new(),
        }
    };

    let allowed_set: HashSet<String> = allowlist.allowed.iter().map(|a| a.entry.clone()).collect();

    // Scan for structs and pruning methods.
    let mut state = AnalysisState::default();
    scan_structs_and_pruning(&repo_root, &mut state);

    // Find fields without pruning.
    let violations = find_unbounded_fields(&state);

    // Filter against allowlist.
    let unallowed: Vec<&FieldRecord> = violations
        .iter()
        .filter(|f| !allowed_set.contains(&f.entry_key(&repo_root)))
        .collect();

    if unallowed.is_empty() {
        println!("No new unbounded caches detected.");
        process::exit(0);
    } else {
        eprintln!(
            "Found {} potentially unbounded cache(s) without pruning logic:",
            unallowed.len()
        );
        eprintln!();
        for field in &unallowed {
            let entry_key = field.entry_key(&repo_root);
            eprintln!("  {} ({})", entry_key, field.collection_type);
        }
        eprintln!();
        eprintln!("To suppress, add entries to .github/custom/unbounded-cache-allowlist.toml:");
        eprintln!();
        for field in &unallowed {
            let entry_key = field.entry_key(&repo_root);
            eprintln!("[[allowed]]");
            eprintln!("entry = \"{}\"", entry_key);
            eprintln!("reason = \"\"");
            eprintln!();
        }
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(code: &str) -> File {
        syn::parse_file(code).expect("Failed to parse test code")
    }

    // -----------------------------------------------------------------------
    // Pass 1 tests: struct field collection
    // -----------------------------------------------------------------------

    mod pass1 {
        use super::*;

        fn collect_fields(code: &str) -> AnalysisState {
            let syntax = parse(code);
            let mut state = AnalysisState::default();
            let mut visitor = Pass1Visitor {
                file_path: "test.rs".to_string(),
                state: &mut state,
            };
            visitor.visit_file(&syntax);
            state
        }

        #[test]
        fn detects_hashmap_field() {
            let state = collect_fields(
                "struct MyCache { entries: HashMap<String, u64> }",
            );
            let fields = &state.struct_fields["MyCache"];
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].field_name, "entries");
            assert_eq!(fields[0].collection_type, "HashMap");
        }

        #[test]
        fn detects_btreemap_field() {
            let state = collect_fields(
                "struct MyCache { entries: BTreeMap<String, u64> }",
            );
            let fields = &state.struct_fields["MyCache"];
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].collection_type, "BTreeMap");
        }

        #[test]
        fn detects_hashset_field() {
            let state = collect_fields(
                "struct MyCache { items: HashSet<String> }",
            );
            let fields = &state.struct_fields["MyCache"];
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].collection_type, "HashSet");
        }

        #[test]
        fn detects_btreeset_field() {
            let state = collect_fields(
                "struct MyCache { items: BTreeSet<u64> }",
            );
            let fields = &state.struct_fields["MyCache"];
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].collection_type, "BTreeSet");
        }

        #[test]
        fn detects_multiple_fields() {
            let state = collect_fields(
                "struct MyCache { a: HashMap<String, u64>, b: HashSet<String>, c: u64 }",
            );
            let fields = &state.struct_fields["MyCache"];
            assert_eq!(fields.len(), 2);
        }

        #[test]
        fn skips_derive_deserialize() {
            let state = collect_fields(
                "#[derive(Deserialize)] struct ApiBody { items: HashMap<String, u64> }",
            );
            assert!(state.struct_fields.is_empty());
        }

        #[test]
        fn skips_derive_serde_deserialize() {
            let state = collect_fields(
                "#[derive(serde::Deserialize)] struct ApiBody { items: HashMap<String, u64> }",
            );
            assert!(state.struct_fields.is_empty());
        }

        #[test]
        fn skips_config_suffix() {
            let state = collect_fields(
                "struct NetworkConfig { peers: HashMap<String, u64> }",
            );
            assert!(state.struct_fields.is_empty());
        }

        #[test]
        fn skips_request_suffix() {
            let state = collect_fields(
                "struct GetBlocksRequest { data: HashMap<String, u64> }",
            );
            assert!(state.struct_fields.is_empty());
        }

        #[test]
        fn skips_response_suffix() {
            let state = collect_fields(
                "struct GetBlocksResponse { data: HashMap<String, u64> }",
            );
            assert!(state.struct_fields.is_empty());
        }

        #[test]
        fn skips_all_suffixes() {
            for suffix in NON_CACHE_SUFFIXES {
                let code = format!("struct My{suffix} {{ items: HashMap<String, u64> }}");
                let state = collect_fields(&code);
                assert!(
                    state.struct_fields.is_empty(),
                    "Should skip struct ending with {suffix}"
                );
            }
        }

        #[test]
        fn skips_fully_generic_struct() {
            let state = collect_fields(
                "struct Wrapper<T, U> { a: T, b: U }",
            );
            assert!(state.struct_fields.is_empty());
        }

        #[test]
        fn does_not_skip_mixed_generic_struct() {
            let state = collect_fields(
                "struct MyCache<T> { data: HashMap<String, T>, count: usize }",
            );
            assert_eq!(state.struct_fields["MyCache"].len(), 1);
        }

        #[test]
        fn ignores_non_collection_fields() {
            let state = collect_fields(
                "struct MyStruct { name: String, count: u64 }",
            );
            assert!(state.struct_fields.is_empty());
        }

        #[test]
        fn records_file_path() {
            let state = collect_fields(
                "struct MyCache { items: HashMap<String, u64> }",
            );
            assert_eq!(state.struct_fields["MyCache"][0].file_path, "test.rs");
        }

        #[test]
        fn populates_collection_field_names() {
            let state = collect_fields(
                "struct MyCache { a: HashMap<String, u64>, b: HashSet<u64> }",
            );
            let names = &state.collection_field_names["MyCache"];
            assert!(names.contains("a"));
            assert!(names.contains("b"));
        }
    }

    // -----------------------------------------------------------------------
    // Pass 2 tests: pruning method detection
    // -----------------------------------------------------------------------

    mod pass2 {
        use super::*;

        fn collect_pruning(
            code: &str,
            struct_name: &str,
            fields: &[&str],
        ) -> Vec<PruningMethod> {
            let syntax = parse(code);
            let mut state = AnalysisState::default();
            let field_set: HashSet<String> =
                fields.iter().map(|f| f.to_string()).collect();
            state
                .collection_field_names
                .insert(struct_name.to_string(), field_set);
            let mut visitor = Pass2Visitor { state: &mut state };
            visitor.visit_file(&syntax);
            state
                .pruning_methods
                .remove(struct_name)
                .unwrap_or_default()
        }

        #[test]
        fn detects_prune_method_by_name() {
            let methods = collect_pruning(
                "impl MyCache { fn prune_old(&mut self) {} }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
            assert_eq!(methods[0].method_name, "prune_old");
            assert!(methods[0].fields_pruned.is_empty()); // covers all
        }

        #[test]
        fn detects_shrink_method_by_name() {
            let methods = collect_pruning(
                "impl MyCache { fn shrink_to_fit(&mut self) {} }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
            assert_eq!(methods[0].method_name, "shrink_to_fit");
        }

        #[test]
        fn detects_evict_method_by_name() {
            let methods = collect_pruning(
                "impl MyCache { fn evict_stale(&mut self) {} }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
        }

        #[test]
        fn detects_truncat_method_by_name() {
            let methods = collect_pruning(
                "impl MyCache { fn truncate_old(&mut self) {} }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
        }

        #[test]
        fn detects_gc_method_by_name() {
            let methods = collect_pruning(
                "impl MyCache { fn gc(&mut self) {} }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
        }

        #[test]
        fn detects_purge_method_by_name() {
            let methods = collect_pruning(
                "impl MyCache { fn purge_expired(&mut self) {} }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
        }

        #[test]
        fn detects_cleanup_method_by_name() {
            let methods = collect_pruning(
                "impl MyCache { fn cleanup(&mut self) {} }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
        }

        #[test]
        fn detects_expire_method_by_name() {
            let methods = collect_pruning(
                "impl MyCache { fn expire_entries(&mut self) {} }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
        }

        #[test]
        fn detects_self_field_remove() {
            let methods = collect_pruning(
                "impl MyCache { fn delete(&mut self, key: &str) { self.entries.remove(key); } }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
            assert_eq!(methods[0].method_name, "delete");
            assert!(methods[0].fields_pruned.contains("entries"));
        }

        #[test]
        fn detects_self_field_retain() {
            let methods = collect_pruning(
                "impl MyCache { fn filter(&mut self) { self.entries.retain(|_k, v| *v > 0); } }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
            assert!(methods[0].fields_pruned.contains("entries"));
        }

        #[test]
        fn detects_self_field_clear() {
            let methods = collect_pruning(
                "impl MyCache { fn reset(&mut self) { self.entries.clear(); } }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
        }

        #[test]
        fn detects_self_field_drain() {
            let methods = collect_pruning(
                "impl MyCache { fn take_all(&mut self) { let _ = self.entries.drain(); } }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
        }

        #[test]
        fn detects_self_field_pop() {
            let methods = collect_pruning(
                "impl MyCache { fn pop_one(&mut self) { self.items.pop(); } }",
                "MyCache",
                &["items"],
            );
            assert_eq!(methods.len(), 1);
        }

        #[test]
        fn detects_self_field_split_off() {
            let methods = collect_pruning(
                "impl MyCache { fn split(&mut self) { let _ = self.entries.split_off(&key); } }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
        }

        #[test]
        fn handles_ref_wrapper() {
            let methods = collect_pruning(
                "impl MyCache { fn delete(&mut self, k: &str) { (&mut self.entries).remove(k); } }",
                "MyCache",
                &["entries"],
            );
            assert_eq!(methods.len(), 1);
            assert!(methods[0].fields_pruned.contains("entries"));
        }

        #[test]
        fn ignores_non_collection_field() {
            let methods = collect_pruning(
                "impl MyCache { fn delete(&mut self, k: &str) { self.other.remove(k); } }",
                "MyCache",
                &["entries"],
            );
            assert!(methods.is_empty());
        }

        #[test]
        fn ignores_unrelated_struct() {
            let methods = collect_pruning(
                "impl OtherStruct { fn prune(&mut self) {} }",
                "MyCache",
                &["entries"],
            );
            assert!(methods.is_empty());
        }

        #[test]
        fn name_detection_covers_all_fields() {
            let methods = collect_pruning(
                "impl MyCache { fn prune(&mut self) {} }",
                "MyCache",
                &["a", "b", "c"],
            );
            assert_eq!(methods.len(), 1);
            assert!(methods[0].covers_field("a"));
            assert!(methods[0].covers_field("b"));
            assert!(methods[0].covers_field("c"));
        }

        #[test]
        fn ast_detection_covers_specific_field() {
            let methods = collect_pruning(
                "impl MyCache { fn delete(&mut self) { self.a.remove(&k); } }",
                "MyCache",
                &["a", "b"],
            );
            assert_eq!(methods.len(), 1);
            assert!(methods[0].covers_field("a"));
            assert!(!methods[0].covers_field("b"));
        }
    }

    // -----------------------------------------------------------------------
    // Analysis tests
    // -----------------------------------------------------------------------

    mod analysis {
        use super::*;

        fn field(struct_name: &str, field_name: &str, coll_type: &'static str) -> FieldRecord {
            FieldRecord {
                file_path: "test.rs".to_string(),
                struct_name: struct_name.to_string(),
                field_name: field_name.to_string(),
                collection_type: coll_type,
            }
        }

        #[test]
        fn no_pruning_is_violation() {
            let mut state = AnalysisState::default();
            state
                .struct_fields
                .insert("MyCache".to_string(), vec![field("MyCache", "entries", "HashMap")]);

            let violations = find_unbounded_fields(&state);
            assert_eq!(violations.len(), 1);
            assert_eq!(violations[0].field_name, "entries");
        }

        #[test]
        fn pruning_exists_is_ok() {
            let mut state = AnalysisState::default();
            state
                .struct_fields
                .insert("MyCache".to_string(), vec![field("MyCache", "entries", "HashMap")]);
            state.pruning_methods.insert(
                "MyCache".to_string(),
                vec![PruningMethod {
                    method_name: "prune".to_string(),
                    fields_pruned: HashSet::new(),
                }],
            );

            let violations = find_unbounded_fields(&state);
            assert!(violations.is_empty());
        }

        #[test]
        fn specific_field_pruning_only_covers_that_field() {
            let mut state = AnalysisState::default();
            state.struct_fields.insert(
                "MyCache".to_string(),
                vec![
                    field("MyCache", "a", "HashMap"),
                    field("MyCache", "b", "HashSet"),
                ],
            );
            let mut pruned = HashSet::new();
            pruned.insert("a".to_string());
            state.pruning_methods.insert(
                "MyCache".to_string(),
                vec![PruningMethod {
                    method_name: "delete_a".to_string(),
                    fields_pruned: pruned,
                }],
            );

            let violations = find_unbounded_fields(&state);
            assert_eq!(violations.len(), 1);
            assert_eq!(violations[0].field_name, "b");
        }
    }

    // -----------------------------------------------------------------------
    // Integration tests
    // -----------------------------------------------------------------------

    mod integration {
        use super::*;
        use std::io::Write;
        use tempfile::TempDir;

        fn write_file(dir: &Path, rel_path: &str, content: &str) {
            let file_path = dir.join(rel_path);
            fs::create_dir_all(file_path.parent().unwrap()).unwrap();
            let mut f = fs::File::create(&file_path).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }

        #[test]
        fn no_pruning_detected_as_violation() {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();

            write_file(
                root,
                "beacon_node/src/cache.rs",
                "struct MyCache { entries: HashMap<String, u64> }
                 impl MyCache { fn insert(&mut self, k: String, v: u64) { } }",
            );

            let mut state = AnalysisState::default();
            scan_structs_and_pruning(root, &mut state);
            let violations = find_unbounded_fields(&state);

            assert_eq!(violations.len(), 1);
            assert_eq!(violations[0].field_name, "entries");
        }

        #[test]
        fn pruning_detected_is_ok() {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();

            write_file(
                root,
                "beacon_node/src/cache.rs",
                "struct MyCache { entries: HashMap<String, u64> }
                 impl MyCache { fn prune(&mut self) { self.entries.retain(|_k, _v| true); } }",
            );

            let mut state = AnalysisState::default();
            scan_structs_and_pruning(root, &mut state);
            let violations = find_unbounded_fields(&state);

            assert!(violations.is_empty());
        }

        #[test]
        fn allowlist_suppresses_violations() {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();

            write_file(
                root,
                "beacon_node/src/cache.rs",
                "struct MyCache { entries: HashMap<String, u64> }",
            );

            write_file(
                root,
                ".github/custom/unbounded-cache-allowlist.toml",
                "[[allowed]]\nentry = \"beacon_node/src/cache.rs::MyCache::entries\"\nreason = \"test\"\n",
            );

            let mut state = AnalysisState::default();
            scan_structs_and_pruning(root, &mut state);
            let violations = find_unbounded_fields(&state);

            assert_eq!(violations.len(), 1);

            let allowlist_path = root.join(".github/custom/unbounded-cache-allowlist.toml");
            let content = fs::read_to_string(&allowlist_path).unwrap();
            let allowlist: Allowlist = toml::from_str(&content).unwrap();
            let allowed_set: HashSet<String> =
                allowlist.allowed.iter().map(|a| a.entry.clone()).collect();

            let unallowed: Vec<&FieldRecord> = violations
                .iter()
                .filter(|f| !allowed_set.contains(&f.entry_key(root)))
                .collect();

            assert!(unallowed.is_empty());
        }

        #[test]
        fn cross_file_impl_detected() {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();

            write_file(
                root,
                "beacon_node/src/types.rs",
                "struct MyCache { entries: HashMap<String, u64> }",
            );

            write_file(
                root,
                "beacon_node/src/cache_ops.rs",
                "impl MyCache { fn prune(&mut self) { self.entries.clear(); } }",
            );

            let mut state = AnalysisState::default();
            scan_structs_and_pruning(root, &mut state);
            let violations = find_unbounded_fields(&state);

            assert!(violations.is_empty());
        }

        #[test]
        fn test_files_skipped() {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();

            write_file(
                root,
                "beacon_node/src/tests/test_cache.rs",
                "struct TestCache { entries: HashMap<String, u64> }",
            );

            let mut state = AnalysisState::default();
            scan_structs_and_pruning(root, &mut state);

            assert!(state.struct_fields.is_empty());
        }
    }
}
