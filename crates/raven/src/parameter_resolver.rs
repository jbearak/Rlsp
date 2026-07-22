//
// parameter_resolver.rs
//
// Resolves function parameter information for completion suggestions.
// Supports user-defined functions (AST extraction), cross-file scope,
// and package functions (R subprocess formals() queries with caching).
//

use std::collections::HashSet;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::RwLock;

use lru::LruCache;
use tree_sitter::Node;
use url::Url;

use crate::cross_file::SymbolKind;
use crate::cross_file::scope::ScopeAtPosition;
use crate::state::WorldState;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// Information about a single function parameter.
#[derive(Debug, Clone)]
pub struct ParameterInfo {
    /// Parameter name (e.g., "x", "na.rm", "...")
    pub name: String,
    /// Default value as a string, if any (e.g., "TRUE", "NULL")
    pub default_value: Option<String>,
    /// Whether this parameter is the `...` (dots) parameter
    pub is_dots: bool,
}

impl ParameterInfo {
    /// Render the parameter as it appears in a signature: `name = default` when
    /// a default is present, otherwise the bare `name` (which is `...` for the
    /// dots parameter, since it carries no default). Single source of truth for
    /// signature-help labels, parameter-completion labels, and hover.
    pub fn label(&self) -> String {
        match &self.default_value {
            Some(default) => format!("{} = {}", self.name, default),
            None => self.name.clone(),
        }
    }
}

/// Where a function signature was obtained from.
#[derive(Debug, Clone)]
pub enum SignatureSource {
    /// From R subprocess `formals()` query
    RSubprocess { package: Option<String> },
    /// From the current file's AST
    CurrentFile { uri: Url, line: u32 },
    /// From a cross-file sourced definition
    CrossFile { uri: Url, line: u32 },
}

/// A resolved function signature with its parameters.
#[derive(Debug, Clone)]
pub struct FunctionSignature {
    /// Function name (read in tests; production code uses the cache key)
    pub name: String,
    /// Ordered list of parameters
    pub parameters: Vec<ParameterInfo>,
    /// Where this signature was obtained from
    pub source: SignatureSource,
}

// ---------------------------------------------------------------------------
// Signature cache
// ---------------------------------------------------------------------------

/// Thread-safe LRU cache for **package** function signatures (the expensive
/// `formals()` results fetched via the R subprocess).
///
/// Cache key format: `"package::function"` (e.g., `"dplyr::filter"`). User-defined
/// signatures are intentionally *not* cached: they resolve from the current file's
/// AST and the position-aware cross-file scope, both of which depend on the cursor
/// position. A position-insensitive `uri#name` cache returned stale signatures when
/// a file redefined a function (hovering a later `f(b = ...)` reused the earlier
/// `f`'s formals), so that path was removed — user resolution now always runs
/// position-aware, matching how hover/completion/go-to-definition resolve scope.
///
/// Uses `RwLock` with `peek()` for reads (no LRU promotion under read lock)
/// and `push()` for writes under write lock, consistent with existing Raven
/// cache patterns.
pub struct SignatureCache {
    /// Package function signatures ("package::function" -> signature)
    package_signatures: RwLock<LruCache<String, FunctionSignature>>,
}

// LruCache doesn't derive Debug; implement manually using finish_non_exhaustive()
impl fmt::Debug for SignatureCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignatureCache").finish_non_exhaustive()
    }
}

impl SignatureCache {
    /// Create a new package-signature cache with the given capacity.
    pub fn new(max_package: usize) -> Self {
        let pkg_cap = NonZeroUsize::new(max_package).unwrap_or(NonZeroUsize::new(500).unwrap());
        Self {
            package_signatures: RwLock::new(LruCache::new(pkg_cap)),
        }
    }

    /// Look up a package function signature (read-only, no LRU promotion).
    pub fn get_package(&self, key: &str) -> Option<FunctionSignature> {
        let cache = self.package_signatures.read().ok()?;
        cache.peek(key).cloned()
    }

    /// Insert a package function signature (promotes/evicts under write lock).
    pub fn insert_package(&self, key: String, sig: FunctionSignature) {
        if let Ok(mut cache) = self.package_signatures.write() {
            cache.push(key, sig);
        }
    }
}

// ---------------------------------------------------------------------------
// AST extraction
// ---------------------------------------------------------------------------

/// Extract parameter information from a `parameters` tree-sitter node.
///
/// Walks the node's children to find:
/// - `parameter` nodes containing `identifier`: regular parameters
/// - `parameter` nodes containing `dots`: the `...` parameter
/// - `parameter` nodes with `=` and a value: parameters with default values
/// - Direct `dots` children: the `...` parameter
///
/// Returns parameters in declaration order, including `...` (R-LS parity).
pub fn extract_from_ast(params_node: Node, text: &str) -> Vec<ParameterInfo> {
    let mut params = Vec::new();
    let mut cursor = params_node.walk();

    for child in params_node.children(&mut cursor) {
        if child.kind() == "parameter" {
            let mut param_cursor = child.walk();
            let param_children: Vec<_> = child.children(&mut param_cursor).collect();

            // Check if this parameter contains dots
            if param_children.iter().any(|n| n.kind() == "dots") {
                params.push(ParameterInfo {
                    name: "...".to_string(),
                    default_value: None,
                    is_dots: true,
                });
            } else if let Some(identifier) =
                param_children.iter().find(|n| n.kind() == "identifier")
            {
                let param_name = node_text(*identifier, text).to_string();

                // Check for default value: identifier = value
                let default_value = if param_children.len() >= 3 && param_children[1].kind() == "="
                {
                    Some(node_text(param_children[2], text).to_string())
                } else {
                    None
                };

                params.push(ParameterInfo {
                    name: param_name,
                    default_value,
                    is_dots: false,
                });
            }
        } else if child.kind() == "dots" {
            // dots can also appear directly as a child of parameters
            params.push(ParameterInfo {
                name: "...".to_string(),
                default_value: None,
                is_dots: true,
            });
        }
    }

    params
}

/// Parse the parameter list retained in a static cross-file signature string.
///
/// Handles nested parentheses and quoted commas/defaults. This is shared by
/// signature help and selective-import parameter resolution so renamed imported
/// functions do not need to rediscover their original assignment name in the AST.
pub(crate) fn parse_signature_parameters(signature: &str) -> Vec<ParameterInfo> {
    let Some(start) = signature.find('(').map(|position| position + 1) else {
        return Vec::new();
    };
    let Some(end) = signature.rfind(')') else {
        return Vec::new();
    };
    if start >= end {
        return Vec::new();
    }

    let mut params = Vec::new();
    let mut current = String::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for ch in signature[start..end].chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match quote {
            Some(active) => {
                current.push(ch);
                if ch == '\\' {
                    escaped = true;
                } else if ch == active {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' | '`' => {
                    quote = Some(ch);
                    current.push(ch);
                }
                '(' => {
                    paren_depth += 1;
                    current.push(ch);
                }
                ')' => {
                    paren_depth = paren_depth.saturating_sub(1);
                    current.push(ch);
                }
                '[' => {
                    bracket_depth += 1;
                    current.push(ch);
                }
                ']' => {
                    bracket_depth = bracket_depth.saturating_sub(1);
                    current.push(ch);
                }
                '{' => {
                    brace_depth += 1;
                    current.push(ch);
                }
                '}' => {
                    brace_depth = brace_depth.saturating_sub(1);
                    current.push(ch);
                }
                ',' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                    if !current.trim().is_empty() {
                        params.push(parse_signature_parameter(&current));
                    }
                    current.clear();
                }
                _ => current.push(ch),
            },
        }
    }
    if !current.trim().is_empty() {
        params.push(parse_signature_parameter(&current));
    }
    params
}

pub(crate) fn parse_signature_parameter(param: &str) -> ParameterInfo {
    let trimmed = param.trim();
    if trimmed == "..." || trimmed.starts_with("...") {
        return ParameterInfo {
            name: "...".to_string(),
            default_value: None,
            is_dots: true,
        };
    }

    let mut quote = None;
    let mut escaped = false;
    let mut eq_position = None;
    for (index, ch) in trimmed.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(active) => {
                if ch == '\\' {
                    escaped = true;
                } else if ch == active {
                    quote = None;
                }
            }
            None if matches!(ch, '\'' | '"' | '`') => quote = Some(ch),
            None if ch == '=' => {
                eq_position = Some(index);
                break;
            }
            None => {}
        }
    }

    if let Some(eq_position) = eq_position {
        let default = trimmed[eq_position + 1..].trim();
        ParameterInfo {
            name: trimmed[..eq_position].trim().to_string(),
            default_value: (!default.is_empty()).then(|| default.to_string()),
            is_dots: false,
        }
    } else {
        ParameterInfo {
            name: trimmed.to_string(),
            default_value: None,
            is_dots: false,
        }
    }
}

/// Get the text content of a tree-sitter node.
fn node_text<'a>(node: Node<'a>, text: &'a str) -> &'a str {
    let start = node.start_byte();
    let end = node.end_byte();
    if start <= end && end <= text.len() {
        &text[start..end]
    } else {
        ""
    }
}

// ---------------------------------------------------------------------------
// Parameter resolution
// ---------------------------------------------------------------------------

/// Resolve a function signature using ONLY user-defined sources: the current
/// file's AST, then cross-file scope.
///
/// Both phases are position-aware, and the result is intentionally *not* cached:
/// the in-scope definition depends on the cursor position (a file may redefine a
/// function, or `source()` a later one), so a position-insensitive `uri#name`
/// cache would return stale formals. See [`SignatureCache`].
///
/// Unlike [`resolve`], this never performs package resolution or an R subprocess
/// (no `block_on`), so it is safe to call from an async context such as `hover`.
/// Returns `None` for package / built-in / unknown callees. [`resolve`] delegates
/// its unqualified Phase 1/2 path here so the two cannot drift.
pub fn resolve_user_only(
    state: &WorldState,
    function_name: &str,
    current_uri: &Url,
    position: tower_lsp::lsp_types::Position,
) -> Option<FunctionSignature> {
    // Phase 1: Local AST search (current file)
    if let Some(sig) = resolve_from_current_file(state, function_name, current_uri, position) {
        return Some(sig);
    }

    // Phase 2: Cross-file scope
    resolve_from_cross_file(state, function_name, current_uri, position)
}

/// Resolve function parameters with multi-phase resolution.
///
/// Resolution priority:
/// 1. **Package cache**: For namespace-qualified calls, check the package cache
/// 2. **Local AST**: Search the current file for the nearest in-scope function
///    definition before the cursor position (works for untitled/unsaved docs)
/// 3. **Cross-file scope**: Search sourced files for function definitions
/// 4. **Package**: Determine which package exports the function using the scope
///    resolver's position-aware `loaded_packages` + `inherited_packages`, then
///    query R subprocess for its formals
///
/// Phases 2-3 (the user-defined path, for unqualified names) are delegated to
/// [`resolve_user_only`]; phases 1 and 4 are the package path unique to this
/// entry point.
///
/// This function is synchronous and may block on R subprocess for package
/// functions. The backend wraps it in `spawn_blocking`.
pub fn resolve(
    state: &WorldState,
    cache: &SignatureCache,
    function_name: &str,
    namespace: Option<&str>,
    is_internal: bool,
    current_uri: &Url,
    position: tower_lsp::lsp_types::Position,
) -> Option<FunctionSignature> {
    // --- Phase 1: Package cache (namespace-qualified calls) ---

    // For namespace-qualified calls (e.g., dplyr::filter), check package cache
    if let Some(ns) = namespace {
        let cache_key = format!("{}::{}", ns, function_name);
        if let Some(sig) = cache.get_package(&cache_key) {
            return Some(sig);
        }
    }

    // --- Phases 2 & 3: Local / cross-file (only for unqualified calls) ---
    // Namespace-qualified calls (e.g., dplyr::filter) skip user-defined lookup
    // and go straight to package resolution.
    if namespace.is_none()
        && let Some(sig) = resolve_user_only(state, function_name, current_uri, position)
    {
        return Some(sig);
    }

    // --- Phase 4: Package resolution ---
    // For unqualified names, check package cache with all possible package keys
    // before attempting R subprocess
    let scope = get_scope(state, current_uri, position);
    let all_packages = collect_packages_at_position(state, &scope);

    // If namespace is specified, we already checked the cache above.
    // For unqualified names, try to find which package exports this function.
    let resolved_package = if let Some(ns) = namespace {
        Some(ns.to_string())
    } else {
        let lookup_key =
            crate::handlers::unquote_backtick_name(function_name).unwrap_or(function_name);
        let selective_package = scope
            .symbols
            .get(function_name)
            .or_else(|| scope.symbols.get(lookup_key))
            .and_then(|symbol| symbol.source_uri.as_str().strip_prefix("package:"))
            .filter(|package| !package.is_empty())
            .map(str::to_string);
        selective_package.or_else(|| {
            // Resolve the true owner package (issue #407) so formals come from
            // the package that actually defines the function (e.g. `dplyr` for
            // a `mutate` made visible through `library(tidyverse)`), not the
            // aggregate that merely made it visible.
            let pkg_list: Vec<String> = all_packages.to_vec();
            state
                .package_library
                .find_package_owner_for_symbol(function_name, &pkg_list)
        })
    };

    if let Some(ref pkg_name) = resolved_package {
        // A load_all() internal resolves to the synthetic sentinel package,
        // which is not installed and must never be sent to the R subprocess
        // (`get_function_formals` already rejects it as an invalid package
        // name, but short-circuit here so we never build a sentinel cache key
        // or attempt the query). Local/cross-file resolution already ran above,
        // so returning None here simply declines package-formal resolution (the
        // caller then falls back to generic signature help).
        if crate::package_library::is_load_all_sentinel(pkg_name) {
            return None;
        }
        let cache_key = format!("{}::{}", pkg_name, function_name);
        if let Some(sig) = cache.get_package(&cache_key) {
            return Some(sig);
        }

        // R subprocess integration with graceful degradation (Requirement 11.1, 11.2, 11.3)
        // Query R for function formals using get_function_formals
        if let Some(r_subprocess) = state.package_library.r_subprocess() {
            // Get tokio runtime handle for async call from sync context
            let handle = match tokio::runtime::Handle::try_current() {
                Ok(h) => h,
                Err(_) => {
                    log::trace!(
                        "No tokio runtime available for R subprocess query; skipping package function {}::{}",
                        pkg_name,
                        function_name
                    );
                    return None;
                }
            };

            // Call async get_function_formals from sync context
            let formals_result = handle.block_on(r_subprocess.get_function_formals(
                function_name,
                Some(pkg_name),
                !is_internal,
            ));

            match formals_result {
                Ok(params) => {
                    // Success: build signature, cache it, and return
                    let signature = FunctionSignature {
                        name: function_name.to_string(),
                        parameters: params,
                        source: SignatureSource::RSubprocess {
                            package: Some(pkg_name.clone()),
                        },
                    };
                    cache.insert_package(cache_key, signature.clone());
                    log::trace!(
                        "Resolved package function {}::{} via R subprocess ({} params)",
                        pkg_name,
                        function_name,
                        signature.parameters.len()
                    );
                    return Some(signature);
                }
                Err(e) => {
                    // Graceful degradation: log error and return None
                    // Timeouts are logged at warn level (Requirement 11.3)
                    // Other errors (parse failures, missing functions) at trace level
                    if e.to_string().contains("timeout") || e.to_string().contains("timed out") {
                        log::warn!(
                            "R subprocess timeout querying formals for {}::{}: {}",
                            pkg_name,
                            function_name,
                            e
                        );
                    } else {
                        log::trace!(
                            "Failed to get formals for {}::{} from R subprocess: {}",
                            pkg_name,
                            function_name,
                            e
                        );
                    }
                    // Fall through to return None (standard completions will still appear)
                }
            }
        } else {
            log::trace!(
                "No R subprocess available; cannot query formals for {}::{}",
                pkg_name,
                function_name
            );
        }
    }

    None
}

/// Retrieve document text and parsed AST from any available document source.
///
/// Searches multiple stores in priority order so that cross-file parameter
/// resolution works even when the source file is not open in the editor.
///
/// Priority order (matching the `content_provider` pattern):
/// 1. Authoritative open documents (`state.documents`)
/// 2. New workspace index (`state.workspace_index`)
/// 3. File cache (`state.cross_file_file_cache`) — parse on demand
pub(crate) fn get_text_and_tree(
    state: &WorldState,
    uri: &Url,
) -> Option<(String, tree_sitter::Tree)> {
    // 1. Open documents. Return the analysis text (masked for Rmd) so
    //    the caller's byte-offset slices into `tree` align; identical to the
    //    raw text for plain R / JAGS / Stan.
    if let Some(doc) = state.documents.get(uri) {
        if let Some(tree) = &doc.tree {
            return Some((doc.analysis_text(), tree.clone()));
        } else {
            log::debug!("Document found but has no parsed tree: {}", uri);
        }
    }

    // 2. New workspace index (indexed closed files). The entry's `tree` is
    //    parsed from the masked analysis text for Rmd/Quarto (on-demand
    //    indexing), while `contents` is RAW — pair the tree with the masked
    //    analysis view so byte offsets align (#343).
    if let Some(entry) = state.workspace_index.get(uri) {
        if let Some(tree) = &entry.tree {
            let raw = entry.contents.to_string();
            let text = state.analysis_text_for_uri(uri, &raw).into_owned();
            return Some((text, tree.clone()));
        } else {
            log::debug!("Document in workspace_index has no parsed tree: {}", uri);
        }
    }

    // 3. File cache — content available but no pre-parsed tree; parse on demand.
    //    The cache stores RAW content; parse (and return) the masked analysis
    //    text for Rmd/Quarto so a closed `.Rmd` resolves chunk-defined symbols
    //    rather than failing closed, and the (text, tree) pair stays aligned
    //    (raw == analysis for plain R, so this is behavior-neutral there) (#343).
    if let Some(content) = state.cross_file_file_cache.get(uri) {
        let analysis = state.analysis_text_for_uri(uri, &content).into_owned();
        if let Some(tree) = crate::parser_pool::with_parser(|p| p.parse(&analysis, None)) {
            return Some((analysis, tree));
        } else {
            log::debug!("Failed to parse file cache content for: {}", uri);
        }
    }

    None
}

/// Search the current file's AST for a user-defined function with the given name.
///
/// Finds the nearest function definition that appears before the cursor position,
/// preferring the innermost enclosing scope. Works for untitled/unsaved documents
/// since it uses in-memory content.
fn resolve_from_current_file(
    state: &WorldState,
    function_name: &str,
    uri: &Url,
    position: tower_lsp::lsp_types::Position,
) -> Option<FunctionSignature> {
    let doc = state.get_document(uri)?;
    let tree = doc.tree.as_ref()?;
    // Analysis text (masked for Rmd) matches `tree`'s byte offsets; equals the
    // raw text for plain R. Signature help is gated for Rmd at the entry point,
    // but pairing the tree with the analysis text is the correct invariant.
    let text = doc.analysis_text();

    // Search for the nearest function definition before cursor position
    let func_node =
        find_function_definition_before_position(tree.root_node(), function_name, &text, position)?;

    // Find the formal_parameters child of the function_definition node
    let params_node = func_node
        .children(&mut func_node.walk())
        .find(|c| c.kind() == "parameters")?;

    let parameters = extract_from_ast(params_node, &text);
    // Anchor roxygen at the assignment-expression start (the `f <-` line), not
    // the `function` keyword line: for a multi-line definition the block attaches
    // above the assignment, so the keyword line would scan past it and drop the
    // docs. The parent of a matched `function_definition` is always its
    // assignment — `find_function_definition_before_position` only returns
    // assigned functions — and its start row is the LHS for `<-`/`=` (the value
    // for `->`), which coincides with cross-file `defined_line` for `<-`/`=`
    // (multi-line right-assignment is the rare exception).
    let def_line = func_node
        .parent()
        .map_or(func_node.start_position().row, |p| p.start_position().row)
        as u32;

    Some(FunctionSignature {
        name: function_name.to_string(),
        parameters,
        source: SignatureSource::CurrentFile {
            uri: uri.clone(),
            line: def_line,
        },
    })
}

/// Search cross-file scope for a function definition.
///
/// Uses the cross-file scope resolver to find function definitions in sourced files.
fn resolve_from_cross_file(
    state: &WorldState,
    function_name: &str,
    uri: &Url,
    position: tower_lsp::lsp_types::Position,
) -> Option<FunctionSignature> {
    let scope = get_scope(state, uri, position);

    // Look for the function in cross-file symbols. Issue #459: a cross-file
    // scope entry is keyed either bare (a regular assignment def) or
    // backtick-wrapped (a directive-declared non-syntactic symbol). Mirror
    // go-to-definition: try the RAW spelling first, then the
    // unconditionally-unquoted spelling, so a redundantly-quoted `` `f` `` and a
    // required `` `my fn` `` both resolve to their stored binding.
    let lookup_key = crate::handlers::unquote_backtick_name(function_name).unwrap_or(function_name);
    let symbol = scope
        .symbols
        .get(function_name)
        .or_else(|| scope.symbols.get(lookup_key))?;

    // Only consider function symbols from other files (not the current file,
    // which was already checked in Phase 1)
    if symbol.source_uri == *uri {
        return None;
    }

    // Only consider actual function definitions, not variables or package exports
    if symbol.kind != SymbolKind::Function {
        return None;
    }

    // Skip package-sourced symbols (those have "package:" URIs). The package
    // phase resolves those using the selective binding's exact source package.
    if symbol.source_uri.as_str().starts_with("package:") {
        return None;
    }

    // Selective local-module imports retain the defining symbol's signature.
    // Prefer it over an AST name lookup because a renamed attachment's local
    // name intentionally differs from the source assignment name.
    if let Some(signature) = &symbol.signature {
        return Some(FunctionSignature {
            name: function_name.to_string(),
            parameters: parse_signature_parameters(signature),
            source: SignatureSource::CrossFile {
                uri: symbol.source_uri.clone(),
                line: symbol.defined_line,
            },
        });
    }

    // Try to get the source file's AST and extract parameters
    // Use get_text_and_tree to access documents from any source (open, workspace index, etc.)
    let (source_text, source_tree) = get_text_and_tree(state, &symbol.source_uri)?;

    // Find the function definition at the known line
    let func_node = find_function_definition_at_line(
        source_tree.root_node(),
        function_name,
        &source_text,
        symbol.defined_line,
    )?;

    let params_node = func_node
        .children(&mut func_node.walk())
        .find(|c| c.kind() == "parameters")?;

    let parameters = extract_from_ast(params_node, &source_text);

    Some(FunctionSignature {
        name: function_name.to_string(),
        parameters,
        source: SignatureSource::CrossFile {
            uri: symbol.source_uri.clone(),
            line: symbol.defined_line,
        },
    })
}

// ---------------------------------------------------------------------------
// AST search helpers
// ---------------------------------------------------------------------------

/// Find the nearest function definition with the given name that appears
/// before the cursor position in the AST.
///
/// Searches the entire tree for assignment nodes where the LHS matches
/// `function_name` and the RHS is a `function_definition`. Returns the
/// `function_definition` node of the last (nearest) match before the cursor.
fn find_function_definition_before_position<'a>(
    root: Node<'a>,
    function_name: &str,
    text: &str,
    position: tower_lsp::lsp_types::Position,
) -> Option<Node<'a>> {
    let mut best: Option<Node<'a>> = None;
    // Canonicalize the request-constant target once, not per node in the walk.
    let canonical_function_name = crate::r_names::canonical_use_name(function_name);
    find_func_def_recursive(root, canonical_function_name, text, position, &mut best);
    best
}

/// Recursive helper for [`find_function_definition_before_position`].
///
/// Issue #459: definition names are matched against the *raw* AST node text
/// (which carries backticks for a non-syntactic name), so both equality
/// operands are run through `canonical_use_name`. That unions a redundantly
/// backtick-quoted syntactic call (`` `f`(...) ``) with the bare `f <- function`
/// definition while a genuinely non-syntactic def/use pair (`` `my fn` ``) keeps
/// its required backticks and still matches.
fn find_func_def_recursive<'a>(
    node: Node<'a>,
    canonical_function_name: &str,
    text: &str,
    position: tower_lsp::lsp_types::Position,
    best: &mut Option<Node<'a>>,
) {
    // Match both left- (`<-`/`=`/`<<-`) and right-assignment (`->`/`->>`)
    // definitions. In tree-sitter-r both are `binary_operator` nodes (there is
    // no distinct `right_assignment` kind), so fetch the children and `op_text`
    // ONCE and branch on the operator; the `op_text` guards keep the two arms
    // mutually exclusive.
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        // Filter extras (e.g., comments) so positional indexing is reliable
        let children = crate::parser_pool::non_extra_children(node, &mut cursor);

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];
            let rhs = children[2];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-")
                && lhs.kind() == "identifier"
                && crate::r_names::canonical_use_name(node_text(lhs, text))
                    == canonical_function_name
                && rhs.kind() == "function_definition"
            {
                // Left-assignment: `name <- function(...)`, name on the LHS.
                // Only consider definitions before the cursor position.
                let def_line = lhs.start_position().row as u32;
                if def_line <= position.line {
                    // Take the last (nearest) definition before cursor
                    match best {
                        Some(prev) => {
                            if def_line >= prev.start_position().row as u32 {
                                *best = Some(rhs);
                            }
                        }
                        None => {
                            *best = Some(rhs);
                        }
                    }
                }
            } else if matches!(op_text, "->" | "->>")
                && rhs.kind() == "identifier"
                && crate::r_names::canonical_use_name(node_text(rhs, text))
                    == canonical_function_name
                && let Some(func) = crate::cross_file::scope::unwrap_function_definition(lhs)
            {
                // Right-assignment: `(function(...)) -> name`, the mirror of the
                // `<-` arm with the value on the LHS and the name on the RHS.
                // The function value must be parenthesized (a bare
                // `function(...) body -> name` parses with `-> name` *inside*
                // the body), so the LHS is unwrapped through
                // `parenthesized_expression` layers.
                let def_line = rhs.start_position().row as u32;
                if def_line <= position.line {
                    match best {
                        Some(prev) => {
                            if def_line >= prev.start_position().row as u32 {
                                *best = Some(func);
                            }
                        }
                        None => {
                            *best = Some(func);
                        }
                    }
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_func_def_recursive(child, canonical_function_name, text, position, best);
    }
}

/// Find a function definition at a specific line in the AST.
///
/// Used for cross-file resolution where we know the exact line of the
/// definition. Issue #459: like the sibling `find_func_def_recursive`, both
/// equality operands are run through `canonical_use_name` so a redundantly
/// backtick-quoted call (`` `f`(...) ``) matches a bare cross-file def
/// (`f <- function`, stored bare by Seam A) while a genuinely non-syntactic
/// def/use pair keeps its required backticks. `resolve_from_cross_file` passes
/// the RAW (still-backticked) `function_name` here, so canonicalizing the call
/// side is required for the bare-stored def node to match.
fn find_function_definition_at_line<'a>(
    root: Node<'a>,
    function_name: &str,
    text: &str,
    target_line: u32,
) -> Option<Node<'a>> {
    let mut result: Option<Node<'a>> = None;
    // Canonicalize the request-constant target once, not per node in the walk.
    let canonical_function_name = crate::r_names::canonical_use_name(function_name);
    find_func_def_at_line_recursive(
        root,
        canonical_function_name,
        text,
        target_line,
        &mut result,
    );
    result
}

fn find_func_def_at_line_recursive<'a>(
    node: Node<'a>,
    canonical_function_name: &str,
    text: &str,
    target_line: u32,
    result: &mut Option<Node<'a>>,
) {
    if result.is_some() {
        return; // Already found
    }

    // Match both left- (`<-`/`=`/`<<-`) and right-assignment (`->`/`->>`)
    // definitions; both are `binary_operator` nodes in tree-sitter-r, so fetch
    // the children and `op_text` ONCE and branch on the operator (the `op_text`
    // guards keep the two arms mutually exclusive). Return on the first match at
    // `target_line`.
    if node.kind() == "binary_operator" {
        let mut cursor = node.walk();
        let children = crate::parser_pool::non_extra_children(node, &mut cursor);

        if children.len() >= 3 {
            let lhs = children[0];
            let op = children[1];
            let rhs = children[2];

            let op_text = node_text(op, text);
            if matches!(op_text, "<-" | "=" | "<<-")
                && lhs.kind() == "identifier"
                && crate::r_names::canonical_use_name(node_text(lhs, text))
                    == canonical_function_name
                && rhs.kind() == "function_definition"
                && lhs.start_position().row as u32 == target_line
            {
                *result = Some(rhs);
                return;
            } else if matches!(op_text, "->" | "->>")
                && rhs.kind() == "identifier"
                && crate::r_names::canonical_use_name(node_text(rhs, text))
                    == canonical_function_name
                && rhs.start_position().row as u32 == target_line
                && let Some(func) = crate::cross_file::scope::unwrap_function_definition(lhs)
            {
                // Right-assignment: `(function(...)) -> name`, the value
                // unwrapped through `parenthesized_expression` layers.
                *result = Some(func);
                return;
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_func_def_at_line_recursive(child, canonical_function_name, text, target_line, result);
    }
}

// ---------------------------------------------------------------------------
// Scope helpers
// ---------------------------------------------------------------------------

/// Get the cross-file scope at the given position.
fn get_scope(
    state: &WorldState,
    uri: &Url,
    position: tower_lsp::lsp_types::Position,
) -> ScopeAtPosition {
    use crate::content_provider::ContentProvider;
    use crate::cross_file::scope;

    let content_provider = state.content_provider();

    let get_artifacts = |target_uri: &Url| -> Option<std::sync::Arc<scope::ScopeArtifacts>> {
        content_provider.get_artifacts(target_uri)
    };

    let get_metadata =
        |target_uri: &Url| -> Option<std::sync::Arc<crate::cross_file::CrossFileMetadata>> {
            content_provider.get_metadata(target_uri)
        };

    let max_depth = state.cross_file_config.max_chain_depth;

    let base_exports = if state.package_library_ready {
        state.package_library.base_exports().clone()
    } else {
        crate::handlers::empty_base_exports().clone()
    };

    // `data()` alias expansion provider (issue #429); gated on package-library
    // readiness so we never expand against an empty cache.
    let data_lookup = |pkg: &str, stem: &str| -> Vec<String> {
        state.package_library.data_objects_for_stem_sync(pkg, stem)
    };
    let data_provider = (state.cross_file_config.packages_enabled && state.package_library_ready)
        .then(|| scope::DataAliasProvider {
            lookup: &data_lookup,
            base_packages: state.package_library.base_packages(),
        });
    let import_env = crate::box_use::ArtifactModuleExportEnv::new(
        &get_artifacts,
        &get_metadata,
        (state.cross_file_config.packages_enabled && state.package_library_ready)
            .then_some(state.package_library.as_ref()),
    );
    let selective_import_provider = scope::SelectiveImportProvider { env: &import_env };

    let package_contribution = state.package_state.scope_contribution();
    let package_query_uri = package_contribution
        .workspace_root
        .as_ref()
        .and_then(|root| state.authoritative_workspace_query_uri_for_open_document(uri, root));

    scope::scope_at_position_with_graph_with_package_query_uri(
        uri,
        position.line,
        position.character,
        &get_artifacts,
        &get_metadata,
        &state.cross_file_graph,
        state.workspace_folders.first(),
        max_depth,
        &base_exports,
        state.cross_file_config.hoist_globals_in_functions,
        state.cross_file_config.backward_dependencies,
        &|| false, // non-diagnostic path, no cancellation,
        Some(package_contribution),
        data_provider.as_ref(),
        Some(&selective_import_provider),
        package_query_uri.as_ref(),
    )
}

/// Collect all packages available at the cursor position.
///
/// Combines base packages, inherited packages, and loaded packages from the
/// scope resolver's position-aware package list.
fn collect_packages_at_position(state: &WorldState, scope: &ScopeAtPosition) -> Vec<String> {
    let mut pkg_set: HashSet<String> = HashSet::new();
    let mut all_packages: Vec<String> = Vec::new();

    // Base packages first (always available without library() calls)
    if state.package_library_ready {
        for pkg in state.package_library.base_packages() {
            if pkg_set.insert(pkg.clone()) {
                all_packages.push(pkg.clone());
            }
        }
    }

    // Then inherited and explicitly loaded packages
    for pkg in scope
        .inherited_packages
        .iter()
        .chain(scope.loaded_packages.iter())
    {
        if pkg_set.insert(pkg.clone()) {
            all_packages.push(pkg.clone());
        }
    }

    all_packages
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Shared test utilities for parameter_resolver test modules.
#[cfg(test)]
mod test_utils {
    use tree_sitter::Node;

    use super::node_text;

    /// Find a function_definition node by name in the tree.
    pub fn find_function_def_in_tree<'a>(
        node: Node<'a>,
        name: &str,
        text: &str,
    ) -> Option<Node<'a>> {
        if node.kind() == "binary_operator" {
            let mut cursor = node.walk();
            let children = crate::parser_pool::non_extra_children(node, &mut cursor);

            if children.len() >= 3 {
                let lhs = children[0];
                let op = children[1];
                let rhs = children[2];

                let op_text = node_text(op, text);
                if matches!(op_text, "<-" | "=" | "<<-")
                    && lhs.kind() == "identifier"
                    && node_text(lhs, text) == name
                    && rhs.kind() == "function_definition"
                {
                    return Some(rhs);
                }
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(func_node) = find_function_def_in_tree(child, name, text) {
                return Some(func_node);
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        let language = tree_sitter_r::LANGUAGE;
        parser
            .set_language(&language.into())
            .expect("Error loading R grammar");
        parser.parse(code, None).expect("Error parsing R code")
    }

    // -- SignatureCache tests --

    #[test]
    fn test_cache_new_default_capacities() {
        let cache = SignatureCache::new(500);
        assert!(cache.get_package("nonexistent").is_none());
    }

    #[test]
    fn test_cache_insert_and_get_package() {
        let cache = SignatureCache::new(10);
        let sig = FunctionSignature {
            name: "filter".to_string(),
            parameters: vec![ParameterInfo {
                name: ".data".to_string(),
                default_value: None,
                is_dots: false,
            }],
            source: SignatureSource::RSubprocess {
                package: Some("dplyr".to_string()),
            },
        };
        cache.insert_package("dplyr::filter".to_string(), sig.clone());
        let cached = cache.get_package("dplyr::filter");
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().name, "filter");
    }

    #[test]
    fn test_cache_lru_eviction() {
        // Create a cache with capacity 2
        let cache = SignatureCache::new(2);

        for i in 0..3 {
            let sig = FunctionSignature {
                name: format!("func_{}", i),
                parameters: vec![],
                source: SignatureSource::RSubprocess { package: None },
            };
            cache.insert_package(format!("pkg::func_{}", i), sig);
        }

        // The first entry should have been evicted
        assert!(cache.get_package("pkg::func_0").is_none());
        // The last two should still be present
        assert!(cache.get_package("pkg::func_1").is_some());
        assert!(cache.get_package("pkg::func_2").is_some());
    }

    // -- extract_from_ast tests --

    #[test]
    fn test_extract_simple_params() {
        let code = "f <- function(x, y) { x + y }";
        let tree = parse_r_code(code);
        let func_node = find_function_def_in_tree(tree.root_node(), "f", code).unwrap();
        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .unwrap();

        let params = extract_from_ast(params_node, code);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name, "x");
        assert!(params[0].default_value.is_none());
        assert!(!params[0].is_dots);
        assert_eq!(params[1].name, "y");
        assert!(params[1].default_value.is_none());
        assert!(!params[1].is_dots);
    }

    #[test]
    fn test_extract_params_with_defaults() {
        let code = "f <- function(x = 1, y = \"hello\") { x }";
        let tree = parse_r_code(code);
        let func_node = find_function_def_in_tree(tree.root_node(), "f", code).unwrap();
        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .unwrap();

        let params = extract_from_ast(params_node, code);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name, "x");
        assert_eq!(params[0].default_value.as_deref(), Some("1"));
        assert_eq!(params[1].name, "y");
        assert_eq!(params[1].default_value.as_deref(), Some("\"hello\""));
    }

    #[test]
    fn test_extract_params_with_dots() {
        let code = "f <- function(...) { list(...) }";
        let tree = parse_r_code(code);
        let func_node = find_function_def_in_tree(tree.root_node(), "f", code).unwrap();
        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .unwrap();

        let params = extract_from_ast(params_node, code);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "...");
        assert!(params[0].is_dots);
        assert!(params[0].default_value.is_none());
    }

    #[test]
    fn test_extract_mixed_params() {
        let code = "f <- function(x, y = 1, ...) { x }";
        let tree = parse_r_code(code);
        let func_node = find_function_def_in_tree(tree.root_node(), "f", code).unwrap();
        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .unwrap();

        let params = extract_from_ast(params_node, code);
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].name, "x");
        assert!(params[0].default_value.is_none());
        assert!(!params[0].is_dots);
        assert_eq!(params[1].name, "y");
        assert_eq!(params[1].default_value.as_deref(), Some("1"));
        assert!(!params[1].is_dots);
        assert_eq!(params[2].name, "...");
        assert!(params[2].is_dots);
    }

    #[test]
    fn test_extract_no_params() {
        let code = "f <- function() { 42 }";
        let tree = parse_r_code(code);
        let func_node = find_function_def_in_tree(tree.root_node(), "f", code).unwrap();
        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .unwrap();

        let params = extract_from_ast(params_node, code);
        assert!(params.is_empty());
    }

    #[test]
    fn test_extract_complex_default_values() {
        let code = "f <- function(x = c(1, 2, 3), y = NULL) { x }";
        let tree = parse_r_code(code);
        let func_node = find_function_def_in_tree(tree.root_node(), "f", code).unwrap();
        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .unwrap();

        let params = extract_from_ast(params_node, code);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name, "x");
        assert_eq!(params[0].default_value.as_deref(), Some("c(1, 2, 3)"));
        assert_eq!(params[1].name, "y");
        assert_eq!(params[1].default_value.as_deref(), Some("NULL"));
    }

    // -- find_function_definition_before_position tests --

    #[test]
    fn test_find_func_def_before_position() {
        let code = "f <- function(x) { x }\ng <- function(y) { y }\nf(1)";
        let tree = parse_r_code(code);

        // At line 2 (the call site), both f and g should be findable
        let pos = tower_lsp::lsp_types::Position::new(2, 0);
        let f_node = find_function_definition_before_position(tree.root_node(), "f", code, pos);
        assert!(f_node.is_some());

        let g_node = find_function_definition_before_position(tree.root_node(), "g", code, pos);
        assert!(g_node.is_some());
    }

    #[test]
    fn test_find_func_def_not_before_position() {
        let code = "f(1)\nf <- function(x) { x }";
        let tree = parse_r_code(code);

        // At line 0 (before the definition), f should not be found
        let pos = tower_lsp::lsp_types::Position::new(0, 0);
        let result = find_function_definition_before_position(tree.root_node(), "f", code, pos);
        // The definition is on line 1, cursor is on line 0
        assert!(result.is_none());
    }

    #[test]
    fn test_find_func_def_nearest_wins() {
        let code = "f <- function(x) { x }\nf <- function(x, y) { x + y }\nf(1)";
        let tree = parse_r_code(code);

        // At line 2, the second definition (line 1) should win
        let pos = tower_lsp::lsp_types::Position::new(2, 0);
        let func_node =
            find_function_definition_before_position(tree.root_node(), "f", code, pos).unwrap();

        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .unwrap();
        let params = extract_from_ast(params_node, code);
        assert_eq!(params.len(), 2); // Second definition has 2 params
    }

    #[test]
    fn test_resolve_redefined_function_is_position_scoped() {
        // The user-signature cache was removed because a position-insensitive
        // `uri#name` key served stale formals when a file redefined a function.
        // This guards the `resolve()` entry point (used by signature-help and
        // completion) against re-introducing such a cache: resolution must
        // follow whichever definition is in scope at the cursor. Hover calls
        // `resolve_user_only` directly, so its redefinition test does not cover
        // the `resolve()` path the other two consumers go through.
        let mut state = WorldState::new();
        let uri = Url::parse("file:///redef.R").unwrap();
        let code = "\
f <- function(alpha) alpha
f(alpha = 1)
f <- function(beta) beta
f(beta = 2)
";
        state
            .documents
            .insert(uri.clone(), crate::state::Document::new(code, None));
        let cache = SignatureCache::new(10);

        let names = |sig: FunctionSignature| {
            sig.parameters
                .into_iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
        };

        // At the earlier call (line 1) only the `alpha` definition is in scope.
        let early = resolve(
            &state,
            &cache,
            "f",
            None,
            false,
            &uri,
            tower_lsp::lsp_types::Position::new(1, 2),
        )
        .expect("earlier call resolves to a user signature");
        assert_eq!(names(early), vec!["alpha"]);

        // At the later call (line 3) the `beta` redefinition shadows it.
        let late = resolve(
            &state,
            &cache,
            "f",
            None,
            false,
            &uri,
            tower_lsp::lsp_types::Position::new(3, 2),
        )
        .expect("later call resolves to a user signature");
        assert_eq!(names(late), vec!["beta"]);
    }

    #[test]
    fn renamed_local_module_function_uses_retained_signature() {
        let tmp = tempfile::TempDir::new().unwrap();
        let module_path = tmp.path().join("mod.r");
        let importer_path = tmp.path().join("main.R");
        let module_code = "box::export(original)\noriginal <- function(x = m[1, 2], y = c(1, 2), label = \"a,b\", `a,b` = 1, ...) x\n";
        let importer_code = "box::use(./mod[renamed = original])\nrenamed(\n";
        std::fs::write(&module_path, module_code).unwrap();
        std::fs::write(&importer_path, importer_code).unwrap();

        let module_uri = Url::from_file_path(&module_path).unwrap();
        let importer_uri = Url::from_file_path(&importer_path).unwrap();
        let mut state = WorldState::new();
        state.workspace_scan_complete = true;
        state.open_document(module_uri.clone(), module_code, Some(1));
        state.open_document(importer_uri.clone(), importer_code, Some(1));

        let signature = resolve_user_only(
            &state,
            "renamed",
            &importer_uri,
            tower_lsp::lsp_types::Position::new(1, 8),
        )
        .expect("renamed module function must retain its source signature");
        assert!(matches!(
            signature.source,
            SignatureSource::CrossFile { ref uri, line: 1 } if uri == &module_uri
        ));
        let labels = signature
            .parameters
            .iter()
            .map(ParameterInfo::label)
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                "x = m[1, 2]",
                "y = c(1, 2)",
                "label = \"a,b\"",
                "`a,b` = 1",
                "...",
            ]
        );
    }

    #[tokio::test]
    async fn attached_package_function_uses_exact_selective_source() {
        use crate::package_library::{PackageInfo, PackageLibrary};
        use std::collections::HashSet;
        use std::sync::Arc;

        let mut state = WorldState::new();
        state.workspace_scan_complete = true;
        state.cross_file_config.packages_enabled = true;
        state.package_library_ready = true;
        state.package_library = Arc::new(PackageLibrary::new_empty());
        state
            .package_library
            .insert_package(PackageInfo::new(
                "pkgA".to_string(),
                HashSet::from(["shared".to_string()]),
            ))
            .await;

        let uri = Url::parse("file:///workspace/main.R").unwrap();
        let code = "box::use(pkgA[shared])\nshared(\n";
        state.open_document(uri.clone(), code, Some(1));
        let cache = SignatureCache::new(10);
        for (package, parameter) in [("pkgA", "from_a"), ("pkgB", "from_b")] {
            cache.insert_package(
                format!("{package}::shared"),
                FunctionSignature {
                    name: "shared".to_string(),
                    parameters: vec![ParameterInfo {
                        name: parameter.to_string(),
                        default_value: None,
                        is_dots: false,
                    }],
                    source: SignatureSource::RSubprocess {
                        package: Some(package.to_string()),
                    },
                },
            );
        }

        let signature = resolve(
            &state,
            &cache,
            "shared",
            None,
            false,
            &uri,
            tower_lsp::lsp_types::Position::new(1, 7),
        )
        .expect("selectively attached package function must resolve cached formals");
        assert_eq!(signature.parameters[0].name, "from_a");
        assert!(matches!(
            signature.source,
            SignatureSource::RSubprocess {
                package: Some(ref package)
            } if package == "pkgA"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn alias_open_test_file_uses_canonical_testthat_scope_for_package_signature() {
        use crate::package_library::{PackageInfo, PackageLibrary};
        use crate::package_state::{PackageScopeContribution, PackageState};
        use crate::state::{Document, WorldState};
        use std::collections::{BTreeSet, HashSet};
        use std::sync::Arc;

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path().join("pkg");
            let link = tmp.path().join("pkg-link");
            std::fs::create_dir_all(root.join("tests").join("testthat")).unwrap();
            std::os::unix::fs::symlink(&root, &link).unwrap();

            let canonical_uri =
                Url::from_file_path(root.join("tests").join("testthat").join("test-x.R")).unwrap();
            let alias_uri =
                Url::from_file_path(link.join("tests").join("testthat").join("test-x.R")).unwrap();
            let workspace_uri = Url::from_file_path(&root).unwrap();

            let mut state = WorldState::new();
            state.workspace_folders = vec![workspace_uri];
            state.workspace_scan_complete = true;
            state.cross_file_config.packages_enabled = true;
            state.package_library = Arc::new(PackageLibrary::new_empty());
            state.package_library_ready = true;

            let mut exports = HashSet::new();
            exports.insert("expect_equal".to_string());
            state
                .package_library
                .insert_package(PackageInfo::new("testthat".to_string(), exports))
                .await;

            let mut test_attached = BTreeSet::new();
            test_attached.insert("testthat".to_string());
            state.package_state.set_from(PackageState {
                scope_contribution: PackageScopeContribution {
                    workspace_root: Some(root.clone()),
                    test_attached_packages: Arc::new(test_attached),
                    ..PackageScopeContribution::default()
                },
                ..PackageState::default()
            });

            let code = "expect_equal(1, 1)\n";
            state.documents.insert(
                alias_uri.clone(),
                Document::new_with_uri(code, None, &alias_uri),
            );
            state
                .open_document_aliases
                .open(alias_uri.clone(), vec![canonical_uri.clone()]);
            assert_eq!(
                state.authoritative_workspace_query_uri_for_open_document(&alias_uri, &root),
                Some(canonical_uri),
                "precondition: alias-open test file must resolve to the canonical package URI"
            );

            let cache = SignatureCache::new(10);
            cache.insert_package(
                "testthat::expect_equal".to_string(),
                FunctionSignature {
                    name: "expect_equal".to_string(),
                    parameters: vec![
                        ParameterInfo {
                            name: "object".to_string(),
                            default_value: None,
                            is_dots: false,
                        },
                        ParameterInfo {
                            name: "expected".to_string(),
                            default_value: None,
                            is_dots: false,
                        },
                    ],
                    source: SignatureSource::RSubprocess {
                        package: Some("testthat".to_string()),
                    },
                },
            );

            let sig = resolve(
                &state,
                &cache,
                "expect_equal",
                None,
                false,
                &alias_uri,
                tower_lsp::lsp_types::Position::new(0, 12),
            )
            .expect("alias-open test file must resolve testthat::expect_equal formals");
            let names: Vec<String> = sig.parameters.into_iter().map(|p| p.name).collect();
            assert_eq!(names, vec!["object", "expected"]);
            assert!(
                matches!(
                    sig.source,
                    SignatureSource::RSubprocess {
                        package: Some(ref package)
                    } if package == "testthat"
                ),
                "signature must come from the Suggests/test-attached package"
            );
        });
    }

    /// Issue #459: a redundantly backtick-quoted *syntactic* callee resolves to
    /// the bare current-file definition (the raw-AST-text match canonicalizes
    /// both operands).
    #[test]
    fn backtick_quoted_syntactic_callee_resolves_current_file_signature() {
        let mut state = WorldState::new();
        let uri = Url::parse("file:///bt.R").unwrap();
        let code = "f <- function(alpha, beta) alpha\n`f`(\n";
        state
            .documents
            .insert(uri.clone(), crate::state::Document::new(code, None));
        let sig = resolve_user_only(
            &state,
            "`f`",
            &uri,
            tower_lsp::lsp_types::Position::new(1, 3),
        )
        .expect("backtick-quoted syntactic callee must resolve to the bare def");
        let names: Vec<String> = sig.parameters.into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    /// A genuinely non-syntactic backtick local definition is matched by a
    /// backtick-quoted call (canonicalizing both operands keeps the required
    /// backticks, so the backticked def still matches).
    #[test]
    fn backtick_quoted_nonsyntactic_callee_resolves_current_file_signature() {
        let mut state = WorldState::new();
        let uri = Url::parse("file:///bt2.R").unwrap();
        let code = "`my fn` <- function(alpha) alpha\n`my fn`(\n";
        state
            .documents
            .insert(uri.clone(), crate::state::Document::new(code, None));
        let sig = resolve_user_only(
            &state,
            "`my fn`",
            &uri,
            tower_lsp::lsp_types::Position::new(1, 8),
        )
        .expect("non-syntactic backtick local must still resolve");
        let names: Vec<String> = sig.parameters.into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["alpha"]);
    }

    /// Issue #459 Phase-2 cross-file fallback (CC1/TC2): a redundantly
    /// backtick-quoted callee `` `my_func`( `` whose definition lives in a
    /// SEPARATE sourced file as a BARE `<-` def resolves its signature through
    /// the cross-file scope. Phase 1 (current file) misses — the def isn't here
    /// — so resolution reaches `resolve_from_cross_file`. Cross-file scope
    /// stores the def under the bare `my_func` (Seam A strips backticks
    /// unconditionally), so the raw `` `my_func` `` lookup misses and the
    /// `lookup_key` (unconditionally unquoted) `.or_else` fallback resolves the
    /// symbol. The def-node match in `find_function_definition_at_line` then
    /// also canonicalizes both operands, so the RAW `` `my_func` `` matches the
    /// bare `my_func <-` def node. Writing the lib def BARE (not backticked) is
    /// load-bearing: a backticked def would coincidentally match on raw equality
    /// and mask the at-line canonicalization bug.
    #[test]
    fn backtick_quoted_callee_resolves_cross_file_signature_via_lookup_key() {
        use crate::state::Document;
        use std::sync::Arc;
        use std::time::SystemTime;

        let mut state = WorldState::new();
        state.workspace_scan_complete = true;

        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let lib_uri = Url::parse("file:///workspace/lib.R").unwrap();

        let main_code = "source(\"lib.R\")\n`my_func`(\n";
        let lib_code = "my_func <- function(alpha, beta) alpha\n";

        // Current file: a plain open document.
        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));

        // Sourced file: indexed with artifacts/metadata so the cross-file scope
        // resolver can read its symbols.
        let lib_doc = Document::new_with_uri(lib_code, None, &lib_uri);
        let lib_metadata = Arc::new(crate::cross_file::extract_metadata(lib_code));
        let lib_artifacts = Arc::new(crate::cross_file::scope::compute_artifacts_with_metadata(
            &lib_uri,
            lib_doc.tree.as_ref().expect("lib.R parses"),
            lib_code,
            Some(&lib_metadata),
        ));
        let entry = crate::workspace_index::IndexEntry {
            contents: lib_doc.contents.clone(),
            tree: lib_doc.tree.clone(),
            loaded_packages: lib_doc.loaded_packages.clone(),
            data_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: SystemTime::UNIX_EPOCH,
                size: lib_code.len() as u64,
                content_hash: None,
            },
            metadata: lib_metadata,
            artifacts: lib_artifacts,
            indexed_at_version: state.workspace_index.version(),
        };
        assert!(state.workspace_index.insert(lib_uri.clone(), entry));

        // Build the dependency edge main.R -> lib.R via `source("lib.R")`.
        for (uri, code) in [(&main_uri, main_code), (&lib_uri, lib_code)] {
            state.cross_file_graph.update_file(
                uri,
                &crate::cross_file::extract_metadata(code),
                None,
                |_| None,
            );
        }

        // At the backticked call on line 1, Phase 1 misses (the def is in
        // lib.R) and the Phase-2 `lookup_key` fallback resolves the bare-stored
        // cross-file signature.
        let sig = resolve_user_only(
            &state,
            "`my_func`",
            &main_uri,
            tower_lsp::lsp_types::Position::new(1, 3),
        )
        .expect("backticked cross-file callee must resolve via the Phase-2 lookup_key fallback");
        assert!(
            matches!(sig.source, SignatureSource::CrossFile { .. }),
            "must resolve from the sourced file (Phase 2), not the current file"
        );
        let names: Vec<String> = sig.parameters.into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    /// Issue #459 coverage (TC1): the RIGHT-assignment branch of the *at-line*
    /// cross-file lookup `find_func_def_at_line_recursive` (`(function(...)) ->
    /// name`). A redundantly backtick-quoted callee `` `my_func`( `` whose
    /// definition lives in a SEPARATE sourced file as a BARE `->` def must
    /// resolve through the Phase-2 cross-file path. The only other cross-file
    /// signature test uses `<-`, and both current-file `->` tests reach
    /// `find_func_def_recursive`, not the at-line branch — so this is the only
    /// test that reaches the at-line `->` branch with a bare def, where both
    /// equality operands must be canonicalized for the bare def node to match
    /// the backticked call name.
    #[test]
    fn backtick_quoted_callee_resolves_cross_file_right_assignment_bare_def() {
        use crate::state::Document;
        use std::sync::Arc;
        use std::time::SystemTime;

        let mut state = WorldState::new();
        state.workspace_scan_complete = true;

        let main_uri = Url::parse("file:///workspace/main.R").unwrap();
        let lib_uri = Url::parse("file:///workspace/lib.R").unwrap();

        let main_code = "source(\"lib.R\")\n`my_func`(\n";
        let lib_code = "(function(alpha, beta) alpha) -> my_func\n";

        state
            .documents
            .insert(main_uri.clone(), Document::new(main_code, None));

        let lib_doc = Document::new_with_uri(lib_code, None, &lib_uri);
        let lib_metadata = Arc::new(crate::cross_file::extract_metadata(lib_code));
        let lib_artifacts = Arc::new(crate::cross_file::scope::compute_artifacts_with_metadata(
            &lib_uri,
            lib_doc.tree.as_ref().expect("lib.R parses"),
            lib_code,
            Some(&lib_metadata),
        ));
        let entry = crate::workspace_index::IndexEntry {
            contents: lib_doc.contents.clone(),
            tree: lib_doc.tree.clone(),
            loaded_packages: lib_doc.loaded_packages.clone(),
            data_packages: vec![],
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: SystemTime::UNIX_EPOCH,
                size: lib_code.len() as u64,
                content_hash: None,
            },
            metadata: lib_metadata,
            artifacts: lib_artifacts,
            indexed_at_version: state.workspace_index.version(),
        };
        assert!(state.workspace_index.insert(lib_uri.clone(), entry));

        for (uri, code) in [(&main_uri, main_code), (&lib_uri, lib_code)] {
            state.cross_file_graph.update_file(
                uri,
                &crate::cross_file::extract_metadata(code),
                None,
                |_| None,
            );
        }

        let sig = resolve_user_only(
            &state,
            "`my_func`",
            &main_uri,
            tower_lsp::lsp_types::Position::new(1, 3),
        )
        .expect("backticked cross-file `->` callee must resolve via the Phase-2 at-line path");
        assert!(
            matches!(sig.source, SignatureSource::CrossFile { .. }),
            "must resolve from the sourced file (Phase 2), not the current file"
        );
        let names: Vec<String> = sig.parameters.into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    /// Issue #459 coverage: the RIGHT-assignment branch of
    /// `find_func_def_recursive` (`function(...) -> name`) canonicalizes both
    /// the stored name and the call name, so a redundantly backtick-quoted
    /// syntactic callee `` `f`( `` resolves to a `-> `f`` definition. No
    /// existing test exercised the `->` branch, so this pins it.
    #[test]
    fn backtick_quoted_syntactic_callee_resolves_right_assignment_signature() {
        let mut state = WorldState::new();
        let uri = Url::parse("file:///rassign.R").unwrap();
        let code = "(function(alpha, beta) alpha) -> `f`\n`f`(\n";
        state
            .documents
            .insert(uri.clone(), crate::state::Document::new(code, None));
        let sig = resolve_user_only(
            &state,
            "`f`",
            &uri,
            tower_lsp::lsp_types::Position::new(1, 3),
        )
        .expect("backtick-quoted syntactic callee must resolve to the `->` def");
        let names: Vec<String> = sig.parameters.into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    /// A genuinely non-syntactic right-assignment definition (`-> `my fn``) is
    /// matched by a backtick-quoted call (canonicalizing both operands keeps the
    /// required backticks, so the backticked def still matches).
    #[test]
    fn backtick_quoted_nonsyntactic_callee_resolves_right_assignment_signature() {
        let mut state = WorldState::new();
        let uri = Url::parse("file:///rassign2.R").unwrap();
        let code = "(function(alpha, beta) alpha) -> `my fn`\n`my fn`(\n";
        state
            .documents
            .insert(uri.clone(), crate::state::Document::new(code, None));
        let sig = resolve_user_only(
            &state,
            "`my fn`",
            &uri,
            tower_lsp::lsp_types::Position::new(1, 8),
        )
        .expect("non-syntactic right-assignment local must still resolve");
        let names: Vec<String> = sig.parameters.into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    // -- Comment between assignment and function definition --

    #[test]
    fn test_find_func_def_with_comment_between_arrow_and_function() {
        // A comment between `<-` and `function(...)` shifts child indices
        // when extras (comments) are not filtered out.
        let code = "f <-\n  # documentation comment\n  function(x, y) { x + y }\nf(1)";
        let tree = parse_r_code(code);

        let pos = tower_lsp::lsp_types::Position::new(3, 0);
        let func_node = find_function_definition_before_position(tree.root_node(), "f", code, pos);
        assert!(
            func_node.is_some(),
            "Should find function definition even with comment between <- and function()"
        );

        // Verify parameters are correctly extracted
        let func_node = func_node.unwrap();
        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .unwrap();
        let params = extract_from_ast(params_node, code);
        assert_eq!(params.len(), 2, "Should find both parameters x and y");
        assert_eq!(params[0].name, "x");
        assert_eq!(params[1].name, "y");
    }

    #[test]
    fn test_find_func_def_at_line_with_comment_between_arrow_and_function() {
        // Same scenario but for the cross-file at-line lookup
        let code = "f <-\n  # doc\n  function(x) { x }";
        let tree = parse_r_code(code);

        // The identifier `f` is at line 0
        let func_node = find_function_definition_at_line(tree.root_node(), "f", code, 0);
        assert!(
            func_node.is_some(),
            "find_function_definition_at_line should handle comments between <- and function()"
        );

        let func_node = func_node.unwrap();
        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .unwrap();
        let params = extract_from_ast(params_node, code);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "x");
    }

    #[test]
    fn test_find_func_def_multiple_comments_between_parts() {
        // Multiple comments between parts of the assignment
        let code = "g <-\n  # first comment\n  # second comment\n  function(p, q, r) { p }\ng(1)";
        let tree = parse_r_code(code);

        let pos = tower_lsp::lsp_types::Position::new(4, 0);
        let func_node = find_function_definition_before_position(tree.root_node(), "g", code, pos);
        assert!(
            func_node.is_some(),
            "Should find function definition with multiple comments between <- and function()"
        );

        let func_node = func_node.unwrap();
        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .unwrap();
        let params = extract_from_ast(params_node, code);
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].name, "p");
        assert_eq!(params[1].name, "q");
        assert_eq!(params[2].name, "r");
    }

    // -- SignatureCache Debug --

    #[test]
    fn test_cache_debug_format() {
        let cache = SignatureCache::new(10);
        let debug_str = format!("{:?}", cache);
        assert!(debug_str.contains("SignatureCache"));
    }

    use super::test_utils::find_function_def_in_tree;
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    /// Parse R code using a fresh parser (test-only).
    fn parse_r_code(code: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        let language = tree_sitter_r::LANGUAGE;
        parser
            .set_language(&language.into())
            .expect("Error loading R grammar");
        parser.parse(code, None).expect("Error parsing R code")
    }

    /// Strategy to generate valid R parameter names.
    /// R identifiers start with a letter or `.` (if followed by non-digit),
    /// and can contain letters, digits, `.`, and `_`.
    /// We keep it simple: start with a lowercase letter, then alphanumeric/dot/underscore.
    fn r_param_name() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9._]{0,7}"
            .prop_filter("must not be empty", |s| !s.is_empty())
            .prop_filter("must not be a reserved word", |s| {
                !matches!(
                    s.as_str(),
                    "if" | "else"
                        | "for"
                        | "in"
                        | "while"
                        | "repeat"
                        | "function"
                        | "return"
                        | "next"
                        | "break"
                        | "TRUE"
                        | "FALSE"
                        | "NULL"
                        | "Inf"
                        | "NaN"
                        | "NA"
                )
            })
    }

    /// Strategy to generate a simple R default value expression.
    fn r_default_value() -> impl Strategy<Value = String> {
        prop_oneof![
            // Numeric literals
            (1..1000i32).prop_map(|n| n.to_string()),
            // String literals
            "[a-z]{1,5}".prop_map(|s| format!("\"{}\"", s)),
            // Boolean/NULL
            Just("TRUE".to_string()),
            Just("FALSE".to_string()),
            Just("NULL".to_string()),
        ]
    }

    /// Strategy to generate a single parameter definition (name with optional default).
    fn r_param_def() -> impl Strategy<Value = (String, Option<String>)> {
        r_param_name().prop_flat_map(|name| {
            let name_clone = name.clone();
            prop_oneof![
                // No default value
                Just((name.clone(), None)),
                // With a default value
                r_default_value().prop_map(move |val| (name_clone.clone(), Some(val))),
            ]
        })
    }

    /// Strategy to generate a list of unique parameter definitions (0-6 params).
    fn r_param_list(
        min: usize,
        max: usize,
    ) -> impl Strategy<Value = Vec<(String, Option<String>)>> {
        prop::collection::vec(r_param_def(), min..=max).prop_filter(
            "parameter names must be unique",
            |params| {
                let mut seen = std::collections::HashSet::new();
                params.iter().all(|(name, _)| seen.insert(name.clone()))
            },
        )
    }

    /// Build an R function definition string from parameter definitions.
    fn build_function_code(
        func_name: &str,
        params: &[(String, Option<String>)],
        include_dots: bool,
    ) -> String {
        let mut param_strs: Vec<String> = params
            .iter()
            .map(|(name, default)| match default {
                Some(val) => format!("{} = {}", name, val),
                None => name.clone(),
            })
            .collect();

        if include_dots {
            param_strs.push("...".to_string());
        }

        format!(
            "{} <- function({}) {{ NULL }}",
            func_name,
            param_strs.join(", ")
        )
    }

    use super::test_utils::find_function_def_in_tree;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // ============================================================================
        // Feature: function-parameter-completions, Property 3: Parameter Extraction Round-Trip
        //
        // For any user-defined R function with parameters, extracting parameters
        // from the AST SHALL produce parameter names that match the original
        // formal parameter names in declaration order.
        //
        // **Validates: Requirements 4.1**
        // ============================================================================

        /// Generate R function definitions with varying parameter counts and defaults;
        /// verify extracted parameter names match original formal parameter names in
        /// declaration order, and default values are correctly extracted when present.
        #[test]
        fn prop_parameter_extraction_round_trip(
            params in r_param_list(0, 6),
            include_dots in proptest::bool::ANY,
        ) {
            let code = build_function_code("test_fn", &params, include_dots);

            let tree = parse_r_code(&code);
            let func_node = find_function_def_in_tree(tree.root_node(), "test_fn", &code);
            prop_assert!(
                func_node.is_some(),
                "Failed to find function definition in code: {}",
                code
            );
            let func_node = func_node.unwrap();

            let params_node = func_node
                .children(&mut func_node.walk())
                .find(|c| c.kind() == "parameters");
            prop_assert!(
                params_node.is_some(),
                "Failed to find parameters node in code: {}",
                code
            );
            let params_node = params_node.unwrap();

            let extracted = extract_from_ast(params_node, &code);

            // Expected count: original params + dots (if included)
            let expected_count = params.len() + if include_dots { 1 } else { 0 };
            prop_assert_eq!(
                extracted.len(),
                expected_count,
                "Parameter count mismatch for code: {} (expected {}, got {})",
                code,
                expected_count,
                extracted.len()
            );

            // Verify each original parameter name and default in declaration order
            for (i, (expected_name, expected_default)) in params.iter().enumerate() {
                prop_assert_eq!(
                    &extracted[i].name,
                    expected_name,
                    "Parameter name mismatch at index {} in code: {}",
                    i,
                    code
                );
                prop_assert!(
                    !extracted[i].is_dots,
                    "Parameter at index {} should not be dots in code: {}",
                    i,
                    code
                );

                // Verify default value round-trip
                match expected_default {
                    Some(val) => {
                        prop_assert_eq!(
                            extracted[i].default_value.as_deref(),
                            Some(val.as_str()),
                            "Default value mismatch at index {} in code: {}",
                            i,
                            code
                        );
                    }
                    None => {
                        prop_assert!(
                            extracted[i].default_value.is_none(),
                            "Expected no default at index {} but got {:?} in code: {}",
                            i,
                            extracted[i].default_value,
                            code
                        );
                    }
                }
            }

            // Verify dots parameter if included
            if include_dots {
                let dots_idx = params.len();
                prop_assert_eq!(
                    &extracted[dots_idx].name,
                    "...",
                    "Dots parameter name mismatch in code: {}",
                    code
                );
                prop_assert!(
                    extracted[dots_idx].is_dots,
                    "Dots parameter should have is_dots=true in code: {}",
                    code
                );
                prop_assert!(
                    extracted[dots_idx].default_value.is_none(),
                    "Dots parameter should have no default value in code: {}",
                    code
                );
            }
        }

        // ============================================================================
        // Feature: function-parameter-completions, Property 5: Dots Parameter Inclusion
        //
        // For any function with a `...` (dots) parameter, the parameter completions
        // SHALL include `...` as a completion item.
        //
        // **Validates: Requirements 5.5**
        // ============================================================================

        /// Generate R functions with `...` parameter at various positions (beginning,
        /// middle, end of parameter list); verify dots is included in parameter
        /// completions with `is_dots = true`, name `"..."`, and no default value.
        #[test]
        fn prop_dots_parameter_inclusion(
            params in r_param_list(0, 5),
            // Position where dots is inserted: 0 = beginning, params.len() = end
            dots_position_ratio in 0.0..=1.0f64,
        ) {
            // Always include dots — compute insertion position from ratio
            let dots_pos = if params.is_empty() {
                0
            } else {
                let pos = (dots_position_ratio * (params.len() as f64)).floor() as usize;
                pos.min(params.len())
            };

            // Build parameter strings with dots inserted at the computed position
            let mut param_strs: Vec<String> = params
                .iter()
                .map(|(name, default)| match default {
                    Some(val) => format!("{} = {}", name, val),
                    None => name.clone(),
                })
                .collect();
            param_strs.insert(dots_pos, "...".to_string());

            let code = format!(
                "test_fn <- function({}) {{ NULL }}",
                param_strs.join(", ")
            );

            let tree = parse_r_code(&code);
            let func_node = find_function_def_in_tree(tree.root_node(), "test_fn", &code);
            prop_assert!(
                func_node.is_some(),
                "Failed to find function definition in code: {}",
                code
            );
            let func_node = func_node.unwrap();

            let params_node = func_node
                .children(&mut func_node.walk())
                .find(|c| c.kind() == "parameters");
            prop_assert!(
                params_node.is_some(),
                "Failed to find parameters node in code: {}",
                code
            );
            let params_node = params_node.unwrap();

            let extracted = extract_from_ast(params_node, &code);

            // Total count: original params + 1 for dots
            let expected_count = params.len() + 1;
            prop_assert_eq!(
                extracted.len(),
                expected_count,
                "Parameter count mismatch for code: {} (expected {}, got {})",
                code,
                expected_count,
                extracted.len()
            );

            // Verify dots is present at the correct position
            let dots_param = &extracted[dots_pos];
            prop_assert_eq!(
                &dots_param.name,
                "...",
                "Dots parameter at position {} should have name '...' in code: {}",
                dots_pos,
                code
            );
            prop_assert!(
                dots_param.is_dots,
                "Dots parameter at position {} should have is_dots=true in code: {}",
                dots_pos,
                code
            );
            prop_assert!(
                dots_param.default_value.is_none(),
                "Dots parameter at position {} should have no default value in code: {}",
                dots_pos,
                code
            );

            // Verify non-dots parameters are correct and in order
            let mut param_idx = 0;
            for (i, extracted_param) in extracted.iter().enumerate() {
                if i == dots_pos {
                    // Already verified dots above
                    continue;
                }
                prop_assert!(
                    param_idx < params.len(),
                    "More non-dots params than expected at index {} in code: {}",
                    i,
                    code
                );
                let (expected_name, _) = &params[param_idx];
                prop_assert_eq!(
                    &extracted_param.name,
                    expected_name,
                    "Non-dots parameter name mismatch at extracted index {}, param index {} in code: {}",
                    i,
                    param_idx,
                    code
                );
                prop_assert!(
                    !extracted_param.is_dots,
                    "Non-dots parameter at index {} should not have is_dots=true in code: {}",
                    i,
                    code
                );
                param_idx += 1;
            }

            // Also verify dots is findable via iteration (simulating completion list search)
            let dots_found = extracted.iter().any(|p| p.is_dots && p.name == "...");
            prop_assert!(
                dots_found,
                "Dots parameter should be findable in extracted params for code: {}",
                code
            );
        }

        // ============================================================================
        // Feature: function-parameter-completions, Property 10: Cache Consistency
        //
        // For any function signature inserted into the cache, subsequent lookups
        // with the same key SHALL return the cached signature without invoking
        // R subprocess.
        //
        // **Validates: Requirements 2.5, 3.5**
        // ============================================================================

        /// Insert a signature into the package cache, then look it up; verify the
        /// cached signature is returned with all fields preserved (parameter
        /// names, default values, is_dots flags).
        #[test]
        fn prop_cache_consistency(
            func_name in "[a-z][a-z0-9._]{0,7}",
            params in r_param_list(0, 6),
            include_dots in proptest::bool::ANY,
        ) {
            // Build parameter list from generated params
            let mut parameters: Vec<ParameterInfo> = params
                .iter()
                .map(|(name, default)| ParameterInfo {
                    name: name.clone(),
                    default_value: default.clone(),
                    is_dots: false,
                })
                .collect();

            if include_dots {
                parameters.push(ParameterInfo {
                    name: "...".to_string(),
                    default_value: None,
                    is_dots: true,
                });
            }

            // Build the signature to insert
            let signature = FunctionSignature {
                name: func_name.clone(),
                parameters: parameters.clone(),
                source: SignatureSource::RSubprocess {
                    package: Some("testpkg".to_string()),
                },
            };

            // Create a fresh cache for each test case
            let cache = SignatureCache::new(500);

            // Build the cache key
            let key = format!("testpkg::{}", func_name);

            // Verify the key is not in cache before insertion
            let before = cache.get_package(&key);
            prop_assert!(
                before.is_none(),
                "Cache should be empty before insertion for key: {}",
                key
            );

            // Insert the signature into the cache
            cache.insert_package(key.clone(), signature.clone());

            // Look up the signature from cache
            let cached = cache.get_package(&key);

            // Verify the cached signature is returned (not None)
            prop_assert!(
                cached.is_some(),
                "Cache lookup should return Some after insertion for key: {}",
                key
            );
            let cached = cached.unwrap();

            // Verify function name is preserved
            prop_assert_eq!(
                &cached.name,
                &func_name,
                "Cached function name mismatch for key: {}",
                key
            );

            // Verify parameter count is preserved
            prop_assert_eq!(
                cached.parameters.len(),
                parameters.len(),
                "Cached parameter count mismatch for key: {} (expected {}, got {})",
                key,
                parameters.len(),
                cached.parameters.len()
            );

            // Verify each parameter's fields are preserved
            for (i, (expected, actual)) in parameters.iter().zip(cached.parameters.iter()).enumerate() {
                prop_assert_eq!(
                    &actual.name,
                    &expected.name,
                    "Parameter name mismatch at index {} for key: {}",
                    i,
                    key
                );
                prop_assert_eq!(
                    &actual.default_value,
                    &expected.default_value,
                    "Default value mismatch at index {} for key: {}",
                    i,
                    key
                );
                prop_assert_eq!(
                    actual.is_dots,
                    expected.is_dots,
                    "is_dots mismatch at index {} for key: {}",
                    i,
                    key
                );
            }

            // Verify a second lookup also returns the same signature (cache is stable)
            let second_lookup = cache.get_package(&key);
            prop_assert!(
                second_lookup.is_some(),
                "Second cache lookup should also return Some for key: {}",
                key
            );
            let second = second_lookup.unwrap();
            prop_assert_eq!(
                second.parameters.len(),
                parameters.len(),
                "Second lookup parameter count mismatch for key: {}",
                key
            );
        }

        // ============================================================================
        // Property: Extras (comments) between assignment operator and function()
        //
        // For any function definition with optional comments placed between the
        // assignment operator and the `function(...)` keyword,
        // `find_function_definition_before_position` SHALL still find the function.
        // ============================================================================

        /// Generate R code with function definitions that may or may not have
        /// comments between `<-` and `function(...)`. Verify the function is
        /// always found regardless of comment placement.
        #[test]
        fn prop_extras_filtering_find_func_def(
            params in r_param_list(1, 4),
            insert_comment in proptest::bool::ANY,
            comment_text in "[a-z ]{1,20}",
        ) {
            let param_strs: Vec<String> = params
                .iter()
                .map(|(name, default)| match default {
                    Some(val) => format!("{} = {}", name, val),
                    None => name.clone(),
                })
                .collect();

            let func_body = format!("function({}) {{ NULL }}", param_strs.join(", "));
            let code = if insert_comment {
                format!("test_fn <-\n  # {}\n  {}\ntest_fn(1)", comment_text, func_body)
            } else {
                format!("test_fn <- {}\ntest_fn(1)", func_body)
            };

            let tree = parse_r_code(&code);

            // Compute call site line
            let call_line = code.lines().count() as u32 - 1;
            let pos = tower_lsp::lsp_types::Position::new(call_line, 0);

            let func_node = find_function_definition_before_position(
                tree.root_node(),
                "test_fn",
                &code,
                pos,
            );

            prop_assert!(
                func_node.is_some(),
                "Should find function definition regardless of comment. insert_comment={}, code:\n{}",
                insert_comment,
                code
            );

            // Verify parameter count matches
            let func_node = func_node.unwrap();
            let params_node = func_node
                .children(&mut func_node.walk())
                .find(|c| c.kind() == "parameters");
            prop_assert!(
                params_node.is_some(),
                "Should find parameters node. code:\n{}",
                code
            );

            let extracted = extract_from_ast(params_node.unwrap(), &code);
            prop_assert_eq!(
                extracted.len(),
                params.len(),
                "Parameter count mismatch. Expected {}, got {}. code:\n{}",
                params.len(),
                extracted.len(),
                code
            );

            // Verify parameter names match
            for (i, (expected_name, _)) in params.iter().enumerate() {
                prop_assert_eq!(
                    &extracted[i].name,
                    expected_name,
                    "Parameter name mismatch at index {}. code:\n{}",
                    i,
                    code
                );
            }
        }
    }

    // -- get_text_and_tree raw/masked pairing (#343, final adversarial review) --

    /// Rmd source whose chunk defines `myfun`, preceded by MULTIBYTE prose.
    ///
    /// `mask_to_r` blanks the prose lines but preserves line count, so the
    /// masked analysis text is SHORTER (in bytes) than the raw source. The
    /// chunk-defined function's byte offsets in the masked tree therefore land
    /// at *different* byte positions than the same text in the raw source — and
    /// the raw source's multibyte prose means those offsets can fall mid-char,
    /// panicking `&text[start..end]`. Pairing the masked tree with the raw text
    /// is the regressed behavior this test locks out.
    const MULTIBYTE_RMD: &str = "Prélude éééé éééé éééé\n\n```{r}\nmyfun <- function(alpha, beta) {\n  alpha + beta\n}\n```\n";

    /// Mirror of `resolve_from_cross_file`'s post-`get_text_and_tree` slicing:
    /// locate `myfun`'s definition at its known line and extract its formals.
    /// Returns the parameter names. Panics if the (text, tree) pair mis-aligns
    /// on a char boundary — exactly the regression under test.
    fn extract_params_via_get_text_and_tree(state: &WorldState, uri: &Url) -> Vec<String> {
        let (text, tree) =
            get_text_and_tree(state, uri).expect("get_text_and_tree must find the document");
        // `myfun` is defined on document line 3 (0-based) in MULTIBYTE_RMD.
        let func_node = find_function_definition_at_line(tree.root_node(), "myfun", &text, 3)
            .expect("function definition must be found at line 3");
        let params_node = func_node
            .children(&mut func_node.walk())
            .find(|c| c.kind() == "parameters")
            .expect("function must have a parameters node");
        extract_from_ast(params_node, &text)
            .into_iter()
            .map(|p| p.name)
            .collect()
    }

    #[tokio::test]
    async fn get_text_and_tree_pairs_masked_tree_with_masked_text_open_record() {
        // Open-record arm (step 1): an open `.Rmd` whose chunk defines a
        // function, reached via cross-file resolution. Before the fix this
        // returned RAW text paired with the masked tree, panicking on the
        // multibyte prose (or silently mis-slicing for ASCII prose).
        let uri = Url::parse("file:///doc.Rmd").unwrap();
        let mut state = WorldState::new();
        state.open_document_with_language_id(uri.clone(), MULTIBYTE_RMD, Some(1), Some("rmd"));

        let params = extract_params_via_get_text_and_tree(&state, &uri);
        assert_eq!(
            params,
            vec!["alpha".to_string(), "beta".to_string()],
            "chunk-defined function's formals must extract from the masked analysis text"
        );
    }

    #[tokio::test]
    async fn get_text_and_tree_pairs_masked_tree_with_masked_text_workspace_index_arm() {
        // workspace_index arm (step 3): an on-demand-indexed (closed) `.Rmd`
        // pairs a masked `tree` with RAW `contents`. Mirror that construction
        // and confirm `get_text_and_tree` returns the masked analysis text so
        // the tree's byte offsets align.
        use crate::cross_file::file_cache::FileSnapshot;

        let uri = Url::parse("file:///closed.Rmd").unwrap();
        let analysis = crate::cross_file::analysis_text_for_path(uri.path(), MULTIBYTE_RMD);
        let tree = crate::parser_pool::with_parser(|p| p.parse(analysis.as_ref(), None));
        let metadata = std::sync::Arc::new(crate::cross_file::extract_metadata_with_tree(
            &analysis,
            tree.as_ref(),
        ));
        let artifacts = std::sync::Arc::new(match tree.as_ref() {
            Some(tree) => crate::cross_file::scope::compute_artifacts_with_metadata(
                &uri,
                tree,
                &analysis,
                Some(&metadata),
            ),
            None => crate::cross_file::scope::ScopeArtifacts::default(),
        });
        let entry = crate::workspace_index::IndexEntry {
            // RAW contents, as on-demand indexing stores them (#343).
            contents: ropey::Rope::from_str(MULTIBYTE_RMD),
            tree,
            loaded_packages: Vec::new(),
            data_packages: vec![],
            snapshot: FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: MULTIBYTE_RMD.len() as u64,
                content_hash: None,
            },
            metadata,
            artifacts,
            indexed_at_version: 0,
        };
        let state = WorldState::new();
        state.workspace_index.insert(uri.clone(), entry);

        let params = extract_params_via_get_text_and_tree(&state, &uri);
        assert_eq!(
            params,
            vec!["alpha".to_string(), "beta".to_string()],
            "chunk-defined function's formals must extract from the masked analysis text"
        );
    }
}
