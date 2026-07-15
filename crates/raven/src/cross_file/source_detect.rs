//
// cross_file/source_detect.rs
//
// Detection of source() and sys.source() calls using tree-sitter
// Detection of rm() and remove() calls for scope tracking
// Detection of library(), require(), loadNamespace() calls for package awareness
//

use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Tree};

use super::scope::FunctionScopeInterval;
use super::types::{ForwardSource, byte_offset_to_utf16_column};

/// A statically-extracted `system.file(...)` call used as the path argument
/// to `source()`. Contains the string-literal positional parts and the
/// `package = "P"` value needed to resolve the path at analysis time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SystemFileCall {
    /// Positional string-literal parts (joined with `/` to form the relative path).
    pub parts: Vec<String>,
    /// The `package` argument value (must be a string literal).
    pub package: String,
}

/// Detected rm()/remove() call with extracted symbol names
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RmCall {
    /// 0-based line of the rm() call
    pub line: u32,
    /// 0-based UTF-16 column
    pub column: u32,
    /// Symbol names to remove
    pub symbols: Vec<String>,
}

/// Detected `exists("name")` call with the statically-extracted name.
///
/// An `exists("name")` call is a runtime existence probe, but a user who writes
/// it is asserting that `name` is a binding they know about. Raven therefore
/// treats it as a variable declaration equivalent to `# raven: var name` (see
/// `compute_artifacts*` in `scope.rs`, which turns each into a
/// `ScopeEvent::Declaration`). Only string-literal names are captured — a
/// non-literal first argument (`exists(varname)`, `exists(paste0(...))`) is not
/// statically determinable and is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistsCall {
    /// The raw symbol name (string-literal contents, without quotes). The
    /// caller re-applies call-site canonicalization (backtick-wrapping a
    /// non-syntactic name) when synthesizing the declared symbol, mirroring the
    /// `# raven: var` path.
    pub name: String,
    /// 0-based line of the `exists()` call.
    pub line: u32,
}

/// Detected library/require/loadNamespace call
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryCall {
    /// Package name (if statically determinable)
    pub package: String,
    /// 0-based line of the call
    pub line: u32,
    /// 0-based UTF-16 column of the call end position
    pub column: u32,
    /// Whether this is inside a function scope
    pub function_scope: Option<FunctionScopeInterval>,
    /// Whether the call *attaches* the package to the search path so its exports
    /// become available as bare names: `true` for `library()` / `require()`,
    /// `false` for `loadNamespace()` (which loads the namespace for qualified
    /// `pkg::fn` access only). Packages recognized from a {targets}
    /// `tar_option_set(packages = ...)` call are also `true` — targets attaches
    /// them before running each target, so downstream scripts use bare names.
    ///
    /// Consumers must require `attaches` only when they gate on a *bare function*
    /// being resolvable — e.g. Shiny deferred-helper recognition
    /// (`push_shiny_deferred_scopes`), where `reactive`/`render*` are exported
    /// functions called by bare name and so need an attach. Surfaces that
    /// dispatch on the object or namespace rather than a bare name (e.g.
    /// data.table's S3 `[.data.table` via `collect_in_play_packages`) are enabled
    /// by a merely-loaded namespace and must *not* gate on `attaches`.
    ///
    /// Defaults to `false` for forward compatibility on deserialize.
    #[serde(default)]
    pub attaches: bool,
}

/// A source range in LSP coordinates: 0-based line, **UTF-16** column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRange {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// The member (RHS) token of a `pkg::member` / `pkg:::member` reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceMember {
    pub name: String,
    pub range: SourceRange,
}

/// A detected `pkg::member` / `pkg:::member` (or incomplete `pkg::`) reference.
///
/// INVARIANT: every member-diagnostic site MUST check `!internal` before using
/// `member` — `internal: true` is `:::`, which never gets member validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceReference {
    /// Package name, with any string/backtick delimiters stripped.
    pub package: String,
    /// Range of the package (LHS) token.
    pub package_range: SourceRange,
    /// Member (RHS) token; `None` for an incomplete `pkg::` with no RHS.
    pub member: Option<NamespaceMember>,
    /// `true` for `:::` (internal lookup), `false` for `::`.
    pub internal: bool,
    /// Range of the whole `pkg::member` expression.
    pub range: SourceRange,
}

/// Locate all top-level `source()` and `sys.source()` calls in an R syntax tree and extract their static parameters.
///
/// This function traverses the given tree-sitter `Tree` of R source code and collects each `source()`
/// or `sys.source()` call that has a statically determinable file path. For each detected call it
/// records the file path, the end-position line and UTF-16 column, whether the call is `sys.source`,
/// and the boolean `local` and `chdir` argument values when they are statically resolvable. Calls with
/// non-string or otherwise non-determinable file arguments are ignored.
///
/// # Returns
///
/// A `Vec<ForwardSource>` containing one entry per detected `source()`/`sys.source()` call in document
/// order, with fields populated for path, line, column, is_sys_source, local, chdir, and (for
/// `sys.source`) `sys_source_global_env`.
///
/// # Examples
///
/// ```
/// use raven::cross_file::source_detect::detect_source_calls;
///
/// let mut parser = tree_sitter::Parser::new();
/// parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
/// let source = "source('utils.R', local = TRUE)\n";
/// let tree = parser.parse(source, None).unwrap();
/// let sources = detect_source_calls(&tree, source);
/// assert_eq!(sources.len(), 1);
/// assert_eq!(sources[0].path, "utils.R");
/// assert!(sources[0].local);
/// ```
pub fn detect_source_calls(tree: &Tree, content: &str) -> Vec<ForwardSource> {
    log::trace!("Starting tree-sitter parsing for source() call detection");
    let mut sources = Vec::new();
    let root = tree.root_node();
    visit_node(root, content, &mut sources);
    log::trace!(
        "Completed source() call detection, found {} calls",
        sources.len()
    );
    for source in &sources {
        log::trace!(
            "  Detected source() call: path='{}' at line {} column {} (is_sys_source={}, local={}, chdir={})",
            source.path,
            source.line,
            source.column,
            source.is_sys_source,
            source.local,
            source.chdir
        );
    }
    sources
}

fn visit_node(node: Node, content: &str, sources: &mut Vec<ForwardSource>) {
    if node.kind() == "call"
        && let Some(source) = try_parse_source_call(node, content)
    {
        sources.push(source);
    }

    for child in node.children(&mut node.walk()) {
        visit_node(child, content, sources);
    }
}

fn try_parse_source_call(node: Node, content: &str) -> Option<ForwardSource> {
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);

    let is_sys_source = match func_text {
        "source" => false,
        "sys.source" => true,
        _ => return None,
    };

    let args_node = node.child_by_field_name("arguments")?;

    // Try normal string-literal path first
    let path = find_file_argument(&args_node, content);

    // If no string literal, try system.file() call in the path position
    let system_file = if path.is_none() {
        try_extract_system_file_argument(&args_node, content)
    } else {
        None
    };

    // Need either a path or a system.file() call
    if path.is_none() && system_file.is_none() {
        return None;
    }

    let local = find_bool_argument(&args_node, content, "local").unwrap_or(false);
    let chdir = find_bool_argument(&args_node, content, "chdir").unwrap_or(false);

    // For sys.source, check if envir is globalenv()/.GlobalEnv
    let sys_source_global_env = if is_sys_source {
        find_envir_is_global(&args_node, content)
    } else {
        true // Not sys.source, so this field doesn't matter
    };

    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);

    Some(ForwardSource {
        path: path.unwrap_or_default(),
        line: start.row as u32,
        column,
        is_directive: false,
        local,
        chdir,
        is_sys_source,
        sys_source_global_env,
        explicit_line: false,  // AST-detected sources never have explicit line=N
        directive_line: 0,     // Not applicable for AST-detected sources
        user_line_zero: false, // Not applicable for AST-detected sources
        is_function_scoped: has_function_definition_ancestor(node),
        system_file,
        resolved_uri: None,
    })
}

/// Walk up the AST from `node` to determine whether any ancestor is a
/// `function_definition`. Used to mark source() calls that only execute
/// when their enclosing function is invoked, so they are not load-time
/// ordering constraints. Tree-sitter normalizes R 4.1+ lambdas (`\(x) ...`)
/// into `function_definition` nodes too, so no separate handling is needed.
fn has_function_definition_ancestor(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "function_definition" {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Check if the envir argument is globalenv() or .GlobalEnv
fn find_envir_is_global(args_node: &Node, content: &str) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, content);
            if name == "envir"
                && let Some(value_node) = child.child_by_field_name("value")
            {
                let value = node_text(value_node, content).trim();
                // Check for globalenv() or .GlobalEnv
                return value == "globalenv()" || value == ".GlobalEnv";
            }
        }
    }
    // If envir is not specified, sys.source defaults to baseenv() which is NOT global
    // So we return false (conservative: no symbol inheritance)
    false
}

fn find_file_argument(args_node: &Node, content: &str) -> Option<String> {
    let mut cursor = args_node.walk();
    let children: Vec<_> = args_node.children(&mut cursor).collect();

    // Look for named "file" argument
    for child in &children {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, content);
            if name == "file"
                && let Some(value_node) = child.child_by_field_name("value")
            {
                return extract_string_literal(value_node, content);
            }
        }
    }

    // Use first positional argument
    for child in &children {
        if child.kind() == "argument"
            && child.child_by_field_name("name").is_none()
            && let Some(value_node) = child.child_by_field_name("value")
        {
            return extract_string_literal(value_node, content);
        }
    }

    None
}

/// Try to extract a `system.file(...)` call from the file-argument position of
/// a `source()` call. Returns `Some(SystemFileCall)` when the argument is a call
/// to `system.file` with all-string-literal positional parts and a literal
/// `package = "P"` named argument. Returns `None` for computed/variable args.
fn try_extract_system_file_argument(args_node: &Node, content: &str) -> Option<SystemFileCall> {
    let mut cursor = args_node.walk();
    let children: Vec<_> = args_node.children(&mut cursor).collect();

    // Look for named "file" argument first
    for child in &children {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, content);
            if name == "file"
                && let Some(value_node) = child.child_by_field_name("value")
            {
                return try_parse_system_file_call(value_node, content);
            }
        }
    }

    // Use first positional argument
    for child in &children {
        if child.kind() == "argument"
            && child.child_by_field_name("name").is_none()
            && let Some(value_node) = child.child_by_field_name("value")
        {
            return try_parse_system_file_call(value_node, content);
        }
    }

    None
}

/// Parse a `system.file(part1, part2, ..., package = "P")` call node.
/// Returns `Some(SystemFileCall)` when:
/// - The callee is `system.file`
/// - All positional arguments are string literals
/// - There is at least one positional string-literal part
/// - There is a named `package = "P"` argument with a string-literal value
fn try_parse_system_file_call(node: Node, content: &str) -> Option<SystemFileCall> {
    if node.kind() != "call" {
        return None;
    }
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);
    if func_text != "system.file" {
        return None;
    }

    let args_node = node.child_by_field_name("arguments")?;
    if args_node.has_error() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut package: Option<String> = None;

    let mut arg_cursor = args_node.walk();
    for child in args_node.children(&mut arg_cursor) {
        if child.kind() != "argument" {
            continue;
        }
        if let Some(name_node) = child.child_by_field_name("name") {
            let name = node_text(name_node, content);
            if name == "package" {
                let value_node = child.child_by_field_name("value")?;
                package = extract_string_literal(value_node, content);
                // Non-literal package arg → bail
                package.as_ref()?;
            }
            // lib.loc: accept when the value is a standard-library reference
            // (.Library, .Library.site, .libPaths()) — these resolve to the
            // default search path our resolver already uses. Reject otherwise.
            if name == "lib.loc" {
                let value_node = child.child_by_field_name("value")?;
                if !is_standard_lib_loc(value_node, content) {
                    return None;
                }
            }
            // fsep: the default is "/"; accept that (no-op), reject others.
            if name == "fsep" {
                let value_node = child.child_by_field_name("value")?;
                let text = node_text(value_node, content);
                if text != "\"/\"" && text != "'/'" {
                    return None;
                }
            }
            // Other named args (e.g. mustWork) don't affect path layout
        } else {
            // Positional argument — must be a string literal
            let value_node = child.child_by_field_name("value")?;
            let part = extract_string_literal(value_node, content)?;
            parts.push(part);
        }
    }

    // Must have at least one part and a package
    let package = package?;
    if parts.is_empty() {
        return None;
    }

    Some(SystemFileCall { parts, package })
}

fn find_bool_argument(args_node: &Node, content: &str, param_name: &str) -> Option<bool> {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, content);
            if name == param_name
                && let Some(value_node) = child.child_by_field_name("value")
            {
                let value = node_text(value_node, content);
                return match value {
                    "TRUE" | "T" => Some(true),
                    "FALSE" | "F" => Some(false),
                    _ => None,
                };
            }
        }
    }
    None
}

fn extract_string_literal(node: Node, content: &str) -> Option<String> {
    if node.kind() == "string" {
        let text = node_text(node, content);
        if (text.starts_with('"') && text.ends_with('"'))
            || (text.starts_with('\'') && text.ends_with('\''))
        {
            return Some(text[1..text.len() - 1].to_string());
        }
    }
    None
}

/// Returns true when a `lib.loc` value node refers to a standard library path
/// that our resolver already searches (`.Library`, `.Library.site`, or a call
/// to `.libPaths()`), or is `NULL` (identical to omitting `lib.loc`).
fn is_standard_lib_loc(node: Node, content: &str) -> bool {
    let text = node_text(node, content);
    if text == ".Library" || text == ".Library.site" || text == "NULL" {
        return true;
    }
    // .libPaths() — a call node whose function leaf is `.libPaths`
    if node.kind() == "call"
        && let Some(func) = node.child_by_field_name("function")
    {
        return node_text(func, content) == ".libPaths";
    }
    false
}

fn node_text<'a>(node: Node<'a>, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}

/// Detect rm() and remove() calls in R code.
/// Returns calls that should affect scope (excludes those with non-default envir=).
/// Extracts bare symbols from positional args and string-literal symbols from list=.
pub fn detect_rm_calls(tree: &Tree, content: &str) -> Vec<RmCall> {
    log::trace!("Starting tree-sitter parsing for rm() call detection");
    let mut rm_calls = Vec::new();
    let root = tree.root_node();
    visit_node_for_rm(root, content, &mut rm_calls);
    log::trace!(
        "Completed rm() call detection, found {} calls",
        rm_calls.len()
    );
    for rm_call in &rm_calls {
        log::trace!(
            "  Detected rm() call at line {} column {} with symbols: {:?}",
            rm_call.line,
            rm_call.column,
            rm_call.symbols
        );
    }
    rm_calls
}

fn visit_node_for_rm(node: Node, content: &str, rm_calls: &mut Vec<RmCall>) {
    if node.kind() == "identifier" {
        return;
    }
    if node.kind() == "call"
        && let Some(rm_call) = try_parse_rm_call(node, content)
    {
        // Only add if there are symbols to remove
        if !rm_call.symbols.is_empty() {
            rm_calls.push(rm_call);
        }
    }

    for child in node.children(&mut node.walk()) {
        visit_node_for_rm(child, content, rm_calls);
    }
}

fn try_parse_rm_call(node: Node, content: &str) -> Option<RmCall> {
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);

    // Check if this is rm() or remove()
    if func_text != "rm" && func_text != "remove" {
        return None;
    }
    if call_is_internal_routine_argument(node, content) {
        return None;
    }

    let args_node = node.child_by_field_name("arguments")?;

    // Skip if arguments contain error or missing nodes
    if args_node.has_error() {
        return None;
    }

    // Check if rm() has a non-default envir= argument
    // If envir= is present and NOT globalenv() or .GlobalEnv, skip this call
    if has_non_default_envir_for_rm(&args_node, content) {
        return None;
    }

    // Extract bare symbol arguments from positional args
    let mut symbols = extract_bare_symbols(&args_node, content);

    // Extract symbols from list= argument (string literals or c() calls)
    let list_symbols = extract_list_symbols(&args_node, content);
    symbols.extend(list_symbols);

    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);

    Some(RmCall {
        line: start.row as u32,
        column,
        symbols,
    })
}

/// True for the routine call nested as `.Internal(<routine>(...))`.
///
/// The nested call head names a hidden C entry point, not an R binding. For
/// rm/remove detection that means `.Internal(remove(list, envir, inherits))`
/// is the implementation of `rm()`/`remove()`, not a user-level removal call.
fn call_is_internal_routine_argument(call_node: Node, content: &str) -> bool {
    if call_node.kind() != "call" {
        return false;
    }
    let Some(arg) = call_node.parent().filter(|node| node.kind() == "argument") else {
        return false;
    };
    if arg.child_by_field_name("name").is_some() {
        return false;
    }
    if arg
        .child_by_field_name("value")
        .is_none_or(|value| value.id() != call_node.id())
    {
        return false;
    }
    let Some(args) = arg.parent().filter(|node| node.kind() == "arguments") else {
        return false;
    };
    if first_positional_arg_value(args).is_none_or(|value| value.id() != call_node.id()) {
        return false;
    }
    let Some(outer_call) = args.parent().filter(|node| node.kind() == "call") else {
        return false;
    };
    outer_call
        .child_by_field_name("function")
        .and_then(|func| callee_leaf_name(func, content))
        == Some(".Internal")
}

/// The leaf callee name of a call's `function` field.
fn callee_leaf_name<'t>(func: Node<'t>, content: &'t str) -> Option<&'t str> {
    match func.kind() {
        "identifier" => Some(node_text(func, content)),
        "namespace_operator" => func
            .child_by_field_name("rhs")
            .map(|rhs| node_text(rhs, content)),
        _ => None,
    }
}

/// The value node of a call's first positional argument, if any.
fn first_positional_arg_value(args: Node) -> Option<Node> {
    let mut cursor = args.walk();
    args.children(&mut cursor).find_map(|child| {
        (child.kind() == "argument" && child.child_by_field_name("name").is_none())
            .then(|| child.child_by_field_name("value"))
            .flatten()
    })
}

/// Check if rm() call has a non-default envir= argument.
/// Returns true if envir= is present and NOT globalenv() or .GlobalEnv.
/// Returns false if envir= is absent (default) or is globalenv()/.GlobalEnv.
fn has_non_default_envir_for_rm(args_node: &Node, content: &str) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, content);
            if name == "envir"
                && let Some(value_node) = child.child_by_field_name("value")
            {
                let value = node_text(value_node, content).trim();
                // Default-equivalent values: globalenv() or .GlobalEnv
                if value == "globalenv()" || value == ".GlobalEnv" {
                    return false;
                }
                // Any other value means non-default
                return true;
            }
        }
    }
    // No envir= argument means default (global environment)
    false
}

/// Extract bare symbol (identifier) arguments from positional args in rm()/remove() calls.
/// Only extracts identifiers from positional arguments (not named arguments).
fn extract_bare_symbols(args_node: &Node, content: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    let mut cursor = args_node.walk();

    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" {
            // Only process positional arguments (no name)
            if child.child_by_field_name("name").is_none()
                && let Some(value_node) = child.child_by_field_name("value")
            {
                // Only extract if it's an identifier (bare symbol)
                if value_node.kind() == "identifier" {
                    let symbol_name = node_text(value_node, content).to_string();
                    symbols.push(symbol_name);
                }
            }
        }
    }

    symbols
}

/// Extract symbols from the list= argument in rm()/remove() calls.
///
/// Handles:
/// - `list = "name"` (single string literal)
/// - `list = c("a", "b", "c")` (character vector)
///
/// Skips non-literal expressions (variables, function calls other than c()).
fn extract_list_symbols(args_node: &Node, content: &str) -> Vec<String> {
    let mut cursor = args_node.walk();

    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" {
            // Look for named argument with name "list"
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, content);
                if name == "list"
                    && let Some(value_node) = child.child_by_field_name("value")
                {
                    return extract_list_value_symbols(value_node, content);
                }
            }
        }
    }

    Vec::new()
}

/// Extract symbols from the value of a list= argument.
/// Handles string literals and c() calls with string arguments.
fn extract_list_value_symbols(value_node: Node, content: &str) -> Vec<String> {
    match value_node.kind() {
        "string" => {
            // rm(list = "x")
            if let Some(s) = extract_string_literal(value_node, content) {
                vec![s]
            } else {
                vec![]
            }
        }
        "call" => {
            // Check if it's a c() call
            if is_c_call(value_node, content) {
                // rm(list = c("x", "y", "z"))
                extract_c_string_args(value_node, content)
            } else {
                // Dynamic expression like ls() - not supported
                vec![]
            }
        }
        _ => {
            // Variable reference or other expression - not supported
            vec![]
        }
    }
}

/// Detect `exists("name")` calls in R code.
///
/// Returns one [`ExistsCall`] per `exists(...)` call whose name argument is a
/// string literal (`exists("x")`, `exists('x')`, or `exists(x = "x")`). Calls
/// with a non-literal name (`exists(varname)`, `exists(paste0(...))`) or no
/// argument are skipped — the name is not statically determinable. Only the
/// bare `exists` callee is matched (not `pkg::exists`), matching the
/// conservative shape of [`detect_rm_calls`].
pub fn detect_exists_calls(tree: &Tree, content: &str) -> Vec<ExistsCall> {
    let mut calls = Vec::new();
    visit_node_for_exists(tree.root_node(), content, &mut calls);
    calls
}

fn visit_node_for_exists(node: Node, content: &str, calls: &mut Vec<ExistsCall>) {
    if node.kind() == "identifier" {
        return;
    }
    if node.kind() == "call"
        && let Some(call) = try_parse_exists_call(node, content)
    {
        calls.push(call);
    }
    for child in node.children(&mut node.walk()) {
        visit_node_for_exists(child, content, calls);
    }
}

fn try_parse_exists_call(node: Node, content: &str) -> Option<ExistsCall> {
    let func_node = node.child_by_field_name("function")?;
    // Bare `exists` only — a `pkg::exists` callee is a `namespace_operator`
    // node, and `file.exists` is a different identifier, so both are excluded.
    if func_node.kind() != "identifier" || node_text(func_node, content) != "exists" {
        return None;
    }

    let args_node = node.child_by_field_name("arguments")?;
    if args_node.has_error() {
        return None;
    }

    // The name lives in the `x` formal: a named `x = "..."` argument, else the
    // first positional argument.
    let value_node = named_arg_value(&args_node, content, "x")
        .or_else(|| first_positional_arg_value(args_node))?;
    let name = extract_string_literal(value_node, content)?;
    // `exists("")` / `exists("   ")` name nothing usable; declare nothing. This
    // matches the `# raven: var` directive, which skips an empty-or-whitespace
    // name (`name.trim().is_empty()` in `directive.rs`).
    if name.trim().is_empty() {
        return None;
    }

    Some(ExistsCall {
        name,
        line: node.start_position().row as u32,
    })
}

/// The value node of the named argument `arg_name`, if present.
fn named_arg_value<'t>(args_node: &Node<'t>, content: &str, arg_name: &str) -> Option<Node<'t>> {
    let mut cursor = args_node.walk();
    args_node.children(&mut cursor).find_map(|child| {
        if child.kind() != "argument" {
            return None;
        }
        let name = child.child_by_field_name("name")?;
        if node_text(name, content) == arg_name {
            child.child_by_field_name("value")
        } else {
            None
        }
    })
}

/// Check if a call node is a c() call (character vector constructor).
fn is_c_call(node: Node, content: &str) -> bool {
    if node.kind() != "call" {
        return false;
    }
    if let Some(func_node) = node.child_by_field_name("function") {
        let func_text = node_text(func_node, content);
        return func_text == "c";
    }
    false
}

/// Extracts string literal arguments from a `c()` call node.
///
/// Only string literal arguments are returned; other argument types are ignored.
///
/// # Examples
///
/// ```text
/// // Given a Tree-sitter `Node` for `c("a", "b")`:
/// let strings = extract_c_string_args(node, r#"c("a", "b")"#);
/// assert_eq!(strings, vec!["a".to_string(), "b".to_string()]);
/// ```
fn extract_c_string_args(node: Node, content: &str) -> Vec<String> {
    let mut symbols = Vec::new();

    if let Some(args_node) = node.child_by_field_name("arguments") {
        let mut cursor = args_node.walk();
        for child in args_node.children(&mut cursor) {
            if child.kind() == "argument"
                && let Some(value_node) = child.child_by_field_name("value")
                && value_node.kind() == "string"
                && let Some(s) = extract_string_literal(value_node, content)
            {
                symbols.push(s);
            }
        }
    }

    symbols
}

// ============================================================================
// Library Call Detection
// ============================================================================

/// Detects package loads in the file: direct `library()`/`require()`/`loadNamespace()`
/// calls, apply-family calls whose FUN argument is a bare reference to
/// `library` or `require`, and {targets} `tar_option_set(packages = ...)`
/// calls (see `try_parse_tar_option_set_call`).
///
/// For direct calls, the package must be a bare identifier (`library(dplyr)`)
/// or a string literal (`library("dplyr")`); direct calls with
/// `character.only = TRUE` or a dynamic package argument are skipped.
///
/// For apply-family calls (`sapply`, `lapply`, `vapply`, `mapply`, plus the
/// bare and `purrr::`-qualified `map`/`walk`/`map_chr`/etc.), `character.only =
/// TRUE` is *required* and the X argument must resolve statically to a vector
/// of string literals — either an inline `c("a","b",...)` or a same-file
/// variable assigned exactly once via `<-`, `=`, or `assign("name", c(...))`.
/// Each apply emits one `LibraryCall` per package, all sharing the apply
/// call's end position.
///
/// For `tar_option_set(packages = ...)` calls, the bare spelling is honored
/// only when the same file also attaches targets (`library(targets)` /
/// `require(targets)` anywhere in the file); `targets::` / `targets:::`
/// qualified spellings are honored unconditionally. See
/// `append_gated_tar_option_set_calls` for the gate.
///
/// # Examples
///
/// ```
/// use raven::cross_file::source_detect::detect_library_calls;
///
/// let mut parser = tree_sitter::Parser::new();
/// parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
/// let source = "library(targets)\ntar_option_set(packages = c(\"dplyr\"))";
/// let tree = parser.parse(source, None).unwrap();
/// let calls = detect_library_calls(&tree, source);
/// assert_eq!(calls.len(), 2);
/// assert_eq!(calls[0].package, "targets");
/// assert_eq!(calls[1].package, "dplyr");
/// assert!(calls[1].attaches);
/// ```
pub fn detect_library_calls(tree: &Tree, content: &str) -> Vec<LibraryCall> {
    log::trace!("Starting tree-sitter parsing for library() call detection");
    let mut library_calls = Vec::new();
    let mut tar_candidates = Vec::new();
    let root = tree.root_node();
    let var_lookup = collect_var_bindings(root, content);
    visit_node_for_library(
        root,
        content,
        &var_lookup,
        &mut library_calls,
        &mut tar_candidates,
    );
    append_gated_tar_option_set_calls(&mut library_calls, tar_candidates);
    log::trace!(
        "Completed library() call detection, found {} calls",
        library_calls.len()
    );
    for lib_call in &library_calls {
        log::trace!(
            "  Detected library() call: package='{}' at line {} column {}",
            lib_call.package,
            lib_call.line,
            lib_call.column
        );
    }
    library_calls
}

/// Parse `text` and return the set of packages it *attaches* via a
/// **top-level** `library()` / `require()` call.
///
/// "Attaches" excludes `loadNamespace()` (which loads a namespace for
/// qualified `pkg::fn` access but does not put exports on the search path —
/// see [`LibraryCall::attaches`]). "Top-level" excludes calls nested inside a
/// function body, which do not attach until that function runs. Calls inside a
/// non-evaluating quoting wrapper (`quote`/`bquote`/`substitute`/`expression`,
/// rlang's `expr`/`quo`/…) are likewise excluded — they capture code without
/// evaluating it.
///
/// This models a testthat preamble file (`tests/testthat/helper*.R` /
/// `setup*.R`): testthat sources such files at the top level before any test
/// runs, so a top-level `library(tidyr)` in a helper attaches tidyr for every
/// sibling test file. The top-level-only gate parallels
/// [`crate::roxygen::extract_top_level_defs`] (both filter to top level),
/// though the justification differs: `extract_top_level_defs` captures only
/// definitions that exist at source time, whereas this excludes function-body
/// `library()` calls because they don't attach until the function runs.
///
/// `requireNamespace()` is NOT a match (it isn't a `library`/`require`/
/// `loadNamespace` call), so it never attaches here.
///
/// Top-level {targets} `tar_option_set(packages = ...)` calls also attach
/// (targets attaches the listed packages before running each target); the bare
/// spelling requires a top-level `library(targets)`/`require(targets)` in the
/// same text, while `targets::`/`targets:::` qualified spellings do not — see
/// `append_gated_tar_option_set_calls`.
///
/// Returns an empty set when the text cannot be parsed.
///
/// # Examples
///
/// ```
/// use raven::cross_file::source_detect::extract_attached_packages;
///
/// let pkgs = extract_attached_packages("library(tidyr)\nrequire(dplyr)\n");
/// assert!(pkgs.contains("tidyr"));
/// assert!(pkgs.contains("dplyr"));
///
/// // loadNamespace does not attach; a nested call does not attach at source time.
/// let none = extract_attached_packages("loadNamespace(\"tidyr\")\nf <- function() library(dplyr)\n");
/// assert!(none.is_empty());
/// ```
pub fn extract_attached_packages(text: &str) -> std::collections::BTreeSet<String> {
    use tree_sitter::Parser;
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .is_err()
    {
        return std::collections::BTreeSet::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return std::collections::BTreeSet::new();
    };
    let root = tree.root_node();
    let var_lookup = collect_var_bindings(root, text);
    let mut calls = Vec::new();
    let mut tar_candidates = Vec::new();
    visit_node_for_top_level_library(
        root,
        text,
        &var_lookup,
        false,
        false,
        &mut calls,
        &mut tar_candidates,
    );
    append_gated_tar_option_set_calls(&mut calls, tar_candidates);
    calls
        .into_iter()
        .filter(|call| call.attaches && !call.package.is_empty())
        .map(|call| call.package)
        .collect()
}

/// Like [`visit_node_for_library`], but only collects package loads that are
/// NOT lexically inside a `function_definition` and NOT inside a non-evaluating
/// quoting wrapper (`quote`/`bquote`/`substitute`/`expression`, rlang's
/// `expr`/`quo`/…). Function-body calls don't attach until the enclosing
/// function is called, and quoted expressions capture code without evaluating
/// it, so neither attaches when testthat sources the preamble file's top-level
/// code (`local({ ... })` blocks DO attach — they evaluate immediately).
/// `inside_fn` latches once an ancestor `function_definition` is entered;
/// `inside_quote` latches once an ancestor quoting call is entered.
///
/// `tar_option_set(packages = ...)` candidates found at the top level are
/// pushed into `tar_candidates` rather than `library_calls`; the caller
/// applies the targets-in-play gate afterwards via
/// [`append_gated_tar_option_set_calls`].
fn visit_node_for_top_level_library(
    node: Node,
    content: &str,
    var_lookup: &HashMap<String, VarBinding>,
    inside_fn: bool,
    inside_quote: bool,
    library_calls: &mut Vec<LibraryCall>,
    tar_candidates: &mut Vec<TarOptionSetCandidate>,
) {
    // Identifier nodes have no children and cannot be calls.
    if node.kind() == "identifier" {
        return;
    }
    // Tree-sitter normalizes R 4.1+ lambdas (`\(x) ...`) into
    // `function_definition` nodes too, so this single check covers both.
    let inside_fn = inside_fn || node.kind() == "function_definition";
    // A `library()` nested inside a quoting call (e.g. `quote(library(x))`)
    // captures code but never evaluates it, so it does not attach at source
    // time. Latch so all descendants are likewise excluded.
    let inside_quote = inside_quote || is_nonevaluating_quote_call(node, content);
    if !inside_fn && !inside_quote && node.kind() == "call" {
        if let Some(lib_call) = try_parse_library_call(node, content) {
            library_calls.push(lib_call);
        } else {
            // The apply-family and tar_option_set callee name-sets are
            // disjoint, so at most one of these can match.
            library_calls.extend(try_parse_apply_library_call(node, content, var_lookup));
            collect_tar_option_set_candidates(node, content, var_lookup, tar_candidates);
        }
    }
    for child in node.children(&mut node.walk()) {
        visit_node_for_top_level_library(
            child,
            content,
            var_lookup,
            inside_fn,
            inside_quote,
            library_calls,
            tar_candidates,
        );
    }
}

/// Whether `node` is a call to a quoting/capturing function that does NOT
/// evaluate its arguments — `quote`, `bquote`, `substitute`, `expression`, and
/// rlang's `expr`/`quo`/`quos`/`exprs`/`enquo`/`enquos`/`enexpr`/`enexprs`. A
/// `library()`/`require()` syntactically nested inside one of these is captured
/// as unevaluated code and never attaches at source time, so
/// [`visit_node_for_top_level_library`] must not record it.
///
/// Any namespace qualifier (`rlang::expr`, `base::quote`) is stripped before
/// matching. This is deliberately limited to statically recognizable quoting
/// wrappers; expressions built dynamically (e.g. `eval(parse(text = …))`) are
/// out of scope.
pub(crate) fn is_nonevaluating_quote_call(node: Node, content: &str) -> bool {
    if node.kind() != "call" {
        return false;
    }
    let Some(func_node) = node.child_by_field_name("function") else {
        return false;
    };
    let func_text = node_text(func_node, content);
    // Strip an optional `pkg::` / `pkg:::` namespace qualifier (the trailing
    // segment after the last colon is the bare function name).
    let name = func_text.rsplit(':').next().unwrap_or(func_text);
    matches!(
        name,
        "quote"
            | "bquote"
            | "substitute"
            | "expression"
            | "expr"
            | "quo"
            | "quos"
            | "exprs"
            | "enquo"
            | "enquos"
            | "enexpr"
            | "enexprs"
    )
}

/// Recursively traverses an AST subtree and collects statically determinable
/// package loads — direct `library`/`require`/`loadNamespace` calls and
/// apply-family calls whose FUN is `library`/`require`. Direct calls push at
/// most one `LibraryCall`; apply calls may push one entry per package, all
/// sharing the apply call's end position. {targets}
/// `tar_option_set(packages = ...)` calls are collected separately into
/// `tar_candidates` so the caller can apply the targets-in-play gate after
/// the walk via [`append_gated_tar_option_set_calls`].
///
/// # Parameters
///
/// - `node`: the current AST node to visit (will recurse into its children).
/// - `content`: source text for extracting node-local values when parsing calls.
/// - `var_lookup`: name→binding map (built once via [`collect_var_bindings`])
///   used by apply and tar_option_set detection to resolve same-file variable
///   references like `libs <- c(...); sapply(libs, library, character.only = TRUE)`.
/// - `library_calls`: mutable collector that receives discovered `LibraryCall`
///   entries in document order.
/// - `tar_candidates`: mutable collector for gate-pending
///   `tar_option_set(packages = ...)` package attachments.
///
/// # Examples
///
/// Traverse the whole tree and collect library-like calls:
///
/// ```text
/// let mut library_calls = Vec::new();
/// let mut tar_candidates = Vec::new();
/// let root = tree.root_node();
/// let var_lookup = collect_var_bindings(root, source_text);
/// visit_node_for_library(root, source_text, &var_lookup, &mut library_calls, &mut tar_candidates);
/// assert!(library_calls.iter().all(|c| !c.package.is_empty()));
/// ```
fn visit_node_for_library(
    node: Node,
    content: &str,
    var_lookup: &HashMap<String, VarBinding>,
    library_calls: &mut Vec<LibraryCall>,
    tar_candidates: &mut Vec<TarOptionSetCandidate>,
) {
    // Skip identifier nodes - they have no children
    if node.kind() == "identifier" {
        return;
    }
    if node.kind() == "call" {
        if let Some(lib_call) = try_parse_library_call(node, content) {
            library_calls.push(lib_call);
        } else {
            // Not a direct library/require/loadNamespace call; try the
            // apply-family form (sapply/map/etc. of bare library/require) and
            // the tar_option_set form. The two callee name-sets are disjoint,
            // so at most one of these can match.
            library_calls.extend(try_parse_apply_library_call(node, content, var_lookup));
            collect_tar_option_set_candidates(node, content, var_lookup, tar_candidates);
        }
    }

    for child in node.children(&mut node.walk()) {
        visit_node_for_library(child, content, var_lookup, library_calls, tar_candidates);
    }
}

/// Parse a call AST node and, if it is a static `library()`, `require()`, or `loadNamespace()` invocation,
/// return a `LibraryCall` containing the package name and the byte position (line and UTF-16 column) at the end of the call.
///
/// Returns `Some(LibraryCall)` when:
///
/// - the function name is `library`, `require`, or `loadNamespace`,
/// - the call's arguments are syntactically valid,
/// - the package name is statically determinable from a bare identifier or string literal (including a named `package=` argument),
/// - `character.only = TRUE` is not present (such calls are skipped as dynamic).
///
/// Returns `None` otherwise.
///
/// # Examples
///
/// Given a tree-sitter `Node` for a call and the source `content`,
/// `try_parse_library_call` returns a `LibraryCall` when the package is statically known.
/// (Constructing a Node requires a tree-sitter parse; this example is illustrative.)
fn try_parse_library_call(node: Node, content: &str) -> Option<LibraryCall> {
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);

    // Check if this is library(), require(), or loadNamespace()
    if func_text != "library" && func_text != "require" && func_text != "loadNamespace" {
        return None;
    }

    let args_node = node.child_by_field_name("arguments")?;

    // Skip if arguments contain error or missing nodes
    if args_node.has_error() {
        return None;
    }

    // Check for character.only = TRUE - skip these calls (dynamic package name)
    if has_character_only_true(&args_node, content) {
        return None;
    }

    // Extract package name from first argument
    let package = extract_package_name(&args_node, content)?;

    // Get position at the end of the call (after the closing paren)
    let end = node.end_position();
    let line_text = content.lines().nth(end.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, end.column);

    Some(LibraryCall {
        package,
        line: end.row as u32,
        column,
        // function_scope will be populated later in task 6.2
        function_scope: None,
        // `library`/`require` attach; `loadNamespace` (the only other name this
        // function admits) merely loads the namespace. Allowlist rather than
        // `!= "loadNamespace"` so a future non-attaching loader admitted above
        // is not silently classified as attaching.
        attaches: func_text == "library" || func_text == "require",
    })
}

/// Detect `pkg::member` / `pkg:::member` references (and incomplete `pkg::`
/// forms that parse as a `namespace_operator` with no RHS) across the document.
///
/// Canonical source for live LSP namespace metadata. Ranges are LSP UTF-16
/// coordinates. Reuses `namespace_completion::unquote_package` for delimiter
/// stripping (delimiter-strip only, not full R literal unescaping).
pub fn detect_namespace_references(tree: &Tree, content: &str) -> Vec<NamespaceReference> {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    visit_node_for_namespace(tree.root_node(), content, &lines, &mut out);
    out.sort_by_key(|r| (r.range.start_line, r.range.start_column));
    out
}

fn visit_node_for_namespace(
    node: Node,
    content: &str,
    lines: &[&str],
    out: &mut Vec<NamespaceReference>,
) {
    if node.kind() == "namespace_operator"
        && let Some(reference) = parse_namespace_operator(node, content, lines)
    {
        out.push(reference);
    }
    let mut walk = node.walk();
    for child in node.children(&mut walk) {
        visit_node_for_namespace(child, content, lines, out);
    }
}

fn parse_namespace_operator(ns: Node, content: &str, lines: &[&str]) -> Option<NamespaceReference> {
    let lhs = ns.child_by_field_name("lhs")?;
    // R accepts only an identifier or string/backtick literal as the package
    // qualifier. Anything else (`a$b::x`) is an invalid LHS — ignore it.
    if !matches!(lhs.kind(), "identifier" | "string") {
        return None;
    }
    let package = crate::namespace_completion::unquote_package(node_text(lhs, content));
    if package.is_empty() {
        return None;
    }

    // The operator (`::`/`:::`) is the unnamed child whose text is colons.
    let mut walk = ns.walk();
    let op = ns
        .children(&mut walk)
        .find(|c| matches!(node_text(*c, content), "::" | ":::"))?;
    let internal = node_text(op, content) == ":::";

    let member = ns.child_by_field_name("rhs").and_then(|rhs| {
        if !matches!(rhs.kind(), "identifier" | "string") {
            return None;
        }
        let name = crate::namespace_completion::unquote_package(node_text(rhs, content));
        if name.is_empty() {
            return None;
        }
        Some(NamespaceMember {
            name: name.to_string(),
            range: node_range_utf16(rhs, lines),
        })
    });

    Some(NamespaceReference {
        package: package.to_string(),
        package_range: node_range_utf16(lhs, lines),
        member,
        internal,
        range: node_range_utf16(ns, lines),
    })
}

/// Convert a node's tree-sitter byte-column range into LSP UTF-16 columns.
fn node_range_utf16(node: Node, lines: &[&str]) -> SourceRange {
    let start = node.start_position();
    let end = node.end_position();
    let col = |row: usize, byte_col: usize| -> u32 {
        let line = lines.get(row).copied().unwrap_or("");
        crate::utf16::byte_offset_to_utf16_column(line, byte_col)
    };
    SourceRange {
        start_line: start.row as u32,
        start_column: col(start.row, start.column),
        end_line: end.row as u32,
        end_column: col(end.row, end.column),
    }
}

/// Determine whether an arguments node sets `character.only` to `TRUE` or `T`.
///
/// Scans the children of `args_node` for a named argument `character.only` and
/// returns `true` only if its value text is the literal `TRUE` or `T`.
///
/// # Returns
///
/// `true` if `character.only` is explicitly `TRUE` or `T`, `false` otherwise.
///
/// # Examples
///
/// `args_node` is the Tree-sitter `arguments` node for a call like
/// `library(foo, character.only = TRUE)` and `content` is the source text
/// containing that call. The function will return `true` for that node.
fn has_character_only_true(args_node: &Node, content: &str) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, content);
            if name == "character.only"
                && let Some(value_node) = child.child_by_field_name("value")
            {
                let value = node_text(value_node, content);
                return value == "TRUE" || value == "T";
            }
        }
    }
    false
}

/// Determine a statically determinable package name from a call's argument list.
///
/// Looks for a named `package` argument first; if absent, inspects the first positional argument.
/// Returns `Some(package_name)` when the argument is a bare identifier (e.g., `library(dplyr)`)
/// or a string literal (e.g., `library("dplyr")` or `library('dplyr')`). Returns `None` for variables,
/// expressions, or other dynamic package specifications.
///
/// # Arguments
///
/// * `args_node` - The Tree-sitter `arguments` node of a call expression.
/// * `content` - The source text for extracting string literal contents.
///
/// # Returns
///
/// `Some(String)` containing the package name when statically determinable, `None` otherwise.
///
/// # Examples
///
/// ```text
/// // Parse an R call, obtain its arguments node, then:
/// let pkg = extract_package_name(&args_node, source_text);
/// assert_eq!(pkg, Some("dplyr".to_string()));
/// ```
fn extract_package_name(args_node: &Node, content: &str) -> Option<String> {
    let mut cursor = args_node.walk();
    let children: Vec<_> = args_node.children(&mut cursor).collect();

    // Look for named "package" argument first
    for child in &children {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, content);
            if name == "package"
                && let Some(value_node) = child.child_by_field_name("value")
            {
                return extract_package_value(value_node, content);
            }
        }
    }

    // Use first positional argument
    for child in &children {
        if child.kind() == "argument"
            && child.child_by_field_name("name").is_none()
            && let Some(value_node) = child.child_by_field_name("value")
        {
            return extract_package_value(value_node, content);
        }
    }

    None
}

/// Extract package name from a value node.
/// Handles bare identifiers (library(dplyr)) and string literals (library("dplyr")).
fn extract_package_value(node: Node, content: &str) -> Option<String> {
    match node.kind() {
        "identifier" => {
            // Bare identifier: library(dplyr)
            Some(node_text(node, content).to_string())
        }
        "string" => {
            // String literal: library("dplyr") or library('dplyr')
            extract_string_literal(node, content)
        }
        _ => {
            // Variable, expression, or other dynamic value - skip
            None
        }
    }
}

// ============================================================================
// Apply-Family Library Detection (issue #172)
// ============================================================================

use std::collections::HashMap;

/// Information collected for an identifier appearing on the LHS of any
/// binding form (assignment operator, `assign("name", ...)` call, or function
/// parameter) anywhere in the file. Used by apply-family detection to resolve
/// X arguments that are variable references.
///
/// `assignment_count` increments for *every* binding form: `<-`, `=`, `<<-`,
/// `->`, `->>`, `assign(...)`, and function parameters. Only `<-`, `=`, or
/// `assign("name", ...)` whose RHS is a `c(...)` of string literals populates
/// `static_packages`.
#[derive(Debug, Default)]
struct VarBinding {
    assignment_count: u32,
    /// `(packages, byte_offset_of_assignment_node)` from the first supported,
    /// statically-resolved assignment.
    static_packages: Option<(Vec<String>, usize)>,
}

impl VarBinding {
    /// Return packages iff `count == 1`, we extracted a static c-of-strings,
    /// and that assignment started before `before_byte`.
    fn resolved_before(&self, before_byte: usize) -> Option<&[String]> {
        if self.assignment_count != 1 {
            return None;
        }
        let (pkgs, off) = self.static_packages.as_ref()?;
        if *off < before_byte {
            Some(pkgs.as_slice())
        } else {
            None
        }
    }
}

fn collect_var_bindings(root: Node, content: &str) -> HashMap<String, VarBinding> {
    let mut map: HashMap<String, VarBinding> = HashMap::new();
    visit_var_bindings(root, content, &mut map);
    map
}

fn visit_var_bindings(node: Node, content: &str, map: &mut HashMap<String, VarBinding>) {
    match node.kind() {
        "binary_operator" => record_binary_assignment(node, content, map),
        "call" => record_assign_call(node, content, map),
        "function_definition" => record_function_params(node, content, map),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_var_bindings(child, content, map);
    }
}

fn record_binary_assignment(node: Node, content: &str, map: &mut HashMap<String, VarBinding>) {
    let mut cursor = node.walk();
    let named: Vec<Node> = node
        .children(&mut cursor)
        .filter(|c| c.is_named())
        .collect();
    if named.len() != 2 {
        return;
    }
    let mut op_walker = node.walk();
    let op_text = node.children(&mut op_walker).find_map(|c| {
        let t = node_text(c, content);
        if matches!(t, "<-" | "=" | "<<-" | "->" | "->>") {
            Some(t.to_string())
        } else {
            None
        }
    });
    drop(op_walker);
    let Some(op) = op_text else { return };

    let (name_node, value_node) = match op.as_str() {
        "<-" | "=" | "<<-" => (named[0], named[1]),
        "->" | "->>" => (named[1], named[0]),
        _ => return,
    };
    if name_node.kind() != "identifier" {
        return;
    }
    let name = node_text(name_node, content).to_string();
    let entry = map.entry(name).or_default();
    entry.assignment_count = entry.assignment_count.saturating_add(1);

    if matches!(op.as_str(), "<-" | "=")
        && let Some(packages) = extract_c_strings_strict(value_node, content)
        && entry.static_packages.is_none()
    {
        entry.static_packages = Some((packages, node.start_byte()));
    }
}

/// Returns true if `name` is a non-empty prefix of `assign()`'s `value`
/// formal that is *not* also a prefix of any other formal — i.e. an
/// unambiguous partial match for `value`. Excludes the exact name "value"
/// itself; that's handled by the exact-match pass.
fn is_partial_prefix_of_value(name: &str) -> bool {
    if name.is_empty() || name == "value" {
        return false;
    }
    if !"value".starts_with(name) {
        return false;
    }
    // assign()'s other formals: x, pos, envir, inherits, immediate.
    // (`x` is a single character so partial-matching `x` just means name == "x".)
    !"x".starts_with(name)
        && !"pos".starts_with(name)
        && !"envir".starts_with(name)
        && !"inherits".starts_with(name)
        && !"immediate".starts_with(name)
}

fn record_assign_call(node: Node, content: &str, map: &mut HashMap<String, VarBinding>) {
    let Some(func_node) = node.child_by_field_name("function") else {
        return;
    };
    if node_text(func_node, content) != "assign" {
        return;
    }
    let Some(args_node) = node.child_by_field_name("arguments") else {
        return;
    };
    if args_node.has_error() {
        return;
    }

    // Bucketize args. R argument matching is two passes (exact, then partial)
    // followed by positional fill. We only track the `x` and `value` formals
    // — other formals (`pos`/`envir`/`inherits`/`immediate`) and unknown
    // names are ignored and don't consume positional slots.
    let mut exact_x: Vec<Node> = Vec::new();
    let mut exact_value: Vec<Node> = Vec::new();
    let mut partial_to_value: Vec<Node> = Vec::new();
    let mut positional: Vec<Node> = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() != "argument" {
            continue;
        }
        let Some(value) = child.child_by_field_name("value") else {
            continue;
        };
        if let Some(name_node) = child.child_by_field_name("name") {
            let arg_name = node_text(name_node, content);
            if arg_name == "x" {
                exact_x.push(value);
            } else if arg_name == "value" {
                exact_value.push(value);
            } else if is_partial_prefix_of_value(arg_name) {
                partial_to_value.push(value);
            }
        } else {
            positional.push(value);
        }
    }

    // If any of the rules R uses would error — duplicate exact, multiple
    // partials matching the same formal, or a partial colliding with an exact
    // — the call itself errors and never assigns. Skip the call entirely so
    // we don't record a static binding R never produced.
    let value_collision = (exact_value.len() == 1 && !partial_to_value.is_empty())
        || exact_value.len() > 1
        || partial_to_value.len() > 1;
    if exact_x.len() > 1 || value_collision {
        return;
    }

    // Resolve x (exact > positional[0]) and value (exact > partial > next
    // positional after x).
    let x_value = exact_x
        .first()
        .copied()
        .or_else(|| positional.first().copied());
    let value_node = exact_value
        .first()
        .copied()
        .or_else(|| partial_to_value.first().copied())
        .or_else(|| {
            let next_idx = if exact_x.is_empty() { 1 } else { 0 };
            positional.get(next_idx).copied()
        });

    let Some(x_value) = x_value else { return };
    let Some(name) = extract_string_literal(x_value, content) else {
        return;
    };

    let entry = map.entry(name).or_default();
    entry.assignment_count = entry.assignment_count.saturating_add(1);
    if let Some(value_node) = value_node
        && let Some(packages) = extract_c_strings_strict(value_node, content)
        && entry.static_packages.is_none()
    {
        entry.static_packages = Some((packages, node.start_byte()));
    }
}

fn record_function_params(node: Node, content: &str, map: &mut HashMap<String, VarBinding>) {
    let parameters = match node.child_by_field_name("parameters") {
        Some(n) => n,
        None => return,
    };
    let mut cursor = parameters.walk();
    for child in parameters.children(&mut cursor) {
        if child.kind() != "parameter" {
            continue;
        }
        let mut param_cursor = child.walk();
        let Some(ident) = child
            .children(&mut param_cursor)
            .find(|c| c.kind() == "identifier")
        else {
            continue;
        };
        let name = node_text(ident, content).to_string();
        let entry = map.entry(name).or_default();
        entry.assignment_count = entry.assignment_count.saturating_add(1);
    }
}

/// Apply-family functions whose bare-identifier form may load packages
/// dynamically when paired with `library`/`require` and `character.only = TRUE`.
///
/// Restricted to functions with a clean `(X, FUN, ...)` shape (or `(FUN, ...)`
/// for `mapply`). Excluded:
/// - `map2`/`walk2`/`map2_*` — two parallel X vectors with FUN at position 2.
/// - `pmap`/`pwalk` — X is a list of vectors, not a single vector.
/// - `map_if`/`map_at` — `(X, predicate-or-selector, FUN, ...)`, FUN at position 2.
const APPLY_BARE_NAMES: &[&str] = &[
    "sapply", "lapply", "vapply", "mapply", "map", "walk", "imap", "iwalk", "map_chr", "map_int",
    "map_dbl", "map_lgl", "map_raw", "map_dfr", "map_dfc", "map_vec",
];

/// Apply-family functions accepted under the `purrr::` namespace.
const APPLY_PURRR_NAMES: &[&str] = &[
    "map", "walk", "imap", "iwalk", "map_chr", "map_int", "map_dbl", "map_lgl", "map_raw",
    "map_dfr", "map_dfc", "map_vec",
];

/// Return `(x_position, fun_position)` describing where in the call's
/// positional argument list the X (vector) and FUN (mapped function) values
/// are expected, for a supported apply-family function. Returns `None` when
/// the function isn't one we recognise.
///
/// Most apply functions (`sapply`, `lapply`, `vapply`, the purrr `map`/`walk`
/// family) take `(X, FUN, ...)`, so X is at position 0 and FUN at position 1.
/// `mapply`'s signature is `(FUN, ...)`, so FUN is at position 0 and the
/// first vector at position 1.
fn apply_arg_positions(func_node: Node, content: &str) -> Option<(usize, usize)> {
    let name = match func_node.kind() {
        "identifier" => {
            let n = node_text(func_node, content);
            if !APPLY_BARE_NAMES.contains(&n) {
                return None;
            }
            n
        }
        "namespace_operator" => {
            let mut cursor = func_node.walk();
            let named_children: Vec<Node> = func_node
                .children(&mut cursor)
                .filter(|c| c.is_named())
                .collect();
            if named_children.len() != 2 {
                return None;
            }
            if node_text(named_children[0], content) != "purrr" {
                return None;
            }
            let n = node_text(named_children[1], content);
            if !APPLY_PURRR_NAMES.contains(&n) {
                return None;
            }
            n
        }
        _ => return None,
    };
    match name {
        "mapply" => Some((1, 0)),
        _ => Some((0, 1)),
    }
}

/// Strict variant of `extract_c_string_args`: returns `Some(packages)` only
/// when `node` is `c(arg1, arg2, ...)` where every argument is a positional
/// string literal and at least one argument is present. Returns `None` for any
/// non-string element, named argument, or empty `c()` — anything but a fully
/// static character vector is treated as dynamic.
fn extract_c_strings_strict(node: Node, content: &str) -> Option<Vec<String>> {
    extract_c_string_nodes_strict(node, content)
        .map(|pairs| pairs.into_iter().map(|(s, _)| s).collect())
}

/// Like [`extract_c_strings_strict`], but pairs each extracted string with its
/// own string-literal `Node` so callers can anchor per-package positions (see
/// `try_parse_tar_option_set_call`). This is the single implementation; the
/// string-only variant delegates here.
fn extract_c_string_nodes_strict<'tree>(
    node: Node<'tree>,
    content: &str,
) -> Option<Vec<(String, Node<'tree>)>> {
    if !is_c_call(node, content) {
        return None;
    }
    let args_node = node.child_by_field_name("arguments")?;
    if args_node.has_error() {
        return None;
    }
    let mut strings = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() != "argument" {
            continue;
        }
        if child.child_by_field_name("name").is_some() {
            return None;
        }
        let value_node = child.child_by_field_name("value")?;
        if value_node.kind() != "string" {
            return None;
        }
        let s = extract_string_literal(value_node, content)?;
        strings.push((s, value_node));
    }
    if strings.is_empty() {
        None
    } else {
        Some(strings)
    }
}

/// Try to interpret `node` as an apply-family call that loads a static vector
/// of packages — e.g. `sapply(c("dplyr","tidyr"), library, character.only = TRUE)`.
///
/// Returns one `LibraryCall` per package when, given the call's
/// `(x_position, fun_position)` from [`apply_arg_positions`]:
/// - `character.only = TRUE` (or `T`) is present,
/// - the positional arg at `fun_position` is the bare identifier `library`
///   or `require` (so we don't match calls like
///   `sapply(c("dplyr"), identity, library, ...)` where library is just a
///   `...`-passthrough),
/// - the positional arg at `x_position` resolves to a static `Vec<String>`
///   via inline `c(...)` or a same-file variable in `var_lookup`.
///
/// All emitted entries share the apply call's end position.
fn try_parse_apply_library_call(
    node: Node,
    content: &str,
    var_lookup: &HashMap<String, VarBinding>,
) -> Vec<LibraryCall> {
    let Some(func_node) = node.child_by_field_name("function") else {
        return Vec::new();
    };
    let Some((x_pos, fun_pos)) = apply_arg_positions(func_node, content) else {
        return Vec::new();
    };

    let Some(args_node) = node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    if args_node.has_error() {
        return Vec::new();
    }
    if !has_character_only_true(&args_node, content) {
        return Vec::new();
    }

    // Positional argument values, in source order. Named args (including
    // `character.only =`, `simplify =`, `FUN =`, `X =`, etc.) are excluded
    // here — see `test_apply_with_named_x_arg_skipped` for the limitation.
    let mut positional_values: Vec<Node> = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() != "argument" {
            continue;
        }
        if child.child_by_field_name("name").is_some() {
            continue;
        }
        if let Some(value) = child.child_by_field_name("value") {
            positional_values.push(value);
        }
    }

    // FUN must sit at the position dictated by the apply's signature and be a
    // bare `library`/`require` identifier — not just appear *somewhere* among
    // the positional args.
    let Some(&fun_value) = positional_values.get(fun_pos) else {
        return Vec::new();
    };
    if fun_value.kind() != "identifier" {
        return Vec::new();
    }
    let fun_text = node_text(fun_value, content);
    if fun_text != "library" && fun_text != "require" {
        return Vec::new();
    }

    let Some(&x_value) = positional_values.get(x_pos) else {
        return Vec::new();
    };
    let packages: Vec<String> = match x_value.kind() {
        "identifier" => {
            let text = node_text(x_value, content);
            match var_lookup
                .get(text)
                .and_then(|b| b.resolved_before(node.start_byte()))
            {
                Some(pkgs) => pkgs.to_vec(),
                None => return Vec::new(),
            }
        }
        _ => match extract_c_strings_strict(x_value, content) {
            Some(v) => v,
            None => return Vec::new(),
        },
    };

    let end = node.end_position();
    let line_text = content.lines().nth(end.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, end.column);
    let line = end.row as u32;

    packages
        .into_iter()
        .map(|package| LibraryCall {
            package,
            line,
            column,
            function_scope: None,
            // This path only fires for `library`/`require` applied via
            // `sapply`/`lapply`/etc. (see the `fun_text` guard above), both of
            // which attach.
            attaches: true,
        })
        .collect()
}

// ============================================================================
// targets::tar_option_set Package Detection (issue #637)
// ============================================================================

/// A package attachment extracted from a {targets}
/// `tar_option_set(packages = ...)` call during a library-detection walk,
/// tagged with whether the callee was the bare spelling (`tar_option_set`)
/// rather than the qualified `targets::tar_option_set` /
/// `targets:::tar_option_set`.
///
/// Bare candidates are subject to the targets-in-play gate applied after the
/// walk — see [`append_gated_tar_option_set_calls`].
struct TarOptionSetCandidate {
    call: LibraryCall,
    bare: bool,
}

/// Whether `func_node` (a call's `function` child) names {targets}'
/// `tar_option_set`: the bare identifier `tar_option_set`, or the qualified
/// `targets::tar_option_set` / `targets:::tar_option_set`.
fn is_tar_option_set_callee(func_node: Node, content: &str) -> bool {
    match func_node.kind() {
        "identifier" => node_text(func_node, content) == "tar_option_set",
        "namespace_operator" => {
            let Some(lhs) = func_node.child_by_field_name("lhs") else {
                return false;
            };
            let Some(rhs) = func_node.child_by_field_name("rhs") else {
                return false;
            };
            lhs.kind() == "identifier"
                && rhs.kind() == "identifier"
                && node_text(lhs, content) == "targets"
                && node_text(rhs, content) == "tar_option_set"
        }
        _ => false,
    }
}

/// If `node` is a `tar_option_set(...)` call whose callee matches
/// [`is_tar_option_set_callee`], parse it via
/// [`try_parse_tar_option_set_call`] and push one gate-pending candidate per
/// package, tagged with whether the callee was the bare spelling.
fn collect_tar_option_set_candidates(
    node: Node,
    content: &str,
    var_lookup: &HashMap<String, VarBinding>,
    tar_candidates: &mut Vec<TarOptionSetCandidate>,
) {
    let Some(func_node) = node.child_by_field_name("function") else {
        return;
    };
    if !is_tar_option_set_callee(func_node, content) {
        return;
    }
    let bare = func_node.kind() == "identifier";
    tar_candidates.extend(
        try_parse_tar_option_set_call(node, content, var_lookup)
            .into_iter()
            .map(|call| TarOptionSetCandidate { call, bare }),
    );
}

/// Append gate-filtered `tar_option_set(packages = ...)` attachments to
/// `library_calls`.
///
/// The targets-in-play gate: a QUALIFIED callee (`targets::tar_option_set` /
/// `targets:::tar_option_set`) is accepted unconditionally — the author has
/// unambiguously named the {targets} package. A BARE `tar_option_set` callee
/// is accepted only when the same walk already found an attaching load of
/// targets (`library(targets)` / `require(targets)`), so that an unrelated
/// user function that happens to be called `tar_option_set` doesn't silently
/// attach packages. This mirrors the `shiny_in_play` gate precedent in
/// `scope.rs`'s `push_shiny_deferred_scopes`. The gate is position-independent
/// within the file: a `library(targets)` anywhere the walk visited counts,
/// regardless of whether it appears before or after the `tar_option_set` call.
///
/// Must be called after the walk completes (the gate inspects the walk's
/// collected `library_calls`), and must stay the single shared post-filter for
/// both `detect_library_calls` and `extract_attached_packages` so the two
/// walkers cannot drift.
fn append_gated_tar_option_set_calls(
    library_calls: &mut Vec<LibraryCall>,
    tar_candidates: Vec<TarOptionSetCandidate>,
) {
    let targets_attached = library_calls
        .iter()
        .any(|call| call.attaches && call.package == "targets");
    library_calls.extend(
        tar_candidates
            .into_iter()
            .filter(|candidate| !candidate.bare || targets_attached)
            .map(|candidate| candidate.call),
    );
}

/// Try to interpret `node` as a {targets} `tar_option_set(packages = ...)`
/// call and return one `LibraryCall` per statically determinable package,
/// each with `attaches: true` — targets attaches these packages before
/// running each target, so scripts sourced by `_targets.R` use their exports
/// as bare names.
///
/// Only the NAMED `packages =` argument is honored. This is an intentional
/// limitation: `tar_option_set`'s first formal is `tidy_eval`, not `packages`,
/// so positional matching (`tar_option_set(TRUE, c("dplyr"))`) would require
/// modeling the full formals list and is deliberately not attempted. Accepted
/// value shapes:
///
/// - a single string literal: `packages = "dplyr"`
/// - a strict `c()` of positional string literals:
///   `packages = c("dplyr", "tidyr")`
/// - an identifier resolving to a same-file, assigned-exactly-once static
///   `c()` of string literals bound before this call (the same
///   `VarBinding::resolved_before` machinery apply-family detection uses)
///
/// Anything else (dynamic calls, `character(0)`, empty `c()`) yields nothing.
///
/// Position anchoring: `tar_option_set()` calls routinely span 10–30 lines,
/// and the missing-package diagnostic builds its range from a `LibraryCall`'s
/// line/column while `# nolint` suppression is line-keyed — so for the
/// string-literal and `c()`-of-literals shapes, each package's `LibraryCall`
/// carries the end position of ITS OWN string-literal node (not the call's
/// closing paren). The variable-resolved shape has no literal at the call
/// site, so it falls back to the call's end position, mirroring the
/// apply-family pattern.
///
/// Union leniency: multiple `tar_option_set` calls in one file union their
/// packages. targets' real runtime semantics are last-call-wins for
/// `packages =`, but raven deliberately favors false negatives (a package
/// wrongly considered available) over false positives (flagging a defined
/// verb as undefined) — do not "fix" this to last-call-wins.
///
/// The caller is responsible for the bare-vs-qualified targets-in-play gate
/// (see [`append_gated_tar_option_set_calls`]); this function parses any
/// callee matching [`is_tar_option_set_callee`].
fn try_parse_tar_option_set_call(
    node: Node,
    content: &str,
    var_lookup: &HashMap<String, VarBinding>,
) -> Vec<LibraryCall> {
    let Some(func_node) = node.child_by_field_name("function") else {
        return Vec::new();
    };
    if !is_tar_option_set_callee(func_node, content) {
        return Vec::new();
    }
    let Some(args_node) = node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    if args_node.has_error() {
        return Vec::new();
    }

    // Find the named `packages =` argument (exact name only). A duplicate
    // named `packages` argument would make the call itself error in R, so
    // treat that as no match.
    let mut packages_values: Vec<Node> = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
            && node_text(name_node, content) == "packages"
            && let Some(value_node) = child.child_by_field_name("value")
        {
            packages_values.push(value_node);
        }
    }
    let [value_node] = packages_values.as_slice() else {
        return Vec::new();
    };

    // Build a LibraryCall anchored at `anchor`'s end position (line + UTF-16
    // column), matching how the other detectors in this file anchor.
    let call_at = |package: String, anchor: Node| -> LibraryCall {
        let end = anchor.end_position();
        let line_text = content.lines().nth(end.row).unwrap_or("");
        LibraryCall {
            package,
            line: end.row as u32,
            column: byte_offset_to_utf16_column(line_text, end.column),
            function_scope: None,
            attaches: true,
        }
    };

    match value_node.kind() {
        // `packages = "dplyr"` — anchored at the literal itself.
        "string" => match extract_string_literal(*value_node, content) {
            Some(package) => vec![call_at(package, *value_node)],
            None => Vec::new(),
        },
        // `pkgs <- c("a", "b"); tar_option_set(packages = pkgs)` — no literal
        // at the call site, so anchor at the call's end position.
        "identifier" => {
            let text = node_text(*value_node, content);
            match var_lookup
                .get(text)
                .and_then(|b| b.resolved_before(node.start_byte()))
            {
                Some(pkgs) => pkgs
                    .iter()
                    .map(|package| call_at(package.clone(), node))
                    .collect(),
                None => Vec::new(),
            }
        }
        // `packages = c("dplyr", "tidyr")` — one call per package, each
        // anchored at its own string-literal node.
        _ => match extract_c_string_nodes_strict(*value_node, content) {
            Some(pairs) => pairs
                .into_iter()
                .map(|(package, literal)| call_at(package, literal))
                .collect(),
            None => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_r(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    #[test]
    fn ns_ref_basic_exported() {
        let code = "dplyr::mutate(x)\n";
        let tree = parse_r(code);
        let refs = detect_namespace_references(&tree, code);
        assert_eq!(refs.len(), 1, "got: {refs:?}");
        let r = &refs[0];
        assert_eq!(r.package, "dplyr");
        assert!(!r.internal);
        let m = r.member.as_ref().expect("member present");
        assert_eq!(m.name, "mutate");
        // package range covers "dplyr" at columns 0..5
        assert_eq!(
            (
                r.package_range.start_line,
                r.package_range.start_column,
                r.package_range.end_column
            ),
            (0, 0, 5)
        );
    }

    #[test]
    fn ns_ref_internal_triple_colon() {
        let refs =
            detect_namespace_references(&parse_r("dplyr:::peek_mask()\n"), "dplyr:::peek_mask()\n");
        assert_eq!(refs.len(), 1);
        assert!(refs[0].internal);
        assert_eq!(refs[0].member.as_ref().unwrap().name, "peek_mask");
    }

    #[test]
    fn ns_ref_quoted_lhs_and_rhs() {
        for code in [
            r#""dplyr"::mutate"#,
            "`dplyr`::mutate",
            r#"dplyr::"mutate""#,
        ] {
            let src = format!("{code}\n");
            let refs = detect_namespace_references(&parse_r(&src), &src);
            assert_eq!(refs.len(), 1, "code: {code} got: {refs:?}");
            assert_eq!(refs[0].package, "dplyr", "code: {code}");
            assert_eq!(
                refs[0].member.as_ref().unwrap().name,
                "mutate",
                "code: {code}"
            );
        }
    }

    #[test]
    fn ns_ref_nonsyntactic_member() {
        let src = "pkg::`non syntactic`\n";
        let refs = detect_namespace_references(&parse_r(src), src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].member.as_ref().unwrap().name, "non syntactic");
    }

    #[test]
    fn ns_ref_incomplete_records_none_member() {
        // Incomplete `pkg::` parsed as a namespace_operator with no rhs is kept
        // for warming with member: None.
        let src = "library(dplyr)\ndplyr::\n";
        let refs = detect_namespace_references(&parse_r(src), src);
        let incomplete: Vec<_> = refs.iter().filter(|r| r.member.is_none()).collect();
        assert_eq!(incomplete.len(), 1, "got: {refs:?}");
        assert_eq!(incomplete[0].package, "dplyr");
    }

    #[test]
    fn ns_ref_ignores_invalid_lhs_and_comments() {
        // a$b::x has a non-identifier/non-string LHS -> ignored.
        // A `::` inside a comment or string is not a namespace_operator node.
        let src = "a$b::x\n# dplyr::mutate\ny <- \"tidyr::pivot\"\n";
        let refs = detect_namespace_references(&parse_r(src), src);
        assert!(refs.is_empty(), "got: {refs:?}");
    }

    #[test]
    fn ns_ref_utf16_columns() {
        // 🎉 is 2 UTF-16 units; pkg starts at column 2.
        let src = "🎉; dplyr::mutate\n";
        let refs = detect_namespace_references(&parse_r(src), src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].package_range.start_column, 4); // 2 (emoji) + 2 ("; ")
    }

    // ------------------------------------------------------------------
    // extract_attached_packages (issue #432): top-level attaching
    // library()/require() only; loadNamespace and function-body calls excluded.
    // ------------------------------------------------------------------

    #[test]
    fn extract_attached_packages_direct_library_and_require() {
        let pkgs = extract_attached_packages("library(tidyr)\nrequire(dplyr)\n");
        assert!(pkgs.contains("tidyr"), "got: {pkgs:?}");
        assert!(pkgs.contains("dplyr"), "got: {pkgs:?}");
        assert_eq!(pkgs.len(), 2, "got: {pkgs:?}");
    }

    #[test]
    fn extract_attached_packages_string_and_named_arg() {
        let pkgs = extract_attached_packages("library(\"tidyr\")\nlibrary(package = dplyr)\n");
        assert!(pkgs.contains("tidyr"), "got: {pkgs:?}");
        assert!(pkgs.contains("dplyr"), "got: {pkgs:?}");
    }

    #[test]
    fn extract_attached_packages_excludes_load_namespace() {
        // loadNamespace loads the namespace for qualified access but does not
        // attach exports to the search path.
        let pkgs = extract_attached_packages("loadNamespace(\"tidyr\")\n");
        assert!(pkgs.is_empty(), "loadNamespace must not attach: {pkgs:?}");
    }

    #[test]
    fn extract_attached_packages_excludes_function_body_calls() {
        // A library() inside a function body does not attach until the function
        // is called, so testthat sourcing the preamble does not attach it.
        let pkgs = extract_attached_packages("f <- function() library(stringr)\n");
        assert!(
            pkgs.is_empty(),
            "function-body library() must not attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_excludes_lambda_body_calls() {
        // R 4.1+ lambda bodies are `function_definition` nodes too.
        let pkgs = extract_attached_packages("g <- \\() library(stringr)\n");
        assert!(
            pkgs.is_empty(),
            "lambda-body library() must not attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_includes_top_level_local_block() {
        // `local({ ... })` evaluates immediately when the file is sourced, so a
        // `library()` inside it DOES attach (it is not a function body).
        let pkgs = extract_attached_packages("local({\n  library(tidyr)\n})\n");
        assert!(
            pkgs.contains("tidyr"),
            "top-level local() library() must attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_includes_top_level_if_block() {
        let pkgs = extract_attached_packages("if (TRUE) {\n  library(tidyr)\n}\n");
        assert!(
            pkgs.contains("tidyr"),
            "top-level if-block library() must attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_includes_suppress_messages_wrapper() {
        // A wrapper call at top level (e.g. suppressMessages(library(x))) still
        // attaches — the inner library() is reached on recursion, no function
        // body intervenes.
        let pkgs = extract_attached_packages("suppressMessages(library(tidyr))\n");
        assert!(
            pkgs.contains("tidyr"),
            "suppressMessages(library()) must attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_excludes_quote_wrapper() {
        // `quote()` captures the call as an unevaluated expression; sourcing the
        // preamble never attaches the package.
        let pkgs = extract_attached_packages("quote(library(tidyr))\n");
        assert!(
            pkgs.is_empty(),
            "quote(library()) must not attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_excludes_rlang_expr_wrapper() {
        // Namespace-qualified rlang quoting function: also non-evaluating.
        let pkgs = extract_attached_packages("e <- rlang::expr(library(tidyr))\n");
        assert!(
            pkgs.is_empty(),
            "rlang::expr(library()) must not attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_excludes_bquote_and_expression() {
        assert!(extract_attached_packages("bquote(library(tidyr))\n").is_empty());
        assert!(extract_attached_packages("expression(library(tidyr))\n").is_empty());
        assert!(extract_attached_packages("substitute(library(tidyr))\n").is_empty());
    }

    #[test]
    fn extract_attached_packages_empty_on_unparseable() {
        // Robustness: never panics, returns empty on garbage.
        let result = extract_attached_packages("library(\n");
        assert!(
            result.is_empty(),
            "malformed input must yield no attached packages: {result:?}"
        );
    }

    #[test]
    fn test_source_double_quotes() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
        assert!(!sources[0].is_sys_source);
        assert!(!sources[0].local);
        assert!(!sources[0].chdir);
    }

    #[test]
    fn test_source_single_quotes() {
        let code = "source('utils.R')";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
    }

    #[test]
    fn test_source_named_argument() {
        let code = r#"source(file = "utils.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
    }

    #[test]
    fn test_sys_source() {
        let code = r#"sys.source("utils.R", envir = globalenv())"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
        assert!(sources[0].is_sys_source);
    }

    #[test]
    fn test_source_with_local_true() {
        let code = r#"source("utils.R", local = TRUE)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].local);
    }

    #[test]
    fn test_source_with_chdir_true() {
        let code = r#"source("utils.R", chdir = TRUE)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].chdir);
    }

    #[test]
    fn test_source_with_variable_path_skipped() {
        let code = "source(my_path)";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_with_paste0_skipped() {
        let code = r#"source(paste0("dir/", filename))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_multiple_source_calls() {
        let code = r#"source("a.R")
source("b.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].path, "a.R");
        assert_eq!(sources[0].line, 0);
        assert_eq!(sources[1].path, "b.R");
        assert_eq!(sources[1].line, 1);
    }

    #[test]
    fn test_source_position() {
        let code = "x <- 1\nsource(\"utils.R\")";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].line, 1);
        assert_eq!(sources[0].column, 0);
    }

    /// Regression coverage for issue #138: source() at top level must be
    /// flagged as not function-scoped.
    #[test]
    fn test_source_top_level_is_not_function_scoped() {
        let code = "source(\"utils.R\")\n";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(!sources[0].is_function_scoped);
    }

    /// Regression coverage for issue #138: source() inside a function body
    /// is deferred-evaluation and must be flagged as function-scoped.
    #[test]
    fn test_source_in_function_body_is_function_scoped() {
        let code = "f <- function() {\n  source(\"utils.R\")\n}\n";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_function_scoped);
    }

    /// Regression coverage for issue #138: walking ancestors must catch
    /// source() inside nested function bodies, not just the immediate
    /// parent function. Also covers R 4.1+ lambda syntax (`\(...)`),
    /// which tree-sitter-r normalizes to `function_definition`.
    #[test]
    fn test_source_in_nested_function_body_is_function_scoped() {
        let code = "outer <- function() {\n  inner <- \\() {\n    source(\"utils.R\")\n  }\n}\n";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_function_scoped);
    }

    #[test]
    fn test_non_source_call_ignored() {
        let code = "print(\"hello\")";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_sys_source_with_globalenv() {
        let code = r#"sys.source("utils.R", envir = globalenv())"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(sources[0].sys_source_global_env);
        assert!(sources[0].inherits_symbols());
    }

    #[test]
    fn test_sys_source_with_global_env_dot() {
        let code = r#"sys.source("utils.R", envir = .GlobalEnv)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(sources[0].sys_source_global_env);
        assert!(sources[0].inherits_symbols());
    }

    #[test]
    fn test_sys_source_with_new_env() {
        let code = r#"sys.source("utils.R", envir = new.env())"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(!sources[0].sys_source_global_env);
        assert!(!sources[0].inherits_symbols());
    }

    #[test]
    fn test_sys_source_without_envir() {
        // sys.source without envir defaults to baseenv(), not global
        let code = r#"sys.source("utils.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(!sources[0].sys_source_global_env);
        assert!(!sources[0].inherits_symbols());
    }

    // ==================== rm()/remove() detection tests ====================

    #[test]
    fn test_rm_single_bare_symbol() {
        let code = "rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[0].column, 0);
    }

    #[test]
    fn test_rm_multiple_bare_symbols() {
        let code = "rm(x, y, z)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y", "z"]);
    }

    #[test]
    fn test_remove_single_bare_symbol() {
        let code = "remove(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_remove_multiple_bare_symbols() {
        let code = "remove(a, b, c)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b", "c"]);
    }
    #[test]
    fn test_internal_remove_routine_call_ignored() {
        // In base/R/rm.R, `.Internal(remove(list, envir, inherits))`
        // names the C entry point used to implement rm()/remove(); it is
        // not an R-level `remove()` call and must not remove `envir` from
        // the surrounding function scope.
        let code = ".Internal(remove(list, envir, inherits))";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_empty_call() {
        // rm() with no arguments should not produce any RmCall
        let code = "rm()";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_string_argument_skipped() {
        // rm("x") with string in positional arg should be skipped (not a bare symbol)
        let code = r#"rm("x")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_expression_argument_skipped() {
        // rm(x + y) should be skipped (not an identifier)
        let code = "rm(x + y)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_number_argument_skipped() {
        // rm(1) should be skipped (not an identifier)
        let code = "rm(1)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_position() {
        let code = "x <- 1\nrm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].line, 1);
        assert_eq!(rm_calls[0].column, 0);
    }

    #[test]
    fn test_rm_position_with_offset() {
        let code = "x <- 1; rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[0].column, 8);
    }

    #[test]
    fn test_multiple_rm_calls() {
        let code = "rm(x)\nrm(y, z)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 2);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[1].symbols, vec!["y", "z"]);
        assert_eq!(rm_calls[1].line, 1);
    }

    #[test]
    fn test_non_rm_call_ignored() {
        let code = "print(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_list_single_string() {
        // rm(list = "x") should extract "x"
        let code = r#"rm(list = "x")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_mixed_bare_and_list() {
        // rm(x, list = "y") - should extract both bare symbol x and list symbol y
        let code = r#"rm(x, list = "y")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y"]);
    }

    // ------------------------------------------------------------------
    // detect_exists_calls: `exists("name")` declares `name` (parity with
    // `# raven: var name`).
    // ------------------------------------------------------------------

    #[test]
    fn test_exists_double_quoted_name() {
        let code = r#"exists("apple")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apple");
        assert_eq!(calls[0].line, 0);
    }

    #[test]
    fn test_exists_single_quoted_name() {
        let code = "exists('apple')";
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apple");
    }

    #[test]
    fn test_exists_negated_guard() {
        // The idiomatic `if (!exists("x")) x <- ...` guard.
        let code = "if (!exists(\"apple\")) apple <- 1";
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apple");
    }

    #[test]
    fn test_exists_named_x_argument() {
        let code = r#"exists(x = "apple")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apple");
    }

    #[test]
    fn test_exists_extra_args_still_declares() {
        // `where=`/`inherits=` do not change that the user named `apple`.
        let code = r#"exists("apple", inherits = FALSE)"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apple");
    }

    #[test]
    fn test_exists_non_literal_argument_skipped() {
        // A variable name (not a string literal) is not statically determinable.
        let code = "exists(varname)";
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_exists_computed_argument_skipped() {
        let code = r#"exists(paste0("ap", "ple"))"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_exists_empty_call_skipped() {
        let code = "exists()";
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_exists_empty_string_skipped() {
        // `exists("")` names nothing usable — declare nothing (parity with the
        // `# raven: var` directive, which also skips an empty name).
        let code = r#"exists("")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_exists_whitespace_string_skipped() {
        // A whitespace-only name is also skipped, matching `# raven: var`'s
        // `name.trim().is_empty()` rule.
        let code = r#"exists("   ")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_exists_non_syntactic_name_extracted_raw() {
        // The detector returns the RAW string contents (`my var`); call-site
        // backtick-wrapping (`` `my var` ``) happens later, during declared-symbol
        // synthesis in `scope.rs` (via `callee_name_for_match`), so it matches a
        // `` `my var` `` use.
        let code = r#"exists("my var")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "my var");
    }

    #[test]
    fn test_exists_multiple_calls() {
        let code = "exists(\"a\")\nexists(\"b\")";
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[0].line, 0);
        assert_eq!(calls[1].name, "b");
        assert_eq!(calls[1].line, 1);
    }

    #[test]
    fn test_non_exists_call_ignored() {
        let code = r#"file.exists("apple")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_rm_inside_function() {
        let code = "f <- function() { rm(x) }";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_utf16_column() {
        // Test with emoji before rm() to verify UTF-16 column calculation
        // 🎉 is 4 bytes in UTF-8, 2 UTF-16 code units
        let code = "🎉; rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
        // Column should be 2 (emoji) + 2 ("; ") = 4 in UTF-16
        assert_eq!(rm_calls[0].column, 4);
    }

    // ==================== list= argument parsing tests ====================

    #[test]
    fn test_rm_list_single_string_double_quotes() {
        let code = r#"rm(list = "myvar")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["myvar"]);
    }

    #[test]
    fn test_rm_list_single_string_single_quotes() {
        let code = "rm(list = 'myvar')";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["myvar"]);
    }

    #[test]
    fn test_rm_list_c_multiple_strings() {
        let code = r#"rm(list = c("a", "b", "c"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_rm_list_c_single_string() {
        let code = r#"rm(list = c("x"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_list_c_empty() {
        let code = "rm(list = c())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // Empty c() produces no symbols, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_variable_skipped() {
        // rm(list = var) - variable reference should be skipped
        let code = "rm(list = my_var)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_ls_skipped() {
        // rm(list = ls()) - function call should be skipped
        let code = "rm(list = ls())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_ls_pattern_skipped() {
        // rm(list = ls(pattern = "^tmp")) - function call should be skipped
        let code = r#"rm(list = ls(pattern = "^tmp"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_paste0_skipped() {
        // rm(list = paste0("x", 1:3)) - function call should be skipped
        let code = r#"rm(list = paste0("x", 1:3))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_c_with_mixed_args() {
        // c() with mixed string and non-string args - only extract strings
        let code = r#"rm(list = c("a", var, "b"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        // Only string literals are extracted
        assert_eq!(rm_calls[0].symbols, vec!["a", "b"]);
    }

    #[test]
    fn test_rm_bare_and_list_combined() {
        // rm(x, y, list = c("a", "b")) - should extract all symbols
        let code = r#"rm(x, y, list = c("a", "b"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y", "a", "b"]);
    }

    #[test]
    fn test_remove_list_single_string() {
        // remove() should work the same as rm()
        let code = r#"remove(list = "x")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_remove_list_c_multiple() {
        let code = r#"remove(list = c("a", "b"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b"]);
    }

    #[test]
    fn test_rm_list_number_skipped() {
        // rm(list = 123) - number should be skipped
        let code = "rm(list = 123)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_expression_skipped() {
        // rm(list = x + y) - expression should be skipped
        let code = "rm(list = x + y)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    // ==================== envir= argument filtering tests ====================

    #[test]
    fn test_rm_without_envir_processed() {
        // rm(x) without envir= should be processed normally
        let code = "rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_envir_globalenv_processed() {
        // rm(x, envir = globalenv()) should be processed normally
        let code = "rm(x, envir = globalenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_envir_dot_globalenv_processed() {
        // rm(x, envir = .GlobalEnv) should be processed normally
        let code = "rm(x, envir = .GlobalEnv)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_envir_custom_skipped() {
        // rm(x, envir = my_env) should be skipped (non-default environment)
        let code = "rm(x, envir = my_env)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_envir_new_env_skipped() {
        // rm(x, envir = new.env()) should be skipped
        let code = "rm(x, envir = new.env())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_envir_parent_frame_skipped() {
        // rm(x, envir = parent.frame()) should be skipped
        let code = "rm(x, envir = parent.frame())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_envir_baseenv_skipped() {
        // rm(x, envir = baseenv()) should be skipped
        let code = "rm(x, envir = baseenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_multiple_symbols_with_envir_custom_skipped() {
        // rm(x, y, z, envir = my_env) should be skipped entirely
        let code = "rm(x, y, z, envir = my_env)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_with_envir_custom_skipped() {
        // rm(list = c("a", "b"), envir = my_env) should be skipped
        let code = r#"rm(list = c("a", "b"), envir = my_env)"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_with_envir_globalenv_processed() {
        // rm(list = c("a", "b"), envir = globalenv()) should be processed
        let code = r#"rm(list = c("a", "b"), envir = globalenv())"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b"]);
    }

    #[test]
    fn test_remove_with_envir_custom_skipped() {
        // remove(x, envir = my_env) should be skipped
        let code = "remove(x, envir = my_env)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_remove_with_envir_globalenv_processed() {
        // remove(x, envir = globalenv()) should be processed
        let code = "remove(x, envir = globalenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_mixed_with_envir_globalenv_processed() {
        // rm(x, list = "y", envir = .GlobalEnv) should be processed
        let code = r#"rm(x, list = "y", envir = .GlobalEnv)"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y"]);
    }

    #[test]
    fn test_multiple_rm_calls_with_different_envir() {
        // Mix of rm() calls with different envir= values
        let code = "rm(a)\nrm(b, envir = my_env)\nrm(c, envir = globalenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // Only rm(a) and rm(c, envir = globalenv()) should be detected
        assert_eq!(rm_calls.len(), 2);
        assert_eq!(rm_calls[0].symbols, vec!["a"]);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[1].symbols, vec!["c"]);
        assert_eq!(rm_calls[1].line, 2);
    }

    // ==================== error/missing AST node tests ====================

    #[test]
    fn test_rm_malformed_empty_arg_skipped() {
        // rm(,) - malformed with missing argument
        let code = "rm(,)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_malformed_list_missing_value_skipped() {
        // rm(list = ) - malformed with missing value
        let code = "rm(list = )";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    // ==================== library()/require()/loadNamespace() detection tests ====================

    #[test]
    fn test_library_bare_identifier() {
        // library(dplyr) - bare identifier
        // Validates: Requirement 1.1
        let code = "library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[0].line, 0);
    }

    #[test]
    fn test_library_double_quoted_string() {
        // library("dplyr") - double-quoted string
        // Validates: Requirement 1.2
        let code = r#"library("dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_library_single_quoted_string() {
        // library('dplyr') - single-quoted string
        // Validates: Requirement 1.3
        let code = "library('dplyr')";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_require_bare_identifier() {
        // require(dplyr) - bare identifier
        // Validates: Requirement 1.4
        let code = "require(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_require_quoted_string() {
        // require("dplyr") - quoted string
        // Validates: Requirement 1.4
        let code = r#"require("dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_load_namespace_quoted_string() {
        // loadNamespace("dplyr") - quoted string
        // Validates: Requirement 1.5
        let code = r#"loadNamespace("dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_load_namespace_bare_identifier() {
        // loadNamespace(dplyr) - bare identifier
        // Validates: Requirement 1.5
        let code = "loadNamespace(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_attaches_flag_distinguishes_library_from_load_namespace() {
        // `library`/`require` attach (bare names become available); `loadNamespace`
        // only loads the namespace for qualified `pkg::` access. The `attaches`
        // flag must reflect that distinction — see `LibraryCall::attaches`.
        for (code, expected) in [
            ("library(shiny)", true),
            ("require(shiny)", true),
            (r#"loadNamespace("shiny")"#, false),
            ("loadNamespace(shiny)", false),
        ] {
            let tree = parse_r(code);
            let lib_calls = detect_library_calls(&tree, code);
            assert_eq!(lib_calls.len(), 1, "exactly one call detected for `{code}`");
            assert_eq!(lib_calls[0].package, "shiny", "package name for `{code}`");
            assert_eq!(
                lib_calls[0].attaches, expected,
                "attaches flag for `{code}`"
            );
        }
    }

    #[test]
    fn test_library_variable_skipped() {
        // library(pkg_name) where pkg_name is a variable - should be skipped
        // Validates: Requirement 1.6
        // Note: We can't distinguish a variable from a bare package name statically,
        // so this test verifies that we DO detect it (as we treat all identifiers as package names)
        let code = "pkg_name <- 'dplyr'\nlibrary(pkg_name)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        // We detect it because we can't distinguish variable from package name
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "pkg_name");
    }

    #[test]
    fn test_library_expression_skipped() {
        // library(paste0("dp", "lyr")) - expression should be skipped
        // Validates: Requirement 1.6
        let code = r#"library(paste0("dp", "lyr"))"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_character_only_true_skipped() {
        // library("dplyr", character.only = TRUE) - should be skipped
        // Validates: Requirement 1.7
        let code = r#"library("dplyr", character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_character_only_t_skipped() {
        // library("dplyr", character.only = T) - should be skipped
        // Validates: Requirement 1.7
        let code = r#"library("dplyr", character.only = T)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_character_only_false_processed() {
        // library("dplyr", character.only = FALSE) - should be processed
        let code = r#"library("dplyr", character.only = FALSE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_require_character_only_true_skipped() {
        // require("dplyr", character.only = TRUE) - should be skipped
        // Validates: Requirement 1.7
        let code = r#"require("dplyr", character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_multiple_library_calls() {
        // Multiple library calls in document order
        // Validates: Requirement 1.8
        let code = "library(dplyr)\nlibrary(ggplot2)\nrequire(tidyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 3);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[0].line, 0);
        assert_eq!(lib_calls[1].package, "ggplot2");
        assert_eq!(lib_calls[1].line, 1);
        assert_eq!(lib_calls[2].package, "tidyr");
        assert_eq!(lib_calls[2].line, 2);
    }

    #[test]
    fn test_library_named_package_argument() {
        // library(package = dplyr) - named argument
        let code = "library(package = dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_library_named_package_argument_quoted() {
        // library(package = "dplyr") - named argument with string
        let code = r#"library(package = "dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_library_position() {
        // Test position tracking
        let code = "x <- 1\nlibrary(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].line, 1);
        // Column should be at end of call
        assert_eq!(lib_calls[0].column, 14); // "library(dplyr)" is 14 chars
    }

    #[test]
    fn test_library_position_with_offset() {
        // Test position with offset on same line
        let code = "x <- 1; library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].line, 0);
        // Column should be at end of call: "x <- 1; library(dplyr)" = 22 chars
        assert_eq!(lib_calls[0].column, 22);
    }

    #[test]
    fn test_library_with_utf16_column() {
        // Test with emoji before library() to verify UTF-16 column calculation
        // 🎉 is 4 bytes in UTF-8, 2 UTF-16 code units
        let code = "🎉; library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
        // Column should be: 2 (emoji) + 2 ("; ") + 14 ("library(dplyr)") = 18 in UTF-16
        assert_eq!(lib_calls[0].column, 18);
    }

    #[test]
    fn test_library_inside_function() {
        // library() inside a function body
        let code = "f <- function() { library(dplyr) }";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
        // function_scope is None for now (will be populated in task 6.2)
        assert!(lib_calls[0].function_scope.is_none());
    }

    #[test]
    fn test_library_empty_call_skipped() {
        // library() with no arguments should be skipped
        let code = "library()";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_non_library_call_ignored() {
        // Other function calls should be ignored
        let code = "print(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_with_other_arguments() {
        // library() with additional arguments
        let code = "library(dplyr, quietly = TRUE, warn.conflicts = FALSE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_mixed_library_and_source_calls() {
        // Mix of library() and source() calls
        let code = r#"library(dplyr)
source("utils.R")
library(ggplot2)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "ggplot2");
    }

    #[test]
    fn test_library_function_scope_is_none() {
        // Verify function_scope is None (will be populated later)
        let code = "library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert!(lib_calls[0].function_scope.is_none());
    }

    // ==================== apply-family library detection (#172) ====================

    #[test]
    fn test_apply_inline_c_with_library() {
        // sapply(c("dplyr","tidyr"), library, character.only = TRUE)
        // Validates issue #172 — the "inline c()" path.
        let code = r#"sapply(c("dplyr", "tidyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
        // Both share the apply call's end position.
        assert_eq!(lib_calls[0].line, 0);
        assert_eq!(lib_calls[1].line, 0);
        assert_eq!(lib_calls[0].column, lib_calls[1].column);
        assert!(lib_calls[0].function_scope.is_none());
    }

    #[test]
    fn test_apply_lapply_inline_c_with_require() {
        let code = r#"lapply(c("dplyr"), require, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_apply_vapply_inline_c() {
        // vapply has extra signature args; library FUN + c() X still detects.
        let code = r#"vapply(c("dplyr","tidyr"), require, logical(1), character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_mapply_inline_c() {
        // mapply puts FUN first; we're position-agnostic so it still matches.
        let code = r#"mapply(library, c("dplyr","tidyr"), character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_with_named_x_arg_skipped() {
        // A c() inside a *named* arg is not picked up — only positional X args
        // are considered. Documents the limitation.
        let code = r#"sapply(X = c("dplyr"), FUN = require, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_purrr_bare_walk() {
        let code = r#"walk(c("dplyr","tidyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_purrr_qualified_map() {
        let code = r#"purrr::map(c("dplyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_apply_purrr_qualified_map_chr() {
        let code = r#"purrr::map_chr(c("dplyr","tidyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
    }

    #[test]
    fn test_apply_other_namespace_not_detected() {
        // foo::map(...) is not purrr — skip.
        let code = r#"foo::map(c("dplyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_single_arrow_assignment() {
        // Same-file variable assigned exactly once via `<-` to a c() of strings.
        let code = "libs <- c(\"dplyr\", \"tidyr\")\nsapply(libs, require, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
        assert_eq!(lib_calls[0].line, 1);
        assert_eq!(lib_calls[1].line, 1);
    }

    #[test]
    fn test_apply_var_equals_assignment() {
        let code = "libs = c(\"dplyr\", \"tidyr\")\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_var_assign_call() {
        let code = "assign(\"libs\", c(\"dplyr\", \"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_var_assignment_after_apply_call_skipped() {
        // Variable assigned *after* the apply call must not resolve.
        let code = "sapply(libs, library, character.only = TRUE)\nlibs <- c(\"dplyr\")";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_multiple_assignments_skipped() {
        let code = "libs <- c(\"dplyr\")\nlibs <- c(\"tidyr\")\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_super_assignment_disqualifies() {
        // <<- alone counts but doesn't extract — single-assignment but no
        // static packages means the binding doesn't resolve.
        let code = "libs <<- c(\"dplyr\")\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_function_param_shadow_disqualifies() {
        // A function parameter named `libs` increments the count and
        // disqualifies the global binding.
        let code = "libs <- c(\"dplyr\")\nf <- function(libs) {}\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_dynamic_x_paste0_skipped() {
        let code = r#"sapply(paste0("dp", "lyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_dynamic_x_setdiff_skipped() {
        let code = r#"sapply(setdiff(c("a","b"), "b"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_dynamic_x_c_with_var_skipped() {
        // c() containing a non-string argument disqualifies the X arg.
        let code = "libs1 <- c(\"a\")\nsapply(c(libs1, \"b\"), library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_anonymous_fun_skipped() {
        // \(x) library(x) — FUN is not a bare identifier so the apply must not
        // pick up "dplyr". The inner `library(x)` may still be detected with
        // package="x" by the existing direct-library detector — that's
        // pre-existing loose behavior; we only assert the apply path didn't
        // fire by checking that no LibraryCall mentions "dplyr".
        let code = r#"sapply(c("dplyr"), \(x) library(x), character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert!(
            lib_calls.iter().all(|c| c.package != "dplyr"),
            "apply path should not emit dplyr; got {:?}",
            lib_calls
        );
    }

    #[test]
    fn test_apply_no_character_only_skipped() {
        let code = r#"sapply(c("dplyr"), library)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_character_only_false_skipped() {
        let code = r#"sapply(c("dplyr"), library, character.only = FALSE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_loadnamespace_fun_skipped() {
        // loadNamespace is intentionally not in the FUN allowlist.
        let code = r#"sapply(c("dplyr"), loadNamespace, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_position_at_call_end_utf16() {
        // 🎉 is 4 UTF-8 bytes / 2 UTF-16 code units.
        let code = "🎉; sapply(c(\"dplyr\",\"tidyr\"), library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        let total_utf16 = code.encode_utf16().count() as u32;
        for call in &lib_calls {
            assert_eq!(call.line, 0);
            assert_eq!(call.column, total_utf16);
        }
    }

    #[test]
    fn test_apply_issue_172_exact_example() {
        // Issue #172 — exact pattern from the report.
        let code =
            "libs <- c(\"lib1\", \"lib2\", \"lib3\")\nsapply(libs, require, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 3);
        assert_eq!(lib_calls[0].package, "lib1");
        assert_eq!(lib_calls[1].package, "lib2");
        assert_eq!(lib_calls[2].package, "lib3");
        for call in &lib_calls {
            assert_eq!(call.line, 1);
        }
    }

    #[test]
    fn test_apply_issue_172_via_extract_metadata() {
        let code =
            "libs <- c(\"lib1\", \"lib2\", \"lib3\")\nsapply(libs, require, character.only = TRUE)";
        let meta = crate::cross_file::extract_metadata(code);
        let pkgs: Vec<&str> = meta
            .library_calls
            .iter()
            .map(|c| c.package.as_str())
            .collect();
        assert_eq!(pkgs, vec!["lib1", "lib2", "lib3"]);
    }

    #[test]
    fn test_apply_library_in_extra_position_skipped() {
        // FUN is `identity`; `library` is just an extra positional arg passed
        // through `...` to identity, which doesn't load anything.
        let code = r#"sapply(c("dplyr"), identity, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_library_at_x_position_skipped() {
        // For sapply/lapply/etc., X is at position 0 and FUN at position 1.
        // Swapping them is not a real library load.
        let code = r#"sapply(library, c("dplyr"), character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_assign_call_named_args() {
        // assign(x = "libs", value = c(...)) — the named-arg form should
        // count and resolve just like the positional form.
        let code = "assign(x = \"libs\", value = c(\"dplyr\", \"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_var_assign_named_overrides_disqualifies() {
        // libs is assigned twice — once via `<-`, once via named-arg assign().
        // The named assign() must count toward the multi-assignment rule.
        let code = "libs <- c(\"dplyr\")\nassign(x = \"libs\", value = c(\"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_map_if_library_in_predicate_slot_not_detected() {
        // map_if has signature (.x, .p, .f) — the predicate is at position 1,
        // not the FUN. Putting `library` in the predicate slot doesn't load
        // anything; we should not match it. Drop map_if/map_at from the
        // supported apply set rather than guessing positions per signature.
        let code = r#"map_if(c("dplyr"), library, is.character, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_assign_partial_named() {
        // R does partial matching after exact: `val` is a unique prefix of
        // `value` among assign()'s formals, so `val = ...` binds `value`.
        let code = "assign(x = \"libs\", val = c(\"dplyr\", \"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_var_assign_partial_named_overrides_disqualifies() {
        // Same as test_apply_var_assign_named_overrides_disqualifies but the
        // second assign uses partial-match `val` for `value`.
        let code = "libs <- c(\"dplyr\")\nassign(x = \"libs\", val = c(\"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_assign_exact_and_partial_value_skipped() {
        // R does exact-name matching first, then partial. With both an exact
        // `value = ...` and a partial `val = ...` present, R errors with
        // "matched by multiple actual arguments" and the assignment never
        // happens; we must not record a static binding from it.
        let code = "assign(x = \"libs\", val = c(\"dplyr\"), value = c(\"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_assign_duplicate_exact_value_skipped() {
        // Same idea: two exact `value =` args also error in R.
        let code = "assign(x = \"libs\", value = c(\"dplyr\"), value = c(\"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    // ==================== tar_option_set package detection (#637) ====================

    /// Convenience: the detected calls minus the `targets` load itself.
    fn tar_calls(code: &str) -> Vec<LibraryCall> {
        let tree = parse_r(code);
        detect_library_calls(&tree, code)
            .into_iter()
            .filter(|c| c.package != "targets")
            .collect()
    }

    #[test]
    fn test_tar_option_set_inline_c_with_library_targets() {
        let code = "library(targets)\ntar_option_set(packages = c(\"dplyr\", \"tidyr\"))";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 2, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert_eq!(calls[1].package, "tidyr");
        assert!(calls[0].attaches);
        assert!(calls[1].attaches);
        // Each package is anchored at its OWN string literal, not the call end.
        assert_eq!(calls[0].line, 1);
        assert_eq!(calls[1].line, 1);
        assert_ne!(
            calls[0].column, calls[1].column,
            "per-literal anchoring: distinct literals have distinct columns"
        );
        assert!(calls[0].column < calls[1].column);
        assert!(calls[0].function_scope.is_none());
    }

    #[test]
    fn test_tar_option_set_single_string_literal() {
        let code = "library(targets)\ntar_option_set(packages = \"dplyr\")";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert!(calls[0].attaches);
        assert_eq!(calls[0].line, 1);
    }

    #[test]
    fn test_tar_option_set_qualified_without_library_targets() {
        // Qualified spellings are accepted unconditionally — no library(targets)
        // needed.
        let code = "targets::tar_option_set(packages = c(\"dplyr\"))";
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");

        let code = "targets:::tar_option_set(packages = c(\"dplyr\"))";
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
    }

    #[test]
    fn test_tar_option_set_bare_without_library_targets_skipped() {
        // The bare spelling requires targets to be attached somewhere in the
        // file (targets-in-play gate).
        let code = "tar_option_set(packages = c(\"dplyr\"))";
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert_eq!(calls.len(), 0, "got: {calls:?}");
    }

    #[test]
    fn test_tar_option_set_bare_gate_is_position_independent() {
        // library(targets) AFTER the call still satisfies the gate.
        let code = "tar_option_set(packages = \"dplyr\")\nlibrary(targets)";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
    }

    #[test]
    fn test_tar_option_set_bare_loadnamespace_targets_does_not_satisfy_gate() {
        // loadNamespace("targets") does not attach, so it does not enable the
        // bare spelling.
        let code = "loadNamespace(\"targets\")\ntar_option_set(packages = c(\"dplyr\"))";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 0, "got: {calls:?}");
    }

    #[test]
    fn test_tar_option_set_var_resolved_anchored_at_call_end() {
        let code =
            "library(targets)\npkgs <- c(\"dplyr\", \"tidyr\")\ntar_option_set(packages = pkgs)";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 2, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert_eq!(calls[1].package, "tidyr");
        // No literal at the call site — both share the call's end position.
        assert_eq!(calls[0].line, 2);
        assert_eq!(calls[1].line, 2);
        assert_eq!(calls[0].column, calls[1].column);
        let call_len = "tar_option_set(packages = pkgs)".len() as u32;
        assert_eq!(calls[0].column, call_len);
    }

    #[test]
    fn test_tar_option_set_positional_packages_skipped() {
        // tar_option_set's first formal is tidy_eval, so positional matching
        // is not attempted (documented limitation).
        let code = "library(targets)\ntar_option_set(TRUE, c(\"dplyr\"))";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 0, "got: {calls:?}");
    }

    #[test]
    fn test_tar_option_set_dynamic_value_skipped() {
        let code = "library(targets)\ntar_option_set(packages = getOption(\"my.pkgs\"))";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 0, "got: {calls:?}");
    }

    #[test]
    fn test_tar_option_set_no_packages_arg_skipped() {
        let code = "library(targets)\ntar_option_set(format = \"qs\")";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 0, "got: {calls:?}");
    }

    #[test]
    fn test_tar_option_set_character0_and_empty_c_skipped() {
        let code = "library(targets)\ntar_option_set(packages = character(0))";
        assert_eq!(tar_calls(code).len(), 0);
        let code = "library(targets)\ntar_option_set(packages = c())";
        assert_eq!(tar_calls(code).len(), 0);
    }

    #[test]
    fn test_tar_option_set_unrelated_named_args_still_detected() {
        let code = "library(targets)\ntar_option_set(packages = c(\"dplyr\"), format = \"qs\", memory = \"transient\")";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
    }

    #[test]
    fn test_tar_option_set_multiple_calls_union() {
        // targets' runtime is last-call-wins, but raven deliberately unions
        // (favoring false negatives) — see try_parse_tar_option_set_call.
        let code = "library(targets)\ntar_option_set(packages = c(\"dplyr\"))\ntar_option_set(packages = c(\"tidyr\"))";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 2, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert_eq!(calls[1].package, "tidyr");
    }

    #[test]
    fn test_tar_option_set_multiline_anchors_at_literal_line() {
        // The diagnostic range is built from LibraryCall.line, and `# nolint`
        // suppression is line-keyed, so each package must anchor at its own
        // literal's line — not the closing paren's.
        let code = "library(targets)\ntar_option_set(\n  packages = c(\n    \"dplyr\",\n    \"tidyr\"\n  ),\n  format = \"qs\"\n)";
        let calls = tar_calls(code);
        assert_eq!(calls.len(), 2, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert_eq!(calls[0].line, 3, "anchored at its own literal's line");
        assert_eq!(calls[1].package, "tidyr");
        assert_eq!(calls[1].line, 4, "anchored at its own literal's line");
    }

    #[test]
    fn extract_attached_packages_includes_top_level_tar_option_set() {
        let pkgs =
            extract_attached_packages("library(targets)\ntar_option_set(packages = c(\"dplyr\"))");
        assert!(pkgs.contains("targets"));
        assert!(pkgs.contains("dplyr"));

        // Qualified spelling needs no library(targets).
        let pkgs = extract_attached_packages("targets::tar_option_set(packages = \"dplyr\")");
        assert!(pkgs.contains("dplyr"));
    }

    #[test]
    fn extract_attached_packages_excludes_function_body_tar_option_set() {
        let pkgs = extract_attached_packages(
            "library(targets)\nf <- function() tar_option_set(packages = c(\"dplyr\"))",
        );
        assert!(!pkgs.contains("dplyr"), "got: {pkgs:?}");
    }

    #[test]
    fn extract_attached_packages_excludes_quote_wrapped_tar_option_set() {
        let pkgs = extract_attached_packages(
            "library(targets)\nq <- quote(tar_option_set(packages = c(\"dplyr\")))",
        );
        assert!(!pkgs.contains("dplyr"), "got: {pkgs:?}");
    }

    // ==================== system.file() detection in source() ====================

    #[test]
    fn test_source_system_file_single_part() {
        let code = r#"source(system.file("helper.R", package = "Matrix"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].path.is_empty());
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["helper.R"]);
        assert_eq!(sf.package, "Matrix");
    }

    #[test]
    fn test_source_system_file_multi_part() {
        let code = r#"source(system.file("a", "b.R", package = "P"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["a", "b.R"]);
        assert_eq!(sf.package, "P");
    }

    #[test]
    fn test_source_system_file_non_literal_arg_skipped() {
        // A variable positional arg → bail (unresolved = no ForwardSource)
        let code = r#"source(system.file(x, package = "P"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_system_file_no_package_skipped() {
        // No package= arg → not parseable
        let code = r#"source(system.file("helper.R"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_system_file_variable_package_skipped() {
        // Non-literal package= → bail
        let code = r#"source(system.file("helper.R", package = pkg))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_system_file_lib_loc_rejected() {
        // lib.loc alters search path — unresolvable statically
        let code = r#"source(system.file("x.R", package = "p", lib.loc = foo))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_system_file_fsep_rejected() {
        // Non-default fsep alters path construction — unresolvable statically
        let code = r#"source(system.file("x.R", package = "p", fsep = "\\"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_system_file_must_work_accepted() {
        // mustWork doesn't affect path layout — still resolvable
        let code = r#"source(system.file("x.R", package = "p", mustWork = FALSE))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["x.R"]);
        assert_eq!(sf.package, "p");
    }

    #[test]
    fn test_source_system_file_lib_loc_dot_library_accepted() {
        // .Library is the default library path — safe to resolve as-if-absent
        let code = r#"source(system.file("test-tools-1.R", package = "Matrix", lib.loc = .Library), keep.source = FALSE)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["test-tools-1.R"]);
        assert_eq!(sf.package, "Matrix");
    }

    #[test]
    fn test_source_system_file_lib_loc_lib_paths_accepted() {
        // .libPaths() returns the default search paths — safe to resolve
        let code = r#"source(system.file("x.R", package = "p", lib.loc = .libPaths()))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["x.R"]);
        assert_eq!(sf.package, "p");
    }

    #[test]
    fn test_source_system_file_lib_loc_null_accepted() {
        // lib.loc = NULL is identical to omitting lib.loc — safe to resolve
        let code = r#"source(system.file("x.R", package = "p", lib.loc = NULL))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["x.R"]);
        assert_eq!(sf.package, "p");
    }

    #[test]
    fn test_source_system_file_fsep_default_accepted() {
        // fsep = "/" is the default — no-op, safe to resolve
        let code = r#"source(system.file("x.R", package = "p", fsep = "/"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["x.R"]);
        assert_eq!(sf.package, "p");
    }
}

// ============================================================================
// Property-Based Tests for Library Call Detection
// ============================================================================

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;
    use tree_sitter::Parser;

    /// Parse R source code into a tree-sitter `Tree`.
    ///
    /// # Examples
    ///
    /// ```
    /// let code = "x <- 1";
    /// let tree = parse_r(code);
    /// // root node kind for R source files is "source"
    /// assert_eq!(tree.root_node().kind(), "source");
    /// ```
    fn parse_r(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    /// R reserved words that cannot be used as package names
    const R_RESERVED: &[&str] = &[
        "if", "else", "for", "in", "while", "repeat", "next", "break", "function", "NA", "NaN",
        "Inf", "NULL", "TRUE", "FALSE", "T", "F",
    ];

    /// Determine whether a string is a valid R package name (non-empty and not an R reserved word).
    ///
    /// # Returns
    ///
    /// `true` if the name is non-empty and not an R reserved word, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// assert!(is_valid_package_name("dplyr"));
    /// assert!(!is_valid_package_name(""));
    /// assert!(!is_valid_package_name("if")); // reserved word
    /// ```
    fn is_valid_package_name(name: &str) -> bool {
        !R_RESERVED.contains(&name) && !name.is_empty()
    }

    /// Strategy that generates valid R package names.
    ///
    /// The produced strings start with a lowercase ASCII letter, contain only
    /// lowercase ASCII letters, digits, and dots, and are at most 9 characters long.
    /// Reserved or otherwise invalid package names are excluded by the strategy's filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// proptest!(|(name in crate::package_name())| {
    ///     let first = name.chars().next().unwrap();
    ///     assert!(first.is_ascii_lowercase());
    ///     assert!(name.len() <= 9);
    ///     assert!(name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.'));
    /// });
    /// ```
    fn package_name() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9\\.]{0,8}".prop_filter("not reserved", |s| is_valid_package_name(s))
    }

    /// Produces a proptest strategy that yields one of the strings `"library"`, `"require"`, or `"loadNamespace"`.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// let strat = crate::library_function();
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let tree = strat.new_tree(&mut runner).unwrap();
    /// let value = tree.current();
    /// assert!(value == "library" || value == "require" || value == "loadNamespace");
    /// ```
    fn library_function() -> impl Strategy<Value = &'static str> {
        prop_oneof![Just("library"), Just("require"), Just("loadNamespace"),]
    }

    /// Generate a quote style for package names
    #[derive(Debug, Clone, Copy)]
    enum QuoteStyle {
        None,   // library(dplyr)
        Double, // library("dplyr")
        Single, // library('dplyr')
    }

    /// Produces a proptest Strategy that yields one of the `QuoteStyle` variants.
    ///
    /// The strategy generates `QuoteStyle::None`, `QuoteStyle::Double`, or `QuoteStyle::Single`.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// // generate a single value from the strategy
    /// let mut runner = TestRunner::default();
    /// let value = quote_style().new_tree(&mut runner).unwrap().current();
    /// match value {
    ///     QuoteStyle::None | QuoteStyle::Double | QuoteStyle::Single => (),
    /// }
    /// ```
    fn quote_style() -> impl Strategy<Value = QuoteStyle> {
        prop_oneof![
            Just(QuoteStyle::None),
            Just(QuoteStyle::Double),
            Just(QuoteStyle::Single),
        ]
    }

    /// A library call specification for code generation
    #[derive(Debug, Clone)]
    struct LibraryCallSpec {
        func: &'static str,
        package: String,
        quote_style: QuoteStyle,
        use_named_arg: bool,
    }

    /// Generates an arbitrary `LibraryCallSpec` strategy for property-based tests.
    ///
    /// The strategy produces tuples describing a library-like call: the function name (`library`, `require`, or `loadNamespace`),
    /// a package name, the string quote style to use (none, single, or double), and a boolean indicating whether the package
    /// is supplied with a named `package=` argument.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let tree = library_call_spec().new_tree(&mut runner).unwrap();
    /// let spec = tree.current();
    /// // `spec` contains generated fields: `func`, `package`, `quote_style`, and `use_named_arg`.
    /// assert!(!spec.package.is_empty());
    /// ```
    fn library_call_spec() -> impl Strategy<Value = LibraryCallSpec> {
        (
            library_function(),
            package_name(),
            quote_style(),
            any::<bool>(),
        )
            .prop_map(
                |(func, package, quote_style, use_named_arg)| LibraryCallSpec {
                    func,
                    package,
                    quote_style,
                    use_named_arg,
                },
            )
    }

    /// Render an R library-like call from a specification.
    ///
    /// Constructs an R call string using the specification's function name, package
    /// name, quoting style, and whether to use a named `package =` argument.
    ///
    /// # Returns
    ///
    /// A `String` containing the R expression (for example `library("dplyr")` or
    /// `require(package = 'pkg')`).
    ///
    /// # Examples
    ///
    /// ```
    /// let spec = LibraryCallSpec {
    ///     func: "library".into(),
    ///     package: "dplyr".into(),
    ///     quote_style: QuoteStyle::Double,
    ///     use_named_arg: false,
    /// };
    /// let code = generate_library_call_code(&spec);
    /// assert_eq!(code, "library(\"dplyr\")");
    /// ```
    fn generate_library_call_code(spec: &LibraryCallSpec) -> String {
        let quoted_pkg = match spec.quote_style {
            QuoteStyle::None => spec.package.clone(),
            QuoteStyle::Double => format!("\"{}\"", spec.package),
            QuoteStyle::Single => format!("'{}'", spec.package),
        };

        if spec.use_named_arg {
            format!("{}(package = {})", spec.func, quoted_pkg)
        } else {
            format!("{}({})", spec.func, quoted_pkg)
        }
    }

    /// Generates a proptest strategy that yields R source text containing 1 to 5 `library`/`require`/`loadNamespace` calls interleaved with simple filler statements, together with the corresponding specs for each library call.
    ///
    /// The returned strategy produces a tuple `(code, specs)` where `code` is the generated R code as a single string (lines joined with `\n`) and `specs` is a vector of `LibraryCallSpec` describing each generated library call in document order.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use proptest::strategy::Strategy;
    /// // `r_code_with_library_calls` yields `(String, Vec<LibraryCallSpec>)`
    /// let strat = r_code_with_library_calls();
    /// // Use the strategy in a proptest test or sample from it via a test runner.
    /// let _ = strat;
    /// ```
    fn r_code_with_library_calls() -> impl Strategy<Value = (String, Vec<LibraryCallSpec>)> {
        // Generate 1-5 library calls
        prop::collection::vec(library_call_spec(), 1..=5)
            .prop_flat_map(|specs| {
                // Generate 0-3 filler lines between each call
                let num_fillers = specs.len() + 1;
                let filler_counts = prop::collection::vec(0..4usize, num_fillers);
                (Just(specs), filler_counts)
            })
            .prop_map(|(specs, filler_counts)| {
                let mut lines = Vec::new();

                // Add filler before first call
                for _ in 0..filler_counts[0] {
                    lines.push("x <- 1".to_string());
                }

                // Add library calls with fillers between them
                for (i, spec) in specs.iter().enumerate() {
                    lines.push(generate_library_call_code(spec));

                    // Add filler after this call
                    if i + 1 < filler_counts.len() {
                        for _ in 0..filler_counts[i + 1] {
                            lines.push("y <- 2".to_string());
                        }
                    }
                }

                let code = lines.join("\n");
                (code, specs)
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // ============================================================================
        // Feature: package-function-awareness, Property 1: Library Call Detection Completeness
        // **Validates: Requirements 1.1, 1.4, 1.5, 1.8**
        //
        // For any R source file containing library(), require(), or loadNamespace() calls
        // with static string package names, the Library_Call_Detector SHALL detect all
        // such calls and return them in document order with correct package names and positions.
        // ============================================================================

        /// Property 1: All library/require/loadNamespace calls with static package names are detected
        #[test]
        fn prop_library_call_detection_completeness((code, specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // 1. All calls should be detected (completeness)
            prop_assert_eq!(
                detected.len(),
                specs.len(),
                "Expected {} library calls, but detected {}. Code:\n{}",
                specs.len(),
                detected.len(),
                code
            );

            // 2. Package names should be correctly extracted
            for (i, (detected_call, spec)) in detected.iter().zip(specs.iter()).enumerate() {
                prop_assert_eq!(
                    &detected_call.package,
                    &spec.package,
                    "Package name mismatch at index {}. Expected '{}', got '{}'. Code:\n{}",
                    i,
                    spec.package,
                    detected_call.package,
                    code
                );
            }
        }

        /// Property 1 extended: Calls are returned in document order (sorted by line, then column)
        #[test]
        fn prop_library_calls_in_document_order((code, _specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // Verify document order: each call should be at same or later position than previous
            for i in 1..detected.len() {
                let prev = &detected[i - 1];
                let curr = &detected[i];

                let prev_pos = (prev.line, prev.column);
                let curr_pos = (curr.line, curr.column);

                prop_assert!(
                    prev_pos <= curr_pos,
                    "Library calls not in document order: call {} at ({}, {}) comes after call {} at ({}, {}). Code:\n{}",
                    i - 1, prev.line, prev.column,
                    i, curr.line, curr.column,
                    code
                );
            }
        }

        /// Property 1 extended: Positions are valid (within code bounds)
        #[test]
        fn prop_library_call_positions_valid((code, _specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            let line_count = code.lines().count() as u32;

            for (i, call) in detected.iter().enumerate() {
                // Line should be within bounds
                prop_assert!(
                    call.line < line_count,
                    "Library call {} has invalid line {}, but code only has {} lines. Code:\n{}",
                    i, call.line, line_count, code
                );

                // Column should be within the line's length (in UTF-16 code units)
                if let Some(line_text) = code.lines().nth(call.line as usize) {
                    let line_len_utf16: u32 = line_text.encode_utf16().count() as u32;
                    prop_assert!(
                        call.column <= line_len_utf16,
                        "Library call {} has invalid column {} on line {}, but line only has {} UTF-16 code units. Line: '{}'. Code:\n{}",
                        i, call.column, call.line, line_len_utf16, line_text, code
                    );
                }
            }
        }

        /// Property 1 extended: Detection is idempotent (same input produces same output)
        #[test]
        fn prop_library_call_detection_idempotent((code, _specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected1 = detect_library_calls(&tree, &code);
            let detected2 = detect_library_calls(&tree, &code);

            prop_assert_eq!(
                detected1.len(),
                detected2.len(),
                "Detection not idempotent: first call returned {} results, second returned {}",
                detected1.len(),
                detected2.len()
            );

            for (i, (d1, d2)) in detected1.iter().zip(detected2.iter()).enumerate() {
                prop_assert_eq!(
                    &d1.package, &d2.package,
                    "Detection not idempotent at index {}: package names differ",
                    i
                );
                prop_assert_eq!(
                    d1.line, d2.line,
                    "Detection not idempotent at index {}: lines differ",
                    i
                );
                prop_assert_eq!(
                    d1.column, d2.column,
                    "Detection not idempotent at index {}: columns differ",
                    i
                );
            }
        }

        /// Property 1 extended: library(), require(), and loadNamespace() are all detected
        #[test]
        fn prop_all_library_functions_detected(pkg in package_name()) {
            // Test all three function types
            let code_library = format!("library({})", pkg);
            let code_require = format!("require({})", pkg);
            let code_loadns = format!("loadNamespace({})", pkg);

            let tree_library = parse_r(&code_library);
            let tree_require = parse_r(&code_require);
            let tree_loadns = parse_r(&code_loadns);

            let detected_library = detect_library_calls(&tree_library, &code_library);
            let detected_require = detect_library_calls(&tree_require, &code_require);
            let detected_loadns = detect_library_calls(&tree_loadns, &code_loadns);

            // All should detect exactly one call
            prop_assert_eq!(detected_library.len(), 1, "library() not detected");
            prop_assert_eq!(detected_require.len(), 1, "require() not detected");
            prop_assert_eq!(detected_loadns.len(), 1, "loadNamespace() not detected");

            // All should extract the correct package name
            prop_assert_eq!(&detected_library[0].package, &pkg, "library() package name mismatch");
            prop_assert_eq!(&detected_require[0].package, &pkg, "require() package name mismatch");
            prop_assert_eq!(&detected_loadns[0].package, &pkg, "loadNamespace() package name mismatch");
        }

        /// Property 1 extended: Both quoted and unquoted package names are detected
        #[test]
        fn prop_quoted_and_unquoted_detected(pkg in package_name()) {
            let code_bare = format!("library({})", pkg);
            let code_double = format!("library(\"{}\")", pkg);
            let code_single = format!("library('{}')", pkg);

            let tree_bare = parse_r(&code_bare);
            let tree_double = parse_r(&code_double);
            let tree_single = parse_r(&code_single);

            let detected_bare = detect_library_calls(&tree_bare, &code_bare);
            let detected_double = detect_library_calls(&tree_double, &code_double);
            let detected_single = detect_library_calls(&tree_single, &code_single);

            // All should detect exactly one call
            prop_assert_eq!(detected_bare.len(), 1, "Bare identifier not detected");
            prop_assert_eq!(detected_double.len(), 1, "Double-quoted string not detected");
            prop_assert_eq!(detected_single.len(), 1, "Single-quoted string not detected");

            // All should extract the correct package name
            prop_assert_eq!(&detected_bare[0].package, &pkg, "Bare identifier package name mismatch");
            prop_assert_eq!(&detected_double[0].package, &pkg, "Double-quoted package name mismatch");
            prop_assert_eq!(&detected_single[0].package, &pkg, "Single-quoted package name mismatch");
        }

        /// Property 1 extended: Named package= argument is detected
        #[test]
        fn prop_named_package_argument_detected(pkg in package_name()) {
            let code_named = format!("library(package = {})", pkg);
            let code_named_quoted = format!("library(package = \"{}\")", pkg);

            let tree_named = parse_r(&code_named);
            let tree_named_quoted = parse_r(&code_named_quoted);

            let detected_named = detect_library_calls(&tree_named, &code_named);
            let detected_named_quoted = detect_library_calls(&tree_named_quoted, &code_named_quoted);

            // Both should detect exactly one call
            prop_assert_eq!(detected_named.len(), 1, "Named bare argument not detected");
            prop_assert_eq!(detected_named_quoted.len(), 1, "Named quoted argument not detected");

            // Both should extract the correct package name
            prop_assert_eq!(&detected_named[0].package, &pkg, "Named bare package name mismatch");
            prop_assert_eq!(&detected_named_quoted[0].package, &pkg, "Named quoted package name mismatch");
        }

        // ============================================================================
        // Feature: package-function-awareness, Property 2: Dynamic Package Name Exclusion
        // **Validates: Requirements 1.6, 1.7**
        //
        // For any R source file containing library calls with variable or expression
        // package names (including character.only = TRUE), the Library_Call_Detector
        // SHALL NOT include those calls in the detected results.
        // ============================================================================

        /// Property 2: Calls with character.only = TRUE are NOT detected
        #[test]
        fn prop_character_only_true_excluded(pkg in package_name()) {
            // Test character.only = TRUE (full form)
            let code_true = format!("library(\"{}\", character.only = TRUE)", pkg);
            let tree_true = parse_r(&code_true);
            let detected_true = detect_library_calls(&tree_true, &code_true);

            prop_assert_eq!(
                detected_true.len(),
                0,
                "library() with character.only = TRUE should NOT be detected. Code: {}",
                code_true
            );

            // Test character.only = T (short form)
            let code_t = format!("library(\"{}\", character.only = T)", pkg);
            let tree_t = parse_r(&code_t);
            let detected_t = detect_library_calls(&tree_t, &code_t);

            prop_assert_eq!(
                detected_t.len(),
                0,
                "library() with character.only = T should NOT be detected. Code: {}",
                code_t
            );
        }

        /// Property 2: require() with character.only = TRUE is NOT detected
        #[test]
        fn prop_require_character_only_true_excluded(pkg in package_name()) {
            // Test require with character.only = TRUE
            let code_true = format!("require(\"{}\", character.only = TRUE)", pkg);
            let tree_true = parse_r(&code_true);
            let detected_true = detect_library_calls(&tree_true, &code_true);

            prop_assert_eq!(
                detected_true.len(),
                0,
                "require() with character.only = TRUE should NOT be detected. Code: {}",
                code_true
            );

            // Test require with character.only = T
            let code_t = format!("require(\"{}\", character.only = T)", pkg);
            let tree_t = parse_r(&code_t);
            let detected_t = detect_library_calls(&tree_t, &code_t);

            prop_assert_eq!(
                detected_t.len(),
                0,
                "require() with character.only = T should NOT be detected. Code: {}",
                code_t
            );
        }

        /// Property 2: Calls with expression package names are NOT detected
        #[test]
        fn prop_expression_package_names_excluded(pkg in package_name()) {
            // Test paste0() expression
            let code_paste0 = format!("library(paste0(\"{}\", \"\"))", pkg);
            let tree_paste0 = parse_r(&code_paste0);
            let detected_paste0 = detect_library_calls(&tree_paste0, &code_paste0);

            prop_assert_eq!(
                detected_paste0.len(),
                0,
                "library() with paste0() expression should NOT be detected. Code: {}",
                code_paste0
            );

            // Test paste() expression
            let code_paste = format!("library(paste(\"{}\", sep = \"\"))", pkg);
            let tree_paste = parse_r(&code_paste);
            let detected_paste = detect_library_calls(&tree_paste, &code_paste);

            prop_assert_eq!(
                detected_paste.len(),
                0,
                "library() with paste() expression should NOT be detected. Code: {}",
                code_paste
            );

            // Test sprintf() expression
            let code_sprintf = format!("library(sprintf(\"%s\", \"{}\"))", pkg);
            let tree_sprintf = parse_r(&code_sprintf);
            let detected_sprintf = detect_library_calls(&tree_sprintf, &code_sprintf);

            prop_assert_eq!(
                detected_sprintf.len(),
                0,
                "library() with sprintf() expression should NOT be detected. Code: {}",
                code_sprintf
            );
        }

        /// Property 2: Calls with get() expression are NOT detected
        #[test]
        fn prop_get_expression_excluded(pkg in package_name()) {
            // Test get() expression
            let code_get = format!("library(get(\"{}\"))", pkg);
            let tree_get = parse_r(&code_get);
            let detected_get = detect_library_calls(&tree_get, &code_get);

            prop_assert_eq!(
                detected_get.len(),
                0,
                "library() with get() expression should NOT be detected. Code: {}",
                code_get
            );
        }

        /// Property 2: character.only = FALSE does NOT exclude the call
        #[test]
        fn prop_character_only_false_not_excluded(pkg in package_name()) {
            // Test character.only = FALSE (should still be detected)
            let code_false = format!("library(\"{}\", character.only = FALSE)", pkg);
            let tree_false = parse_r(&code_false);
            let detected_false = detect_library_calls(&tree_false, &code_false);

            prop_assert_eq!(
                detected_false.len(),
                1,
                "library() with character.only = FALSE SHOULD be detected. Code: {}",
                code_false
            );
            prop_assert_eq!(
                &detected_false[0].package,
                &pkg,
                "Package name mismatch for character.only = FALSE case"
            );

            // Test character.only = F (should still be detected)
            let code_f = format!("library(\"{}\", character.only = F)", pkg);
            let tree_f = parse_r(&code_f);
            let detected_f = detect_library_calls(&tree_f, &code_f);

            prop_assert_eq!(
                detected_f.len(),
                1,
                "library() with character.only = F SHOULD be detected. Code: {}",
                code_f
            );
            prop_assert_eq!(
                &detected_f[0].package,
                &pkg,
                "Package name mismatch for character.only = F case"
            );
        }
    }

    // ============================================================================
    // Property 2 Extended: Dynamic Package Exclusion with Mixed Code
    // ============================================================================

    /// Types of dynamic library calls that should be excluded
    #[derive(Debug, Clone)]
    enum DynamicCallType {
        /// character.only = TRUE
        CharacterOnlyTrue,
        /// character.only = T
        CharacterOnlyT,
        /// paste0() expression
        Paste0Expression,
        /// paste() expression
        PasteExpression,
        /// get() expression
        GetExpression,
        /// sprintf() expression
        SprintfExpression,
        /// c() expression (vector of packages)
        CExpression,
    }

    /// Creates a proptest strategy that yields one of the `DynamicCallType` variants representing
    /// dynamic/library-call patterns that should be treated as non-statically-determinable.
    ///
    /// # Returns
    ///
    /// A `Strategy` that produces a `DynamicCallType` chosen from:
    /// `CharacterOnlyTrue`, `CharacterOnlyT`, `Paste0Expression`, `PasteExpression`,
    /// `GetExpression`, `SprintfExpression`, and `CExpression`.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// // obtain the strategy and use it in a proptest
    /// let strat = crate::dynamic_call_type();
    ///
    /// proptest!(|(kind in strat)| {
    ///     // `kind` is a `DynamicCallType` variant
    ///     match kind {
    ///         crate::DynamicCallType::CharacterOnlyTrue => {},
    ///         crate::DynamicCallType::CharacterOnlyT => {},
    ///         _ => {},
    ///     }
    /// });
    /// ```
    fn dynamic_call_type() -> impl Strategy<Value = DynamicCallType> {
        prop_oneof![
            Just(DynamicCallType::CharacterOnlyTrue),
            Just(DynamicCallType::CharacterOnlyT),
            Just(DynamicCallType::Paste0Expression),
            Just(DynamicCallType::PasteExpression),
            Just(DynamicCallType::GetExpression),
            Just(DynamicCallType::SprintfExpression),
            Just(DynamicCallType::CExpression),
        ]
    }

    /// Generate R code representing a dynamic (non-statically-determinable) library/require/loadNamespace call.
    ///
    /// This returns an R expression string that uses a dynamic form of specifying the package (e.g., `character.only = TRUE`, `paste0(...)`, `get(...)`, `c(...)`, etc.), which should not be recognized as a statically determinable package by static analyzers.
    /// - `call_type`: selects which dynamic pattern to emit.
    /// - `pkg`: the package name used within the generated expression (may be split or wrapped depending on the pattern).
    /// - `func`: the function name to call (e.g., `"library"`, `"require"`, or `"loadNamespace"`).
    ///
    /// # Examples
    ///
    /// ```
    /// let s = generate_dynamic_library_call(&DynamicCallType::CharacterOnlyTrue, "dplyr", "library");
    /// assert!(s.contains("character.only"));
    /// let s2 = generate_dynamic_library_call(&DynamicCallType::Paste0Expression, "pkgname", "require");
    /// assert!(s2.contains("paste0"));
    /// ```
    fn generate_dynamic_library_call(call_type: &DynamicCallType, pkg: &str, func: &str) -> String {
        match call_type {
            DynamicCallType::CharacterOnlyTrue => {
                format!("{}(\"{}\", character.only = TRUE)", func, pkg)
            }
            DynamicCallType::CharacterOnlyT => {
                format!("{}(\"{}\", character.only = T)", func, pkg)
            }
            DynamicCallType::Paste0Expression => {
                // Split package name for paste0
                let mid = pkg.len() / 2;
                let (p1, p2) = pkg.split_at(mid);
                format!("{}(paste0(\"{}\", \"{}\"))", func, p1, p2)
            }
            DynamicCallType::PasteExpression => {
                format!("{}(paste(\"{}\", sep = \"\"))", func, pkg)
            }
            DynamicCallType::GetExpression => {
                format!("{}(get(\"{}\"))", func, pkg)
            }
            DynamicCallType::SprintfExpression => {
                format!("{}(sprintf(\"%s\", \"{}\"))", func, pkg)
            }
            DynamicCallType::CExpression => {
                // c() returns a vector, not a valid single package name
                format!("{}(c(\"{}\", \"other\"))", func, pkg)
            }
        }
    }

    /// A specification for a dynamic library call
    #[derive(Debug, Clone)]
    struct DynamicLibraryCallSpec {
        func: &'static str,
        package: String,
        call_type: DynamicCallType,
    }

    /// Creates a proptest strategy that generates `DynamicLibraryCallSpec` values.
    ///
    /// The produced spec combines a library-related function variant, a package name, and a dynamic call type.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// let strategy = dynamic_library_call_spec();
    /// proptest!(|(spec in strategy)| {
    ///     // `spec` is a generated DynamicLibraryCallSpec
    ///     let _pkg: String = spec.package.clone();
    /// });
    /// ```
    fn dynamic_library_call_spec() -> impl Strategy<Value = DynamicLibraryCallSpec> {
        (library_function(), package_name(), dynamic_call_type()).prop_map(
            |(func, package, call_type)| DynamicLibraryCallSpec {
                func,
                package,
                call_type,
            },
        )
    }

    /// Generate R source code containing dynamic library calls that should not be detected.
    ///
    /// The returned proptest Strategy produces tuples of `(code, specs)` where `code` is the generated
    /// R source as a `String` and `specs` is a `Vec<DynamicLibraryCallSpec>` describing each dynamic
    /// library-style call inserted into the code. The code includes 1–5 dynamic calls with optional
    /// filler lines between them.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// let strat = r_code_with_dynamic_library_calls();
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let (code, specs) = strat.new_tree(&mut runner).unwrap().current();
    /// assert!(!code.is_empty());
    /// assert!(!specs.is_empty());
    /// ```
    fn r_code_with_dynamic_library_calls()
    -> impl Strategy<Value = (String, Vec<DynamicLibraryCallSpec>)> {
        // Generate 1-5 dynamic library calls
        prop::collection::vec(dynamic_library_call_spec(), 1..=5)
            .prop_flat_map(|specs| {
                // Generate 0-2 filler lines between each call
                let num_fillers = specs.len() + 1;
                let filler_counts = prop::collection::vec(0..3usize, num_fillers);
                (Just(specs), filler_counts)
            })
            .prop_map(|(specs, filler_counts)| {
                let mut lines = Vec::new();

                // Add filler before first call
                for _ in 0..filler_counts[0] {
                    lines.push("x <- 1".to_string());
                }

                // Add dynamic library calls with fillers between them
                for (i, spec) in specs.iter().enumerate() {
                    lines.push(generate_dynamic_library_call(
                        &spec.call_type,
                        &spec.package,
                        spec.func,
                    ));

                    // Add filler after this call
                    if i + 1 < filler_counts.len() {
                        for _ in 0..filler_counts[i + 1] {
                            lines.push("y <- 2".to_string());
                        }
                    }
                }

                let code = lines.join("\n");
                (code, specs)
            })
    }

    /// Generate R source text containing an interleaved sequence of statically determinable
    /// and dynamic library-style calls, along with the specifications used to build them.
    ///
    /// The produced strategy yields a tuple containing:
    /// 1. A single String with the generated R code (multiple lines, interleaved static and dynamic calls).
    /// 2. A Vec of `LibraryCallSpec` describing the statically determinable calls included.
    /// 3. A Vec of `DynamicLibraryCallSpec` describing the dynamic calls included.
    ///
    /// # Examples
    ///
    /// ```
    /// // Obtain the proptest strategy and use it in property tests.
    /// let strat = r_code_with_mixed_library_calls();
    /// // `strat` is a `Strategy` that generates `(String, Vec<LibraryCallSpec>, Vec<DynamicLibraryCallSpec>)`.
    /// let _ = strat;
    /// ```
    fn r_code_with_mixed_library_calls()
    -> impl Strategy<Value = (String, Vec<LibraryCallSpec>, Vec<DynamicLibraryCallSpec>)> {
        (
            prop::collection::vec(library_call_spec(), 1..=3),
            prop::collection::vec(dynamic_library_call_spec(), 1..=3),
        )
            .prop_map(|(static_specs, dynamic_specs)| {
                let mut lines = Vec::new();

                // Interleave static and dynamic calls
                let max_len = static_specs.len().max(dynamic_specs.len());
                for i in 0..max_len {
                    if i < static_specs.len() {
                        lines.push(generate_library_call_code(&static_specs[i]));
                    }
                    if i < dynamic_specs.len() {
                        lines.push(generate_dynamic_library_call(
                            &dynamic_specs[i].call_type,
                            &dynamic_specs[i].package,
                            dynamic_specs[i].func,
                        ));
                    }
                }

                let code = lines.join("\n");
                (code, static_specs, dynamic_specs)
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // ============================================================================
        // Feature: package-function-awareness, Property 2: Dynamic Package Name Exclusion
        // **Validates: Requirements 1.6, 1.7**
        // ============================================================================

        /// Property 2: All dynamic library calls are excluded from detection
        #[test]
        fn prop_dynamic_package_exclusion((code, _specs) in r_code_with_dynamic_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // No dynamic calls should be detected
            prop_assert_eq!(
                detected.len(),
                0,
                "Dynamic library calls should NOT be detected. Found {} calls. Code:\n{}",
                detected.len(),
                code
            );
        }

        /// Property 2: Mixed code correctly detects only static calls
        #[test]
        fn prop_mixed_static_dynamic_detection((code, static_specs, _dynamic_specs) in r_code_with_mixed_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // Only static calls should be detected
            prop_assert_eq!(
                detected.len(),
                static_specs.len(),
                "Expected {} static library calls, but detected {}. Code:\n{}",
                static_specs.len(),
                detected.len(),
                code
            );

            // Verify detected packages match static specs
            let detected_packages: std::collections::HashSet<_> = detected.iter().map(|c| &c.package).collect();
            let expected_packages: std::collections::HashSet<_> = static_specs.iter().map(|s| &s.package).collect();

            prop_assert_eq!(
                detected_packages,
                expected_packages,
                "Detected packages don't match expected static packages. Code:\n{}",
                code
            );
        }

        /// Property 2: character.only with variable value is still excluded
        #[test]
        fn prop_character_only_with_variable_excluded(pkg in package_name()) {
            // When character.only is set to a variable (not TRUE/T/FALSE/F),
            // we can't statically determine if it's true, so we should still
            // detect the call (conservative approach - only exclude TRUE/T)
            let code = format!("library(\"{}\", character.only = my_var)", pkg);
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // This should be detected because character.only is not TRUE/T
            prop_assert_eq!(
                detected.len(),
                1,
                "library() with character.only = variable SHOULD be detected (conservative). Code: {}",
                code
            );
        }
    }
}
